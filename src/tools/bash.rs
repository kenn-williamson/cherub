use std::time::Duration;

use tokio::process::Command;

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;

use super::ToolResult;

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT: usize = 256 * 1024; // 256 KiB

/// Bash command execution tool.
pub struct BashTool;

impl BashTool {
    pub async fn execute(
        &self,
        params: &serde_json::Value,
        _token: CapabilityToken,
    ) -> Result<ToolResult, CherubError> {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CherubError::InvalidInvocation("missing 'command' parameter".to_owned()))?;

        let result = tokio::time::timeout(TIMEOUT, async {
            Command::new("bash")
                .arg("-c")
                .arg(command)
                .kill_on_drop(true)
                .output()
                .await
        })
        .await;

        match result {
            Err(_) => Err(CherubError::ToolExecution(format!(
                "command timed out after {}s",
                TIMEOUT.as_secs()
            ))),
            Ok(Err(e)) => Err(CherubError::ToolExecution(format!("failed to spawn: {e}"))),
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr);

                if !stderr.is_empty() {
                    if !stdout.is_empty() && !stdout.ends_with('\n') {
                        stdout.push('\n');
                    }
                    stdout.push_str(&stderr);
                }

                if !output.status.success() {
                    let code = output.status.code().map_or("unknown".to_owned(), |c| c.to_string());
                    if !stdout.is_empty() && !stdout.ends_with('\n') {
                        stdout.push('\n');
                    }
                    stdout.push_str(&format!("[exit code: {code}]"));
                }

                // Truncate at byte boundary (safe: we truncate the String, not raw bytes)
                if stdout.len() > MAX_OUTPUT {
                    stdout.truncate(MAX_OUTPUT);
                    stdout.push_str("\n[output truncated]");
                }

                Ok(ToolResult { output: stdout })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Helper: create a token via the enforcement layer for testing.
    // We go through the proper enforcement path.
    fn allow_token() -> CapabilityToken {
        use crate::enforcement::{self, policy::Policy};
        use crate::tools::{Proposed, ToolInvocation};
        use std::str::FromStr;

        let policy_str = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^echo ", "^false$", "^sh "]
"#;
        let policy = Policy::from_str(policy_str).unwrap();
        let proposal = ToolInvocation::<Proposed>::new("bash", "execute", json!({"command": "echo test"}));
        let (_, decision) = enforcement::evaluate(proposal, &policy);
        match decision {
            enforcement::Decision::Allow(token) => token,
            _ => panic!("expected Allow"),
        }
    }

    #[tokio::test]
    async fn echo_hello() {
        let tool = BashTool;
        let result = tool
            .execute(&json!({"command": "echo hello"}), allow_token())
            .await
            .unwrap();
        assert_eq!(result.output.trim(), "hello");
    }

    #[tokio::test]
    async fn captures_stderr() {
        let tool = BashTool;
        let result = tool
            .execute(
                &json!({"command": "echo out && echo err >&2"}),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("out"));
        assert!(result.output.contains("err"));
    }

    #[tokio::test]
    async fn nonzero_exit_code() {
        let tool = BashTool;
        let result = tool
            .execute(&json!({"command": "false"}), allow_token())
            .await
            .unwrap();
        assert!(result.output.contains("[exit code: 1]"));
    }

    #[tokio::test]
    async fn missing_command_param() {
        let tool = BashTool;
        let err = tool
            .execute(&json!({"args": ["--version"]}), allow_token())
            .await
            .unwrap_err();
        assert!(matches!(err, CherubError::InvalidInvocation(_)));
    }

    #[tokio::test]
    async fn exit_code_included() {
        let tool = BashTool;
        let result = tool
            .execute(&json!({"command": "sh -c 'exit 42'"}), allow_token())
            .await
            .unwrap();
        assert!(result.output.contains("[exit code: 42]"));
    }
}
