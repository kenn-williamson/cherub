//! Memory tool: policy-gated access to the enforced memory store.
//!
//! The agent uses this tool to store, recall, search, update, and forget memories.
//! Every operation passes through the enforcement pipeline; the tier required depends
//! on the scope and operation (see `config/default_policy.toml` for policy config).
//!
//! Provenance (session_id, turn_number) is injected by the tool from `ToolContext` —
//! the agent cannot forge it.

use std::sync::Arc;

use uuid::Uuid;

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;
use crate::storage::{
    MemoryCategory, MemoryFilter, MemoryScope, MemoryStore, MemoryUpdate, NewMemory, SourceType,
};
use crate::tools::ToolResult;

use super::ToolContext;

pub struct MemoryTool {
    store: Arc<dyn MemoryStore>,
}

impl MemoryTool {
    pub fn new(store: Arc<dyn MemoryStore>) -> Self {
        Self { store }
    }

    pub async fn execute(
        &self,
        params: &serde_json::Value,
        _token: CapabilityToken,
        ctx: &ToolContext,
    ) -> Result<ToolResult, CherubError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CherubError::InvalidInvocation("memory tool requires 'action'".to_owned())
            })?;

        match action {
            "store" => self.op_store(params, ctx).await,
            "recall" => self.op_recall(params, ctx).await,
            "search" => self.op_search(params, ctx).await,
            "update" => self.op_update(params).await,
            "forget" => self.op_forget(params).await,
            other => Err(CherubError::InvalidInvocation(format!(
                "unknown memory action: {other}"
            ))),
        }
    }

    async fn op_store(
        &self,
        params: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, CherubError> {
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CherubError::InvalidInvocation("store requires 'content'".to_owned()))?;

        let category_str = params
            .get("category")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CherubError::InvalidInvocation("store requires 'category'".to_owned())
            })?;

        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CherubError::InvalidInvocation("store requires 'path'".to_owned()))?;

        let scope = parse_scope(params)?;
        let category = category_str
            .parse::<MemoryCategory>()
            .map_err(|e| CherubError::InvalidInvocation(e.to_string()))?;

        let source_type = params
            .get("source_type")
            .and_then(|v| v.as_str())
            .unwrap_or("explicit")
            .parse::<SourceType>()
            .map_err(|e| CherubError::InvalidInvocation(e.to_string()))?;

        let confidence = params
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0) as f32;

        let structured = params.get("structured").cloned();

        let id = self
            .store
            .store(NewMemory {
                user_id: ctx.user_id.clone(),
                scope,
                category,
                path: path.to_owned(),
                content: content.to_owned(),
                structured,
                source_session_id: Some(ctx.session_id),
                source_turn_number: Some(ctx.turn_number),
                source_type,
                confidence,
            })
            .await?;

        Ok(ToolResult {
            output: format!("stored: {id}"),
        })
    }

    async fn op_recall(
        &self,
        params: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, CherubError> {
        let scope = if params.get("scope").is_some() {
            Some(parse_scope(params)?)
        } else {
            None
        };

        let category = params
            .get("category")
            .and_then(|v| v.as_str())
            .map(|s| {
                s.parse::<MemoryCategory>()
                    .map_err(|e| CherubError::InvalidInvocation(e.to_string()))
            })
            .transpose()?;

        let path_prefix = params
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());

        let limit = params.get("limit").and_then(|v| v.as_i64());

        let memories = self
            .store
            .recall(MemoryFilter {
                scope,
                category,
                path_prefix,
                user_id: Some(ctx.user_id.clone()),
                limit,
            })
            .await?;

        if memories.is_empty() {
            return Ok(ToolResult {
                output: "no memories found".to_owned(),
            });
        }

        // Touch each recalled memory's last_referenced_at (best-effort, non-fatal).
        for m in &memories {
            let _ = self.store.touch(m.id).await;
        }

        let output = memories
            .iter()
            .map(|m| format!("[{}] ({}) {}: {}", m.id, m.scope, m.path, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolResult { output })
    }

    async fn op_search(
        &self,
        params: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, CherubError> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CherubError::InvalidInvocation("search requires 'query'".to_owned()))?;

        let scope = if params.get("scope").is_some() {
            Some(parse_scope(params)?)
        } else {
            None
        };

        let limit = params.get("limit").and_then(|v| v.as_i64()).unwrap_or(5);

        let memories = self
            .store
            .search(query, scope, Some(&ctx.user_id), limit)
            .await?;

        if memories.is_empty() {
            return Ok(ToolResult {
                output: "no results".to_owned(),
            });
        }

        let output = memories
            .iter()
            .map(|m| format!("[{}] ({}) {}: {}", m.id, m.scope, m.path, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolResult { output })
    }

    async fn op_update(&self, params: &serde_json::Value) -> Result<ToolResult, CherubError> {
        let id_str = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CherubError::InvalidInvocation("update requires 'id'".to_owned()))?;

        let id = id_str
            .parse::<Uuid>()
            .map_err(|e| CherubError::InvalidInvocation(format!("invalid memory id: {e}")))?;

        let changes = MemoryUpdate {
            content: params
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned()),
            structured: params.get("structured").cloned(),
            confidence: params
                .get("confidence")
                .and_then(|v| v.as_f64())
                .map(|f| f as f32),
        };

        let new_id = self.store.update(id, changes).await?;
        Ok(ToolResult {
            output: format!("updated: {new_id} (supersedes {id})"),
        })
    }

    async fn op_forget(&self, params: &serde_json::Value) -> Result<ToolResult, CherubError> {
        let id_str = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CherubError::InvalidInvocation("forget requires 'id'".to_owned()))?;

        let id = id_str
            .parse::<Uuid>()
            .map_err(|e| CherubError::InvalidInvocation(format!("invalid memory id: {e}")))?;

        self.store.forget(id).await?;
        Ok(ToolResult {
            output: format!("forgotten: {id}"),
        })
    }
}

fn parse_scope(params: &serde_json::Value) -> Result<MemoryScope, CherubError> {
    params
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("user")
        .parse::<MemoryScope>()
        .map_err(|e| CherubError::InvalidInvocation(e.to_string()))
}
