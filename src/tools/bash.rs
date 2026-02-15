use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;

use super::{Tool, ToolResult};

/// Bash command execution tool. Milestone 2 adds actual process spawning.
pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn execute(
        &self,
        _action: &str,
        _params: &serde_json::Value,
        _token: CapabilityToken,
    ) -> Result<ToolResult, CherubError> {
        // Stub: Milestone 2 implements bash execution via tokio::process::Command.
        Ok(ToolResult {
            output: String::new(),
        })
    }
}
