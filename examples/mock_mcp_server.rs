//! Mock MCP server for integration testing.
//!
//! Exposes two tools: `echo` (returns its input as text) and `add` (adds two numbers).
//! Runs over stdio. Used by `tests/mcp_integration.rs`.

use rmcp::handler::server::ServerHandler;
use rmcp::model::*;
use rmcp::service::ServiceExt;

struct MockServer;

impl ServerHandler for MockServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability { list_changed: None }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_
    {
        let tools = vec![
            Tool::new(
                "echo",
                "Echo back the input message",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "The message to echo"
                        }
                    },
                    "required": ["message"]
                })
                .as_object()
                .cloned()
                .unwrap(),
            ),
            Tool::new(
                "add",
                "Add two numbers",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "a": { "type": "number", "description": "First number" },
                        "b": { "type": "number", "description": "Second number" }
                    },
                    "required": ["a", "b"]
                })
                .as_object()
                .cloned()
                .unwrap(),
            ),
        ];

        std::future::ready(Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        }))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_
    {
        let result = match request.name.as_ref() {
            "echo" => {
                let message = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("<no message>");
                CallToolResult::success(vec![Content::text(message.to_owned())])
            }
            "add" => {
                let a = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("a"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let b = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("b"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let sum = a + b;
                CallToolResult::success(vec![Content::text(sum.to_string())])
            }
            other => CallToolResult::error(vec![Content::text(format!("unknown tool: {other}"))]),
        };
        std::future::ready(Ok(result))
    }
}

#[tokio::main]
async fn main() {
    let service = MockServer
        .serve(rmcp::transport::stdio())
        .await
        .expect("failed to start mock MCP server");

    service.waiting().await.expect("server error");
}
