pub mod bash;
#[cfg(feature = "container")]
pub mod container;
#[cfg(feature = "container")]
pub mod container_bash;
#[cfg(feature = "browser")]
pub mod container_browser;
#[cfg(feature = "credentials")]
pub mod credential_broker;
#[cfg(feature = "container")]
pub mod dev_environment;
pub mod file;
#[cfg(feature = "credentials")]
pub mod http;
#[cfg(feature = "credentials")]
pub(crate) mod leak_detector;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "memory")]
pub mod memory;
pub(crate) mod path;
pub mod sub_agent;
#[cfg(feature = "wasm")]
pub mod wasm;

use std::marker::PhantomData;
#[cfg(feature = "container")]
use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;
use crate::providers::ToolDefinition;

use bash::BashTool;
#[cfg(feature = "container")]
use container::ContainerTool;
#[cfg(feature = "container")]
use dev_environment::DevEnvironmentTool;
use file::FileTool;
#[cfg(feature = "credentials")]
use http::HttpTool;
#[cfg(feature = "mcp")]
use mcp::proxy::McpToolProxy;
#[cfg(feature = "memory")]
use memory::MemoryTool;
use sub_agent::SubAgentTool;
#[cfg(feature = "wasm")]
use wasm::WasmTool;

/// Typestate: tool invocation parsed from model output, not yet evaluated.
pub struct Proposed;

/// Typestate: enforcement layer has evaluated this invocation.
pub struct Evaluated;

/// A tool invocation progressing through the enforcement pipeline.
///
/// `ToolInvocation<Proposed>` → enforcement evaluates → `ToolInvocation<Evaluated>`
///
/// `execute()` only exists on `Evaluated` — the compiler rejects calls on `Proposed`.
pub struct ToolInvocation<State> {
    pub(crate) tool: String,
    pub(crate) action: String,
    pub(crate) params: serde_json::Value,
    _state: PhantomData<State>,
}

impl ToolInvocation<Proposed> {
    pub fn new(tool: &str, action: &str, params: serde_json::Value) -> Self {
        Self {
            tool: tool.to_owned(),
            action: action.to_owned(),
            params,
            _state: PhantomData,
        }
    }

    /// Transition to Evaluated state. Only callable within the crate (by enforcement).
    pub(crate) fn transition(self) -> ToolInvocation<Evaluated> {
        ToolInvocation {
            tool: self.tool,
            action: self.action,
            params: self.params,
            _state: PhantomData,
        }
    }
}

/// Per-turn session context passed to tool implementations for provenance tracking.
///
/// Injected by `AgentLoop::run_turn()`. Tools that don't need it (e.g. bash) ignore it.
pub struct ToolContext {
    pub user_id: String,
    pub session_id: Uuid,
    pub turn_number: i32,
}

impl ToolInvocation<Evaluated> {
    /// Execute the tool invocation via the registry. Requires a `CapabilityToken` (consumed on use).
    pub async fn execute(
        self,
        token: CapabilityToken,
        registry: &ToolRegistry,
        ctx: &ToolContext,
    ) -> Result<ToolResult, CherubError> {
        let tool = registry.find(&self.tool).ok_or_else(|| {
            CherubError::InvalidInvocation(format!("unknown tool: {}", self.tool))
        })?;
        tool.execute(&self.params, token, ctx).await
    }
}

/// An image returned by a tool (e.g. browser screenshot).
#[derive(Debug, Clone)]
pub struct ToolImage {
    /// MIME type (e.g. `"image/png"`).
    pub media_type: String,
    /// Base64-encoded image data.
    pub data: String,
}

#[derive(Debug)]
pub struct ToolResult {
    pub output: String,
    /// Images returned alongside text (e.g. browser screenshots).
    /// Empty for most tools — only the browser tool populates this.
    pub images: Vec<ToolImage>,
    /// Sub-agent cost data: (model_name, usage). Present only for SubAgent tools.
    pub sub_agent_usage: Option<(String, crate::providers::ApiUsage)>,
}

impl ToolResult {
    pub fn text(output: String) -> Self {
        Self {
            output,
            images: vec![],
            sub_agent_usage: None,
        }
    }
}

/// Enum dispatch for tool implementations. Known variants at compile time.
pub(crate) enum ToolImpl {
    Bash(BashTool),
    File(FileTool),
    #[cfg(feature = "memory")]
    Memory(MemoryTool),
    #[cfg(feature = "credentials")]
    Http(HttpTool),
    #[cfg(feature = "wasm")]
    Wasm(WasmTool),
    #[cfg(feature = "container")]
    Container(Arc<ContainerTool>),
    #[cfg(feature = "container")]
    DevEnvironment(DevEnvironmentTool),
    #[cfg(feature = "mcp")]
    Mcp(McpToolProxy),
    SubAgent(SubAgentTool),
}

