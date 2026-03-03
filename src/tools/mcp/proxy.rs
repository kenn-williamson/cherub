//! MCP tool proxy: wraps a single tool from an MCP server.
//!
//! One `McpToolProxy` per discovered tool. Routes calls through the shared
//! `McpClient` (which is `Arc<Mutex<>>` because MCP IPC is sequential).

use std::ops::Deref;
use std::sync::Arc;

use rmcp::model::RawContent;
use tokio::sync::Mutex;

use super::client::McpClient;
use crate::error::CherubError;
use crate::providers::ToolDefinition;
use crate::tools::ToolResult;

/// Proxy for a single tool discovered from an MCP server.
pub struct McpToolProxy {
    /// Server name from config (e.g., "google-workspace").
    pub server_name: String,
    /// Tool name from the MCP server (e.g., "list_events").
    pub tool_name: String,
    /// Composite name: "{server}__{tool}" — what the LLM sees and calls.
    pub composite_name: String,
    /// Tool description from the MCP server.
    pub description: String,
    /// Input schema from the MCP server (JSON Schema object).
    pub input_schema: serde_json::Value,
    /// Shared client for this server.
    pub client: Arc<Mutex<McpClient>>,
}

impl McpToolProxy {
    /// Execute this tool with the given params.
    ///
    /// Strips `__mcp_server` and `__mcp_tool` internal keys before forwarding
    /// to the MCP server.
    // Called from `ToolImpl::Mcp` dispatch which requires and consumes a
    // `CapabilityToken`. Direct calls bypass enforcement — callers are
    // responsible for ensuring the capability gate has already been passed.
    pub async fn execute(&self, params: &serde_json::Value) -> Result<ToolResult, CherubError> {
        // Strip internal enforcement keys before forwarding.
        let arguments = match params.as_object() {
            Some(map) => {
                let cleaned: serde_json::Map<String, serde_json::Value> = map
                    .iter()
                    .filter(|(k, _)| !k.starts_with("__mcp_"))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if cleaned.is_empty() {
                    None
                } else {
                    Some(cleaned)
                }
            }
            None => None,
        };

        let client = self.client.lock().await;
        let result = client.call_tool(&self.tool_name, arguments).await?;

        // Extract text content from the MCP result.
        // Content = Annotated<RawContent>, which derefs to RawContent.
        let output = result
            .content
            .iter()
            .filter_map(|c| match c.deref() {
                RawContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let is_error = result.is_error.unwrap_or(false);
        if is_error {
            Err(CherubError::Mcp(format!(
                "server '{}', tool '{}': {output}",
                self.server_name, self.tool_name
            )))
        } else {
            Ok(ToolResult { output })
        }
    }

    /// Build a `ToolDefinition` for the LLM.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.composite_name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}
