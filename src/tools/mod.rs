pub mod bash;

use std::marker::PhantomData;

use serde_json::json;

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;
use crate::providers::ToolDefinition;

use bash::BashTool;

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

impl ToolInvocation<Evaluated> {
    /// Execute the tool invocation via the registry. Requires a `CapabilityToken` (consumed on use).
    pub async fn execute(
        self,
        token: CapabilityToken,
        registry: &ToolRegistry,
    ) -> Result<ToolResult, CherubError> {
        let tool = registry.find(&self.tool).ok_or_else(|| {
            CherubError::InvalidInvocation(format!("unknown tool: {}", self.tool))
        })?;
        tool.execute(&self.params, token).await
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
}

impl ToolImpl {
    fn name(&self) -> &str {
        match self {
            Self::Bash(_) => "bash",
        }
    }

    async fn execute(
        &self,
        params: &serde_json::Value,
        token: CapabilityToken,
    ) -> Result<ToolResult, CherubError> {
        match self {
            Self::Bash(tool) => tool.execute(params, token).await,
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
