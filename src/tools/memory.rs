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
    Memory, MemoryCategory, MemoryFilter, MemoryScope, MemoryStore, MemoryUpdate, NewMemory,
    SourceType,
};
use crate::tools::ToolResult;

use super::ToolContext;

const CONTRADICTION_SEARCH_LIMIT: i64 = 5;

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

    async fn check_contradictions(
        &self,
        content: &str,
        scope: MemoryScope,
        user_id: &str,
    ) -> Vec<Memory> {
        match self
            .store
            .search(
                content,
                Some(scope),
                Some(user_id),
                CONTRADICTION_SEARCH_LIMIT,
            )
            .await
        {
            Ok(memories) => memories,
            Err(e) => {
                tracing::warn!(error = %e, "contradiction check search failed, allowing write");
                Vec::new()
            }
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

        let confirmed = params
            .get("confirmed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Pre-write contradiction check: find similar memories before storing.
        if !confirmed {
            let similar = self
                .check_contradictions(content, scope, &ctx.user_id)
                .await;
            if !similar.is_empty() {
                let mut msg = "Similar memories exist — resolve before storing:\n".to_owned();
                for m in &similar {
                    msg.push_str(&format!(
                        "- [{}] ({}/{}) \"{}\" [confidence: {:.2}, stored: {}]\n",
                        m.id,
                        m.scope,
                        m.path,
                        m.content,
                        m.confidence,
                        m.created_at.format("%Y-%m-%d"),
                    ));
                }
                msg.push_str(
                    "\nOptions:\n\
                     1. Update or forget the conflicting memory, then retry this store\n\
                     2. Add confirmed=true to store alongside the existing memory",
                );
                return Ok(ToolResult::text(msg));
            }
        }

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

        // After a confirmed write, surface similar memories as FYI.
        if confirmed {
            let similar = self
                .check_contradictions(content, scope, &ctx.user_id)
                .await;
            if !similar.is_empty() {
                let mut msg = format!("stored: {id}\n\n[Note: similar memories exist]\n");
                for m in &similar {
                    msg.push_str(&format!(
                        "- [{}] ({}/{}) \"{}\" [confidence: {:.2}, stored: {}]\n",
                        m.id,
                        m.scope,
                        m.path,
                        m.content,
                        m.confidence,
                        m.created_at.format("%Y-%m-%d"),
                    ));
                }
                msg.push_str("\nConsider updating or forgetting outdated memories.");
                return Ok(ToolResult::text(msg));
            }
        }

        Ok(ToolResult::text(format!("stored: {id}")))
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
            return Ok(ToolResult::text("no memories found".to_owned()));
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

        Ok(ToolResult::text(output))
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
            return Ok(ToolResult::text("no results".to_owned()));
        }

        let output = memories
            .iter()
            .map(|m| format!("[{}] ({}) {}: {}", m.id, m.scope, m.path, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolResult::text(output))
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
        Ok(ToolResult::text(format!(
            "updated: {new_id} (supersedes {id})"
        )))
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
        Ok(ToolResult::text(format!("forgotten: {id}")))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use chrono::Utc;
    use serde_json::json;
    use uuid::{NoContext, Timestamp, Uuid};

    fn new_uuid() -> Uuid {
        Uuid::new_v7(Timestamp::now(NoContext))
    }

    fn test_ctx(user_id: &str) -> ToolContext {
        ToolContext {
            user_id: user_id.to_owned(),
            session_id: new_uuid(),
            turn_number: 1,
        }
    }

    fn make_memory(content: &str, user_id: &str, scope: MemoryScope, path: &str) -> Memory {
        Memory {
            id: new_uuid(),
            user_id: user_id.to_owned(),
            scope,
            category: MemoryCategory::Preference,
            path: path.to_owned(),
            content: content.to_owned(),
            structured: None,
            source_session_id: None,
            source_turn_number: None,
            source_type: SourceType::Explicit,
            confidence: 1.0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_referenced_at: None,
            superseded_by: None,
        }
    }

    // ─── In-memory MemoryStore for unit tests ────────────────────────────────

    #[derive(Default)]
    struct InMemoryStore {
        memories: Mutex<Vec<Memory>>,
    }

    impl InMemoryStore {
        fn new_arc() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn add(&self, m: Memory) {
            self.memories.lock().unwrap().push(m);
        }
    }

    #[async_trait]
    impl MemoryStore for InMemoryStore {
        async fn store(&self, memory: NewMemory) -> Result<Uuid, CherubError> {
            let id = new_uuid();
            let row = Memory {
                id,
                user_id: memory.user_id,
                scope: memory.scope,
                category: memory.category,
                path: memory.path,
                content: memory.content,
                structured: memory.structured,
                source_session_id: memory.source_session_id,
                source_turn_number: memory.source_turn_number,
                source_type: memory.source_type,
                confidence: memory.confidence,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                last_referenced_at: None,
                superseded_by: None,
            };
            self.memories.lock().unwrap().push(row);
            Ok(id)
        }

        async fn recall(&self, filter: MemoryFilter) -> Result<Vec<Memory>, CherubError> {
            let memories = self.memories.lock().unwrap();
            let results = memories
                .iter()
                .filter(|m| filter.user_id.as_deref().is_none_or(|uid| m.user_id == uid))
                .filter(|m| filter.scope.is_none_or(|s| m.scope == s))
                .filter(|m| filter.category.is_none_or(|c| m.category == c))
                .cloned()
                .collect();
            Ok(results)
        }

        async fn search(
            &self,
            query: &str,
            scope: Option<MemoryScope>,
            user_id: Option<&str>,
            limit: i64,
        ) -> Result<Vec<Memory>, CherubError> {
            let memories = self.memories.lock().unwrap();
            let query_lower = query.to_lowercase();
            let query_words: Vec<&str> = query_lower
                .split_whitespace()
                .filter(|w| w.len() >= 4)
                .collect();
            let results = memories
                .iter()
                .filter(|m| user_id.is_none_or(|uid| m.user_id == uid))
                .filter(|m| scope.is_none_or(|s| m.scope == s))
                .filter(|m| {
                    let content_lower = m.content.to_lowercase();
                    query_words.iter().any(|w| content_lower.contains(*w))
                })
                .take(limit as usize)
                .cloned()
                .collect();
            Ok(results)
        }

        async fn update(&self, id: Uuid, _changes: MemoryUpdate) -> Result<Uuid, CherubError> {
            Ok(id)
        }

        async fn forget(&self, _id: Uuid) -> Result<(), CherubError> {
            Ok(())
        }

        async fn touch(&self, _id: Uuid) -> Result<(), CherubError> {
            Ok(())
        }
    }

    // ─── FailingSearchStore: search always errors ────────────────────────────

    struct FailingSearchStore;

    #[async_trait]
    impl MemoryStore for FailingSearchStore {
        async fn store(&self, _memory: NewMemory) -> Result<Uuid, CherubError> {
            Ok(new_uuid())
        }
        async fn recall(&self, _filter: MemoryFilter) -> Result<Vec<Memory>, CherubError> {
            Ok(Vec::new())
        }
        async fn search(
            &self,
            _query: &str,
            _scope: Option<MemoryScope>,
            _user_id: Option<&str>,
            _limit: i64,
        ) -> Result<Vec<Memory>, CherubError> {
            Err(CherubError::ToolExecution("search unavailable".to_owned()))
        }
        async fn update(&self, id: Uuid, _changes: MemoryUpdate) -> Result<Uuid, CherubError> {
            Ok(id)
        }
        async fn forget(&self, _id: Uuid) -> Result<(), CherubError> {
            Ok(())
        }
        async fn touch(&self, _id: Uuid) -> Result<(), CherubError> {
            Ok(())
        }
    }

    // ─── Tests ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn store_blocked_when_similar_memory_exists() {
        let store = InMemoryStore::new_arc();
        store.add(make_memory(
            "User prefers Italian food",
            "user1",
            MemoryScope::User,
            "preferences/food",
        ));

        let tool = MemoryTool::new(store as Arc<dyn MemoryStore>);
        let ctx = test_ctx("user1");
        let params = json!({
            "action": "store",
            "content": "User prefers Thai food",
            "category": "preference",
            "path": "preferences/food",
        });

        let result = tool.op_store(&params, &ctx).await.unwrap();
        assert!(
            result.output.contains("Similar memories exist"),
            "should block: {}",
            result.output,
        );
        assert!(result.output.contains("Italian food"));
        assert!(result.output.contains("confirmed=true"));
    }

    #[tokio::test]
    async fn store_succeeds_when_no_similar() {
        let store = InMemoryStore::new_arc();
        store.add(make_memory(
            "Loves Italian cooking",
            "user1",
            MemoryScope::User,
            "preferences/food",
        ));

        let tool = MemoryTool::new(store as Arc<dyn MemoryStore>);
        let ctx = test_ctx("user1");
        let params = json!({
            "action": "store",
            "content": "Lives in Tokyo",
            "category": "fact",
            "path": "facts/location",
        });

        let result = tool.op_store(&params, &ctx).await.unwrap();
        assert!(
            result.output.starts_with("stored:"),
            "should succeed: {}",
            result.output,
        );
    }

    #[tokio::test]
    async fn store_succeeds_with_confirmed_true() {
        let store = InMemoryStore::new_arc();
        store.add(make_memory(
            "User prefers Italian food",
            "user1",
            MemoryScope::User,
            "preferences/food",
        ));

        let tool = MemoryTool::new(store as Arc<dyn MemoryStore>);
        let ctx = test_ctx("user1");
        let params = json!({
            "action": "store",
            "content": "User prefers Thai food",
            "category": "preference",
            "path": "preferences/food",
            "confirmed": true,
        });

        let result = tool.op_store(&params, &ctx).await.unwrap();
        assert!(
            result.output.contains("stored:"),
            "confirmed write should succeed: {}",
            result.output,
        );
    }

    #[tokio::test]
    async fn confirmed_store_includes_similar_note() {
        let store = InMemoryStore::new_arc();
        store.add(make_memory(
            "User prefers Italian food",
            "user1",
            MemoryScope::User,
            "preferences/food",
        ));

        let tool = MemoryTool::new(store as Arc<dyn MemoryStore>);
        let ctx = test_ctx("user1");
        let params = json!({
            "action": "store",
            "content": "User prefers Thai food",
            "category": "preference",
            "path": "preferences/food",
            "confirmed": true,
        });

        let result = tool.op_store(&params, &ctx).await.unwrap();
        assert!(result.output.contains("stored:"));
        assert!(
            result.output.contains("[Note: similar memories exist]"),
            "should include FYI note: {}",
            result.output,
        );
        assert!(result.output.contains("Italian food"));
        assert!(result.output.contains("Consider updating or forgetting"));
    }

    #[tokio::test]
    async fn contradiction_check_search_failure_allows_write() {
        let store: Arc<dyn MemoryStore> = Arc::new(FailingSearchStore);
        let tool = MemoryTool::new(store);
        let ctx = test_ctx("user1");
        let params = json!({
            "action": "store",
            "content": "User prefers Italian food",
            "category": "preference",
            "path": "preferences/food",
        });

        let result = tool.op_store(&params, &ctx).await.unwrap();
        assert!(
            result.output.starts_with("stored:"),
            "search failure should not block write: {}",
            result.output,
        );
    }

    #[tokio::test]
    async fn contradiction_check_respects_user_isolation() {
        let store = InMemoryStore::new_arc();
        // User A has a memory about food
        store.add(make_memory(
            "User prefers Italian food",
            "user_a",
            MemoryScope::User,
            "preferences/food",
        ));

        let tool = MemoryTool::new(store as Arc<dyn MemoryStore>);
        // User B stores a similar memory — should NOT be blocked by user A's memory
        let ctx = test_ctx("user_b");
        let params = json!({
            "action": "store",
            "content": "User prefers Thai food",
            "category": "preference",
            "path": "preferences/food",
        });

        let result = tool.op_store(&params, &ctx).await.unwrap();
        assert!(
            result.output.starts_with("stored:"),
            "user B should not be blocked by user A's memories: {}",
            result.output,
        );
    }
}