impl ToolImpl {
    fn name(&self) -> &str {
        match self {
            Self::Bash(_) => "bash",
            Self::File(_) => "file",
            #[cfg(feature = "memory")]
            Self::Memory(_) => "memory",
            #[cfg(feature = "credentials")]
            Self::Http(_) => "http",
            #[cfg(feature = "wasm")]
            Self::Wasm(t) => &t.module.name,
            #[cfg(feature = "container")]
            Self::Container(t) => &t.metadata.name,
            #[cfg(feature = "container")]
            Self::DevEnvironment(_) => "dev_environment",
            #[cfg(feature = "mcp")]
            Self::Mcp(t) => &t.composite_name,
            Self::SubAgent(t) => &t.name,
        }
    }

    async fn execute(
        &self,
        params: &serde_json::Value,
        token: CapabilityToken,
        // Prefixed with _ to suppress warning when compiled without --features memory/credentials.
        // Memory arms use it for provenance; http uses it for user_id; bash ignores it.
        _ctx: &ToolContext,
    ) -> Result<ToolResult, CherubError> {
        match self {
            Self::Bash(tool) => tool.execute(params, token).await,
            Self::File(tool) => tool.execute(params, token).await,
            #[cfg(feature = "memory")]
            Self::Memory(tool) => tool.execute(params, token, _ctx).await,
            #[cfg(feature = "credentials")]
            Self::Http(tool) => tool.execute(params, token, _ctx).await,
            #[cfg(feature = "wasm")]
            Self::Wasm(tool) => tool.execute(params, token, &_ctx.user_id).await,
            #[cfg(feature = "container")]
            Self::Container(tool) => tool.execute(params, token, _ctx).await,
            #[cfg(feature = "container")]
            Self::DevEnvironment(tool) => tool.execute(params, token).await,
            #[cfg(feature = "mcp")]
            Self::Mcp(tool) => {
                let _ = token; // Consume the capability token.
                tool.execute(params).await
            }
            Self::SubAgent(tool) => {
                let _ = token; // Consume the capability token.
                // Box::pin to break recursive async cycle:
                // ToolImpl::execute → SubAgentTool::execute → evaluated.execute → ToolImpl::execute
                Box::pin(tool.execute(params, _ctx)).await
            }
        }
    }

    fn definition(&self) -> ToolDefinition {
        match self {
            Self::Bash(_) => ToolDefinition {
                name: "bash".to_owned(),
                description: "Execute a bash command. The command is passed to `bash -c`."
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The bash command to execute"
                        }
                    },
                    "required": ["command"]
                }),
            },
            Self::File(_) => ToolDefinition {
                name: "file".to_owned(),
                description: "Read, write, edit, search, and find files in the workspace. \
                    All paths are relative to the workspace root. \
                    Use this instead of bash for file operations."
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["read", "write", "edit", "glob", "grep"],
                            "description": "Operation to perform"
                        },
                        "path": {
                            "type": "string",
                            "description": "Relative file path (required for read/write/edit; optional base dir for glob/grep)"
                        },
                        "content": {
                            "type": "string",
                            "description": "File content to write (for write action; creates parent dirs)"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Start line (1-indexed, for read)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max lines to return (for read)"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "Text to find and replace (for edit)"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Replacement text (for edit)"
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Replace all occurrences (for edit, default false)"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern (for glob) or regex pattern (for grep)"
                        },
                        "include": {
                            "type": "string",
                            "description": "File name glob filter (for grep, e.g. '*.rs')"
                        },
                        "context": {
                            "type": "integer",
                            "description": "Context lines around matches (for grep, default 0)"
                        }
                    },
                    "required": ["action"]
                }),
            },
            #[cfg(feature = "memory")]
            Self::Memory(_) => ToolDefinition {
                name: "memory".to_owned(),
                description: "Store, recall, search, update, or forget memories across sessions. \
                    All operations are policy-enforced."
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["store", "recall", "search", "update", "forget"],
                            "description": "Operation to perform"
                        },
                        "content": {
                            "type": "string",
                            "description": "Natural language content (required for store)"
                        },
                        "category": {
                            "type": "string",
                            "enum": ["preference", "fact", "instruction", "identity", "observation"],
                            "description": "Category of memory (required for store)"
                        },
                        "path": {
                            "type": "string",
                            "description": "Hierarchical path, e.g. 'preferences/food' (required for store; optional prefix filter for recall)"
                        },
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "user", "working"],
                            "description": "Memory scope (default: user)"
                        },
                        "query": {
                            "type": "string",
                            "description": "Full-text search query (required for search)"
                        },
                        "id": {
                            "type": "string",
                            "description": "Memory UUID (required for update, forget)"
                        },
                        "source_type": {
                            "type": "string",
                            "enum": ["explicit", "confirmed", "inferred"],
                            "description": "How the memory was established (default: explicit)"
                        },
                        "confidence": {
                            "type": "number",
                            "minimum": 0.0,
                            "maximum": 1.0,
                            "description": "Confidence score 0.0–1.0 (default: 1.0)"
                        },
                        "structured": {
                            "type": "object",
                            "description": "Optional machine-queryable structured data"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results to return for recall/search (default: 10/5)"
                        },
                        "confirmed": {
                            "type": "boolean",
                            "description": "Set to true to store even when similar memories exist (bypass contradiction check)"
                        }
                    },
                    "required": ["action"]
                }),
            },
            #[cfg(feature = "credentials")]
            Self::Http(_) => http::http_tool_definition(),
            #[cfg(feature = "wasm")]
            Self::Wasm(t) => {
                let m = &t.module;
                ToolDefinition {
                    name: m.name.clone(),
                    description: m.description.clone(),
                    input_schema: m.schema.clone(),
                }
            }
            #[cfg(feature = "container")]
            Self::Container(t) => {
                let m = &t.metadata;
                ToolDefinition {
                    name: m.name.clone(),
                    description: m.description.clone(),
                    input_schema: m.schema.clone(),
                }
            }
            #[cfg(feature = "container")]
            Self::DevEnvironment(_) => dev_environment::tool_definition(),
            #[cfg(feature = "mcp")]
            Self::Mcp(t) => t.definition(),
            Self::SubAgent(t) => t.definition(),
        }
    }
}

