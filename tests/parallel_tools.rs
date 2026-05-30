//! Integration tests for parallel tool execution (M18c).
//!
//! Verifies that when the model returns multiple `tool_use` blocks,
//! they execute concurrently via the 4-phase pipeline. No API key required.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::json;

use async_trait::async_trait;

use cherub::enforcement::policy::Policy;
use cherub::error::CherubError;
use cherub::providers::{ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition};
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
use cherub::runtime::output::NullSink;
use cherub::tools::ToolRegistry;

// ---------------------------------------------------------------------------
// Mock Provider
// ---------------------------------------------------------------------------

struct MockProvider {
    responses: Mutex<VecDeque<Message>>,
}

impl MockProvider {
    fn new(responses: Vec<Message>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        let mut queue = self.responses.lock().unwrap();
        Ok((
            queue.pop_front().unwrap_or_else(|| Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: "done".to_owned(),
                }],
                stop_reason: StopReason::EndTurn,
            }),
            None,
        ))
    }

    fn model_name(&self) -> &str {
        "mock"
    }

    fn max_output_tokens(&self) -> u32 {
        4096
    }
}

// ---------------------------------------------------------------------------
// Mock Approval Gate
// ---------------------------------------------------------------------------

struct AlwaysApproveGate;

impl ApprovalGate for AlwaysApproveGate {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        ApprovalResult::Approved
    }
}

struct AlwaysDenyGate;

impl ApprovalGate for AlwaysDenyGate {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        ApprovalResult::Denied
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn multi_tool_msg(tools: Vec<(&str, &str, serde_json::Value)>) -> Message {
    let mut content: Vec<ContentBlock> = Vec::new();
    for (id, name, input) in tools {
        content.push(ContentBlock::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input,
        });
    }
    Message::Assistant {
        content,
        stop_reason: StopReason::ToolUse,
    }
}

fn single_tool_msg(id: &str, name: &str, input: serde_json::Value) -> Message {
    Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input,
        }],
        stop_reason: StopReason::ToolUse,
    }
}

fn end_turn() -> Message {
    Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "done".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    }
}

fn default_policy() -> Policy {
    Policy::load(std::path::Path::new("config/default_policy.toml")).unwrap()
}

/// Inline policy that allows `sleep` and `echo` so the timing test can actually execute.
const SLEEP_POLICY: &str = r#"
[tools.bash]
enabled = true

[tools.bash.actions.run]
tier = "observe"
patterns = ["^sleep ", "^echo "]
"#;

fn make_agent<A: ApprovalGate>(responses: Vec<Message>, gate: A) -> AgentLoop<A, NullSink> {
    let policy = default_policy();
    let provider = Arc::new(MockProvider::new(responses));
    let registry = ToolRegistry::new();
    AgentLoop::new(
        policy,
        provider,
        Arc::new(registry),
        "test".to_owned(),
        gate,
        NullSink,
        "test-user",
    )
}

