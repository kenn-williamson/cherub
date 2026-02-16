use std::future::Future;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct EscalationContext<'a> {
    pub tool: &'a str,
    pub command: &'a str,
    pub params: &'a serde_json::Value,
}

pub enum ApprovalResult {
    Approved,
    Denied,
}

/// Abstraction over approval gates. Allows mock gates for testing.
pub trait ApprovalGate: Send + Sync {
    fn request_approval(
        &self,
        context: &EscalationContext<'_>,
    ) -> impl Future<Output = ApprovalResult> + Send;
}

pub struct CliApprovalGate {
    pub(crate) timeout: Duration,
}

impl CliApprovalGate {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl ApprovalGate for CliApprovalGate {
    /// Prompt the user for approval of an escalated action.
    ///
    /// Prints to stderr (not stdout — stdout is for tool output).
    /// Only `y` or `yes` (case-insensitive) → Approved.
    /// Everything else (empty, `n`, garbage, timeout, EOF) → Denied.
    async fn request_approval(&self, context: &EscalationContext<'_>) -> ApprovalResult {
        eprintln!(
            "\n[ESCALATION] {} wants to execute: {}",
            context.tool, context.command
        );
        eprint!("Allow? [y/N] ({}s timeout): ", self.timeout.as_secs());

        let stdin = tokio::io::BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();

        let result = tokio::time::timeout(self.timeout, lines.next_line()).await;

        match result {
            Ok(Ok(Some(line))) => {
                let trimmed = line.trim().to_lowercase();
                if trimmed == "y" || trimmed == "yes" {
                    ApprovalResult::Approved
                } else {
                    ApprovalResult::Denied
                }
            }
            // Timeout, EOF, or I/O error → Denied
            _ => {
                eprintln!();
                ApprovalResult::Denied
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_input(input: &str) -> ApprovalResult {
        let trimmed = input.trim().to_lowercase();
        if trimmed == "y" || trimmed == "yes" {
            ApprovalResult::Approved
        } else {
            ApprovalResult::Denied
        }
    }

    #[test]
    fn y_approves() {
        assert!(matches!(parse_input("y"), ApprovalResult::Approved));
    }

    #[test]
    fn yes_approves() {
        assert!(matches!(parse_input("yes"), ApprovalResult::Approved));
    }

    #[test]
    fn uppercase_y_approves() {
        assert!(matches!(parse_input("Y"), ApprovalResult::Approved));
    }

    #[test]
    fn yes_mixed_case_approves() {
        assert!(matches!(parse_input("Yes"), ApprovalResult::Approved));
    }

    #[test]
    fn n_denies() {
        assert!(matches!(parse_input("n"), ApprovalResult::Denied));
    }

    #[test]
    fn empty_denies() {
        assert!(matches!(parse_input(""), ApprovalResult::Denied));
    }

    #[test]
    fn garbage_denies() {
        assert!(matches!(parse_input("maybe"), ApprovalResult::Denied));
    }

    #[test]
    fn whitespace_only_denies() {
        assert!(matches!(parse_input("  \t  "), ApprovalResult::Denied));
    }
}
