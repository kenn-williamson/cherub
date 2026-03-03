//! MCP client: wraps a single MCP server process.
//!
//! Owns the `rmcp::service::RunningService`. Handles tool discovery via
//! `tools/list` and tool calls via `call_tool`. Wrapped in `Arc<Mutex<>>`
//! because IPC is inherently sequential (same pattern as `ContainerTool`).

use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;

use crate::error::CherubError;

/// A running MCP server process.
///
/// The `S` type parameter is the handler type used during initialization.
/// `()` is the standard client handler — no custom notifications or sampling.
pub struct McpClient {
    service: RunningService<RoleClient, ()>,
    server_name: String,
}

impl McpClient {
    /// Wrap a running rmcp service.
    pub fn new(service: RunningService<RoleClient, ()>, server_name: &str) -> Self {
        Self {
            service,
            server_name: server_name.to_owned(),
        }
    }

    /// Discover all available tools from the server (handles pagination).
    pub async fn list_all_tools(&self) -> Result<Vec<rmcp::model::Tool>, CherubError> {
        self.service.peer().list_all_tools().await.map_err(|e| {
            CherubError::Mcp(format!(
                "server '{}': failed to list tools: {e}",
                self.server_name
            ))
        })
    }

    /// Call a tool on the server.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, CherubError> {
        self.service
            .peer()
            .call_tool(CallToolRequestParams {
                meta: None,
                name: tool_name.to_owned().into(),
                arguments,
                task: None,
            })
            .await
            .map_err(|e| {
                CherubError::Mcp(format!(
                    "server '{}', tool '{}': call failed: {e}",
                    self.server_name, tool_name
                ))
            })
    }

    /// Shut down the server process gracefully.
    pub fn shutdown(self) {
        self.service.cancellation_token().cancel();
    }
}
