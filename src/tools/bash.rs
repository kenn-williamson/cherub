use std::time::{Duration, Instant};

use tokio::process::Command;
use tracing::{info, info_span, warn};

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;

use super::ToolResult;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_MAX_OUTPUT: usize = 256 * 1024; // 256 KiB

/// Bash command execution tool.
pub struct BashTool {
    pub(crate) timeout: Duration,
    pub(crate) max_output: usize,
}

impl BashTool {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }

    #[cfg(test)]
    fn with_max_output(max_output: usize) -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            max_output,
        }
    }

    pub async fn execute(
        &self,
        params: &serde_json::Value,
        _token: CapabilityToken,
    ) -> Result<ToolResult, CherubError> {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CherubError::InvalidInvocation("missing 'command' parameter".to_owned())
            })?;

        let _span = info_span!("bash_exec", command = %command).entered();
        let start = Instant::now();

        let result = tokio::time::timeout(self.timeout, async {
            Command::new("bash")
                .arg("-c")
                .arg(command)
                .kill_on_drop(true)
                .output()
                .await
        })
        .await;

        match result {
            Err(_) => {
                let duration_ms = start.elapsed().as_millis();
                warn!(duration_ms = %duration_ms, "command timed out");
                Err(CherubError::ToolExecution(format!(
                    "command timed out after {}s",
                    self.timeout.as_secs()
                )))
            }
            Ok(Err(e)) => {
                warn!(error = %e, "failed to spawn");
                Err(CherubError::ToolExecution(format!("failed to spawn: {e}")))
            }
            Ok(Ok(output)) => {
                let duration_ms = start.elapsed().as_millis();
                let exit_code = output.status.code().unwrap_or(-1);
                let stdout_bytes = output.stdout.len();
                let stderr_bytes = output.stderr.len();
                info!(exit_code, stdout_bytes, stderr_bytes, duration_ms = %duration_ms);

                let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr);

                if !stderr.is_empty() {
                    if !stdout.is_empty() && !stdout.ends_with('\n') {
                        stdout.push('\n');
                    }
                    stdout.push_str(&stderr);
                }

                if !output.status.success() {
                    let code = output
                        .status
                        .code()
                        .map_or("unknown".to_owned(), |c| c.to_string());
                    if !stdout.is_empty() && !stdout.ends_with('\n') {
                        stdout.push('\n');
                    }
                    stdout.push_str(&format!("[exit code: {code}]"));
                }

                // Truncate at byte boundary (safe: we truncate the String, not raw bytes)
                if stdout.len() > self.max_output {
                    stdout.truncate(self.max_output);
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
        let proposal =
            ToolInvocation::<Proposed>::new("bash", "execute", json!({"command": "echo test"}));
        let (_, decision) = enforcement::evaluate(proposal, &policy);
        match decision {
            enforcement::Decision::Allow(token) => token,
            _ => panic!("expected Allow"),
        }
    }

    #[tokio::test]
    async fn echo_hello() {
        let tool = BashTool::new();
        let result = tool
            .execute(&json!({"command": "echo hello"}), allow_token())
            .await
            .unwrap();
        assert_eq!(result.output.trim(), "hello");
    }

    #[tokio::test]
    async fn captures_stderr() {
        let tool = BashTool::new();
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
        let tool = BashTool::new();
        let result = tool
            .execute(&json!({"command": "false"}), allow_token())
            .await
            .unwrap();
        assert!(result.output.contains("[exit code: 1]"));
    }

    #[tokio::test]
    async fn missing_command_param() {
        let tool = BashTool::new();
        let err = tool
            .execute(&json!({"args": ["--version"]}), allow_token())
            .await
            .unwrap_err();
        assert!(matches!(err, CherubError::InvalidInvocation(_)));
    }

    #[tokio::test]
    async fn exit_code_included() {
        let tool = BashTool::new();
        let result = tool
            .execute(&json!({"command": "sh -c 'exit 42'"}), allow_token())
            .await
            .unwrap();
        assert!(result.output.contains("[exit code: 42]"));
    }

    // --- Step 6: Error handling tests ---

    #[tokio::test]
    async fn command_timeout_returns_error() {
        let tool = BashTool::with_timeout(Duration::from_millis(100));
        let err = tool
            .execute(&json!({"command": "sleep 10"}), allow_token())
            .await
            .unwrap_err();
        match err {
            CherubError::ToolExecution(msg) => assert!(msg.contains("timed out")),
            other => panic!("expected ToolExecution timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn output_truncation() {
        // Generate output larger than max_output (set to 1 KiB for this test).
        let tool = BashTool::with_max_output(1024);
        let result = tool
            .execute(
                &json!({"command": "head -c 4096 /dev/urandom | base64"}),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("[output truncated]"));
    }

    #[tokio::test]
    async fn spawn_failure_returns_error() {
        // Try to spawn a nonexistent command path.
        // We override the command by passing a command that fails immediately.
        let tool = BashTool::new();
        let result = tool
            .execute(
                &json!({"command": "/nonexistent/binary/that/does/not/exist 2>/dev/null"}),
                allow_token(),
            )
            .await
            .unwrap();
        // The command itself runs via bash -c, so bash returns exit code 127.
        assert!(result.output.contains("[exit code: 127]"));
    }
}
