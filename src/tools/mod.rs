pub mod bash;
#[cfg(feature = "memory")]
pub mod memory;

use std::marker::PhantomData;

use serde_json::json;
use uuid::Uuid;

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;
use crate::providers::ToolDefinition;

use bash::BashTool;
#[cfg(feature = "memory")]
use memory::MemoryTool;

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

#[derive(Debug)]
pub struct ToolResult {
    pub output: String,
}

/// Enum dispatch for tool implementations. Known variants at compile time.
/// `dyn Tool` deferred to M7 plugin IPC.
pub(crate) enum ToolImpl {
    Bash(BashTool),
    #[cfg(feature = "memory")]
    Memory(MemoryTool),
}

impl ToolImpl {
    fn name(&self) -> &str {
        match self {
            Self::Bash(_) => "bash",
            #[cfg(feature = "memory")]
            Self::Memory(_) => "memory",
        }
    }

    async fn execute(
        &self,
        params: &serde_json::Value,
        token: CapabilityToken,
        // Prefixed with _ to suppress warning when compiled without --features memory.
        // Memory arms use it for provenance; bash ignores it.
        _ctx: &ToolContext,
    ) -> Result<ToolResult, CherubError> {
        match self {
            Self::Bash(tool) => tool.execute(params, token).await,
            #[cfg(feature = "memory")]
            Self::Memory(tool) => tool.execute(params, token, _ctx).await,
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
                        }
                    },
                    "required": ["action"]
                }),
            },
        }
    }
}

/// Registry of available tools. Provides lookup and schema definitions.
pub struct ToolRegistry {
    tools: Vec<ToolImpl>,
}

impl ToolRegistry {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            tools: vec![ToolImpl::Bash(BashTool::new())],
        }
    }

    /// Create a registry with the memory tool attached.
    #[cfg(feature = "memory")]
    pub fn with_memory(store: Box<dyn crate::storage::MemoryStore>) -> Self {
        Self {
            tools: vec![
                ToolImpl::Bash(BashTool::new()),
                ToolImpl::Memory(MemoryTool::new(store)),
            ],
        }
    }

    pub(crate) fn find(&self, name: &str) -> Option<&ToolImpl> {
        self.tools.iter().find(|t| t.name() == name)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }
}

/// Extension point for tool implementations. Retained for future M7 plugin use.
/// Not used in M2 — enum dispatch via `ToolImpl` is preferred for known variants.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn execute(
        &self,
        action: &str,
        params: &serde_json::Value,
        token: CapabilityToken,
    ) -> Result<ToolResult, CherubError>;
}
