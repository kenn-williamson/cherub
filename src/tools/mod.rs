pub mod bash;

use std::marker::PhantomData;

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;

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
    /// Execute the tool invocation. Requires a `CapabilityToken` (consumed on use).
    pub fn execute(
        self,
        _token: CapabilityToken,
    ) -> Result<ToolResult, CherubError> {
        // Stub: Milestone 2 adds tool dispatch.
        Ok(ToolResult {
            output: String::new(),
        })
    }
}

pub struct ToolResult {
    pub output: String,
}

/// Extension point for tool implementations. One of only two `dyn Trait`
/// boundaries in the project (with `Provider`).
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn execute(
        &self,
        action: &str,
        params: &serde_json::Value,
        token: CapabilityToken,
    ) -> Result<ToolResult, CherubError>;
}