fn make_agent_with_policy<A: ApprovalGate>(
    policy: Policy,
    responses: Vec<Message>,
    gate: A,
) -> AgentLoop<A, NullSink> {
    let provider = Arc::new(MockProvider::new(responses));
    let registry = ToolRegistry::new();
    AgentLoop::new(
        policy,
        provider,
        Arc::new(registry),
        "test".to_owned(),
        gate,
        NullSink,
        "test-user",
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Two bash observe-tier tool calls execute successfully.
#[tokio::test]
async fn parallel_two_tools_both_execute() {
    let responses = vec![
        multi_tool_msg(vec![
            ("t1", "bash", json!({"command": "echo hello"})),
            ("t2", "bash", json!({"command": "echo world"})),
        ]),
        end_turn(),
    ];

    let mut agent = make_agent(responses, AlwaysApproveGate);
    let result = agent.run_turn_text("run two commands").await;
    assert!(result.is_ok());

    // Session should have: user msg, assistant msg with 2 tool_uses,
    // 2 tool results, final assistant response.
    let msgs = agent.session_messages();
    assert!(msgs.len() >= 5, "expected 5+ messages, got {}", msgs.len());

    // Both tool results should be present.
    let results: Vec<&str> = msgs
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "expected 2 tool results: {results:?}");
}

/// Single tool call uses the fast path (no join_all overhead).
#[tokio::test]
async fn parallel_single_tool_fast_path() {
    let responses = vec![
        single_tool_msg("t1", "bash", json!({"command": "echo hello"})),
        end_turn(),
    ];

    let mut agent = make_agent(responses, AlwaysApproveGate);
    let result = agent.run_turn_text("run one command").await;
    assert!(result.is_ok());

    let results: Vec<&str> = agent
        .session_messages()
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
}

/// One tool allowed, one tool rejected — mixed decisions handled correctly.
#[tokio::test]
async fn parallel_one_rejected_one_allowed() {
    let responses = vec![
        multi_tool_msg(vec![
            ("t1", "bash", json!({"command": "echo hello"})), // observe → allow
            ("t2", "bash", json!({"command": "rm -rf /"})),   // commit → escalate
        ]),
        end_turn(),
    ];

    // Deny escalations so the rm command is rejected.
    let mut agent = make_agent(responses, AlwaysDenyGate);
    let result = agent.run_turn_text("mixed commands").await;
    assert!(result.is_ok());

    let msgs = agent.session_messages();
    let results: Vec<(&str, bool)> = msgs
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult {
                content, is_error, ..
            } => Some((content.as_str(), *is_error)),
            _ => None,
        })
        .collect();

    assert_eq!(results.len(), 2, "expected 2 tool results: {results:?}");

    // One should be an error (rejected), one should succeed.
    let errors: Vec<_> = results.iter().filter(|(_, is_err)| *is_err).collect();
    let successes: Vec<_> = results.iter().filter(|(_, is_err)| !*is_err).collect();
    assert_eq!(errors.len(), 1, "expected 1 rejected tool");
    assert_eq!(successes.len(), 1, "expected 1 successful tool");

    // The rejected one should say "action not permitted".
    assert!(
        errors[0].0.contains("action not permitted"),
        "rejected tool should return 'action not permitted': {}",
        errors[0].0,
    );
}

/// Escalated tool approved, then both execute in parallel.
#[tokio::test]
async fn parallel_escalation_then_execute() {
    let responses = vec![
        multi_tool_msg(vec![
            ("t1", "bash", json!({"command": "echo safe"})), // observe → allow
            ("t2", "bash", json!({"command": "rm tmp.txt"})), // commit → escalate → approve
        ]),
        end_turn(),
    ];

    let mut agent = make_agent(responses, AlwaysApproveGate);
    let result = agent.run_turn_text("both should execute").await;
    assert!(result.is_ok());

    let results: Vec<(&str, bool)> = agent
        .session_messages()
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult {
                content, is_error, ..
            } => Some((content.as_str(), *is_error)),
            _ => None,
        })
        .collect();

    assert_eq!(results.len(), 2, "expected 2 tool results: {results:?}");
    // Both should have executed (not rejected).
    // The rm might fail on execution (no such file), but it shouldn't be "action not permitted".
    for (content, _) in &results {
        assert!(
            !content.contains("action not permitted"),
            "both tools should be allowed: {content}",
        );
    }
}

/// Parallel execution actually runs concurrently (timing test).
/// Two `sleep 1` commands should complete in ~1s, not ~2s.
/// Uses an inline policy that allows `sleep` (not in the default policy).
#[tokio::test]
async fn parallel_execution_is_concurrent() {
    use std::str::FromStr;
    let policy = Policy::from_str(SLEEP_POLICY).unwrap();

    let responses = vec![
        multi_tool_msg(vec![
            ("t1", "bash", json!({"command": "sleep 1 && echo a"})),
            ("t2", "bash", json!({"command": "sleep 1 && echo b"})),
        ]),
        end_turn(),
    ];

    let mut agent = make_agent_with_policy(policy, responses, AlwaysApproveGate);
    let start = Instant::now();
    let result = agent.run_turn_text("concurrent sleep").await;
    let elapsed = start.elapsed();

    assert!(result.is_ok());
    // If sequential, would take ~2s. Parallel should be ~1s.
    // Use 1.8s as threshold to account for overhead.
    assert!(
        elapsed.as_secs_f64() < 1.8,
        "parallel execution took {:.2}s — should be ~1s if concurrent",
        elapsed.as_secs_f64(),
    );
}