/// Registry of available tools. Provides lookup and schema definitions.
pub struct ToolRegistry {
    tools: Vec<ToolImpl>,
}

/// Returns the workspace root directory.
///
/// Uses `CHERUB_WORKSPACE` if set, otherwise falls back to the current
/// working directory. The workspace root is the containment boundary for
/// the file tool — all relative paths are resolved against it and must
/// stay inside it.
fn workspace_root() -> std::path::PathBuf {
    std::env::var("CHERUB_WORKSPACE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
}

impl ToolRegistry {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            tools: vec![
                ToolImpl::Bash(BashTool::new()),
                ToolImpl::File(FileTool::new(workspace_root())),
            ],
        }
    }

    /// Create a registry with no built-in tools.
    ///
    /// Used when bash is replaced by a container-sandboxed equivalent
    /// (registered later via `with_container()`).
    pub fn new_without_bash() -> Self {
        Self {
            tools: vec![ToolImpl::File(FileTool::new(workspace_root()))],
        }
    }

    /// Create a registry with the memory tool attached.
    #[cfg(feature = "memory")]
    pub fn with_memory(store: std::sync::Arc<dyn crate::storage::MemoryStore>) -> Self {
        Self {
            tools: vec![
                ToolImpl::Bash(BashTool::new()),
                ToolImpl::File(FileTool::new(workspace_root())),
                ToolImpl::Memory(MemoryTool::new(store)),
            ],
        }
    }

    /// Create a registry with only the memory tool (no built-in bash).
    ///
    /// Used when bash is replaced by a container-sandboxed equivalent.
    #[cfg(feature = "memory")]
    pub fn with_memory_no_bash(store: std::sync::Arc<dyn crate::storage::MemoryStore>) -> Self {
        Self {
            tools: vec![
                ToolImpl::File(FileTool::new(workspace_root())),
                ToolImpl::Memory(MemoryTool::new(store)),
            ],
        }
    }

    /// Add the HTTP tool to an existing registry (consumes and returns self).
    ///
    /// The `CredentialBroker` is shared between the tool and the registry.
    /// Call after `new()` or `with_memory()`.
    #[cfg(feature = "credentials")]
    pub fn with_credentials(
        mut self,
        broker: std::sync::Arc<credential_broker::CredentialBroker>,
    ) -> Self {
        self.tools.push(ToolImpl::Http(HttpTool::new(broker)));
        self
    }

    /// Append WASM tools to the registry (builder pattern).
    ///
    /// Call after `new()`, `with_memory()`, or `with_credentials()`.
    #[cfg(feature = "wasm")]
    pub fn with_wasm(mut self, tools: Vec<WasmTool>) -> Self {
        self.tools.extend(tools.into_iter().map(ToolImpl::Wasm));
        self
    }

    /// Append container tools to the registry (builder pattern).
    ///
    /// Call after `new()`, `with_memory()`, `with_credentials()`, or `with_wasm()`.
    #[cfg(feature = "container")]
    pub fn with_container(mut self, tools: Vec<Arc<ContainerTool>>) -> Self {
        self.tools
            .extend(tools.into_iter().map(ToolImpl::Container));
        self
    }

    /// Add the dev_environment tool to the registry (builder pattern).
    #[cfg(feature = "container")]
    pub fn with_dev_environment(mut self, tool: DevEnvironmentTool) -> Self {
        self.tools.push(ToolImpl::DevEnvironment(tool));
        self
    }

    /// Append MCP tools to the registry (builder pattern).
    ///
    /// Call after other `with_*` methods, before building the `AgentLoop`.
    #[cfg(feature = "mcp")]
    pub fn with_mcp(mut self, tools: Vec<McpToolProxy>) -> Self {
        self.tools.extend(tools.into_iter().map(ToolImpl::Mcp));
        self
    }

    /// Append sub-agent tools to the registry (builder pattern).
    ///
    /// Call after other `with_*` methods, before building the `AgentLoop`.
    pub fn with_sub_agents(mut self, tools: Vec<SubAgentTool>) -> Self {
        self.tools.extend(tools.into_iter().map(ToolImpl::SubAgent));
        self
    }

    /// Create a registry with only the named base tools.
    ///
    /// Used to build sub-agent registries that contain a subset of tools.
    /// Only `bash` and `file` are supported in M13d scope.
    pub fn for_sub_agent(tool_names: &[String]) -> Self {
        let mut tools: Vec<ToolImpl> = Vec::new();
        for name in tool_names {
            match name.as_str() {
                "bash" => tools.push(ToolImpl::Bash(BashTool::new())),
                "file" => tools.push(ToolImpl::File(FileTool::new(workspace_root()))),
                other => {
                    tracing::warn!(tool = %other, "unknown tool in sub-agent config, skipping");
                }
            }
        }
        Self { tools }
    }

    pub(crate) fn find(&self, name: &str) -> Option<&ToolImpl> {
        self.tools.iter().find(|t| t.name() == name)
    }

    /// Return the enforcement policy name for a tool.
    ///
    /// For MCP tools, returns the server name (e.g., "google-workspace").
    /// For all other tools, returns the tool name as-is.
    pub fn enforcement_name<'a>(&'a self, tool_name: &'a str) -> &'a str {
        match self.find(tool_name) {
            #[cfg(feature = "mcp")]
            Some(ToolImpl::Mcp(t)) => &t.server_name,
            _ => tool_name,
        }
    }

    /// Enrich params with enforcement metadata.
    ///
    /// - MCP tools: injects `__mcp_server` and `__mcp_tool` keys for
    ///   `McpStructured` extraction.
    /// - Sub-agent tools: injects `"action": "invoke"` for `Structured` extraction.
    /// - All other tools: returns params unchanged.
    pub fn enrich_params(&self, tool_name: &str, params: &serde_json::Value) -> serde_json::Value {
        match self.find(tool_name) {
            #[cfg(feature = "mcp")]
            Some(ToolImpl::Mcp(t)) => {
                let mut enriched = params.clone();
                if let Some(obj) = enriched.as_object_mut() {
                    // Always overwrite — prevents adversarial injection.
                    obj.insert(
                        "__mcp_server".to_owned(),
                        serde_json::Value::String(t.server_name.clone()),
                    );
                    obj.insert(
                        "__mcp_tool".to_owned(),
                        serde_json::Value::String(t.tool_name.clone()),
                    );
                }
                enriched
            }
            Some(ToolImpl::SubAgent(_)) => {
                let mut enriched = params.clone();
                if let Some(obj) = enriched.as_object_mut() {
                    obj.insert(
                        "action".to_owned(),
                        serde_json::Value::String("invoke".to_owned()),
                    );
                }
                enriched
            }
            _ => params.clone(),
        }
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforcement_name_non_mcp_returns_tool_name() {
        let registry = ToolRegistry::new();
        assert_eq!(registry.enforcement_name("bash"), "bash");
        assert_eq!(registry.enforcement_name("unknown"), "unknown");
    }

    #[test]
    fn enrich_params_non_mcp_returns_unchanged() {
        let registry = ToolRegistry::new();
        let params = json!({"command": "ls /tmp"});
        let enriched = registry.enrich_params("bash", &params);
        assert_eq!(enriched, params);
    }

    #[test]
    fn enrich_params_non_mcp_no_mcp_keys() {
        let registry = ToolRegistry::new();
        let params = json!({"command": "ls"});
        let enriched = registry.enrich_params("bash", &params);
        assert!(enriched.get("__mcp_server").is_none());
        assert!(enriched.get("__mcp_tool").is_none());
    }
}

/// Extension point for tool implementations. Not used for known variants —
/// enum dispatch via `ToolImpl` is preferred. Reserved for future external plugins.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn execute(
        &self,
        action: &str,
        params: &serde_json::Value,
        token: CapabilityToken,
    ) -> Result<ToolResult, CherubError>;
}
