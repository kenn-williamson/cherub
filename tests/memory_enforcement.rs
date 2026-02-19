//! Enforcement tests for the memory tool (M6b).
//!
//! Tests that memory operations route to the correct tier (Observe/Act/Commit)
//! based on the structured match_source and the configured patterns.
//!
//! Does not require a database — uses the mock provider + in-memory enforcement.

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::Mutex;

use serde_json::json;

use cherub::enforcement::policy::Policy;
use cherub::error::CherubError;
use cherub::providers::{ContentBlock, Message, Provider, StopReason, ToolDefinition};
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
use cherub::runtime::output::NullSink;
use cherub::tools::ToolRegistry;

// ---------------------------------------------------------------------------
// Mock infrastructure (shared with adversarial.rs)
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

impl Provider for MockProvider {
    async fn complete(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<Message, CherubError> {
        let mut queue = self.responses.lock().unwrap();
        Ok(queue.pop_front().unwrap_or_else(end_turn))
    }
}

enum MockApprovalPolicy {
    AlwaysDeny,
    AlwaysApprove,
}

struct MockApprovalGate {
    policy: MockApprovalPolicy,
}

impl ApprovalGate for MockApprovalGate {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        match self.policy {
            MockApprovalPolicy::AlwaysDeny => ApprovalResult::Denied,
            MockApprovalPolicy::AlwaysApprove => ApprovalResult::Approved,
        }
    }
}

fn end_turn() -> Message {
    Message::Assistant {
        content: vec![ContentBlock::Text {
            text: String::new(),
        }],
        stop_reason: StopReason::EndTurn,
    }
}

fn memory_tool_msg(id: &str, action: &str, path: Option<&str>) -> Message {
    let input = if let Some(p) = path {
        json!({
            "action": action,
            "path": p,
            "content": "test content",
            "category": "preference"
        })
    } else {
        json!({
            "action": action,
            "content": "test content",
            "category": "preference"
        })
    };
    Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: id.to_owned(),
            name: "memory".to_owned(),
            input,
        }],
        stop_reason: StopReason::ToolUse,
    }
}

// Policy used for all enforcement tests below.
const MEMORY_POLICY: &str = r#"
[tools.memory]
enabled = true
match_source = "structured"

[tools.memory.actions.read]
tier = "observe"
patterns = [
    "^recall$",
    "^recall:",
    "^search$",
    "^search:",
]

[tools.memory.actions.write_working]
tier = "observe"
patterns = [
    "^store:working/",
    "^update:working/",
    "^forget:working/",
]

[tools.memory.actions.write_user]
tier = "act"
patterns = [
    "^store:preferences/",
    "^store:observations/",
    "^store:facts/",
    "^update:preferences/",
    "^update:observations/",
    "^update:facts/",
]

[tools.memory.actions.write_identity]
tier = "commit"
patterns = [
    "^store:identity/",
    "^update:identity/",
    "^store:agent/",
    "^update:agent/",
    "^store:instructions/",
    "^update:instructions/",
]

[tools.memory.actions.delete]
tier = "commit"
patterns = [
    "^forget$",
    "^forget:",
]
"#;

fn make_agent(
    responses: Vec<Message>,
    approval_policy: MockApprovalPolicy,
) -> AgentLoop<MockProvider, MockApprovalGate, NullSink> {
    let policy = Policy::from_str(MEMORY_POLICY).unwrap();
    let provider = MockProvider::new(responses);
    let registry = ToolRegistry::new(); // no memory store — enforcement tests only
    let approval_gate = MockApprovalGate {
        policy: approval_policy,
    };
    AgentLoop::new(
        policy,
        provider,
        registry,
        "test".to_owned(),
        approval_gate,
        NullSink,
        "test_user",
    )
}

// ---------------------------------------------------------------------------
// Tests: read operations are observe-tier (auto-allowed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recall_is_allowed_no_path() {
    // recall without path → matches "^recall$" → Observe → Allow
    // Tool is registered but has no store, so it will error on execute.
    // We just check it gets past enforcement (error from missing tool impl is OK).
    let responses = vec![memory_tool_msg("t1", "recall", None), end_turn()];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysDeny);
    agent.run_turn_text("test").await.unwrap();
    // If enforcement rejected, it would push "action not permitted".
    // If enforcement allowed but tool errored, we get a ToolResult with error.
    // Either way the run completes without panicking.
    let msgs = agent.session_messages();
    let tool_result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    if let Some(Message::ToolResult { content, .. }) = tool_result {
        // Must NOT be a policy rejection.
        assert_ne!(
            content, "action not permitted",
            "recall should not be rejected by policy"
        );
    }
}

#[tokio::test]
async fn search_is_allowed() {
    let responses = vec![
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t2".to_owned(),
                name: "memory".to_owned(),
                input: json!({"action": "search", "query": "preferences"}),
            }],
            stop_reason: StopReason::ToolUse,
        },
        end_turn(),
    ];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysDeny);
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    if let Some(Message::ToolResult { content, .. }) = result {
        assert_ne!(
            content, "action not permitted",
            "search should not be rejected by policy"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests: identity/agent writes escalate (require human approval)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_identity_escalates_and_deny_rejects() {
    // store:identity/ is commit-tier → escalates → gate denies → "action not permitted"
    let responses = vec![
        memory_tool_msg("t3", "store", Some("identity/values")),
        end_turn(),
    ];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysDeny);
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { is_error: true, .. }));
    assert!(
        result.is_some(),
        "identity write should result in error when denied"
    );
    if let Some(Message::ToolResult { content, .. }) = result {
        assert_eq!(content, "action not permitted");
    }
}

#[tokio::test]
async fn store_identity_escalates_and_approve_executes() {
    // Same but gate approves → tool runs (will error since no store, but passes enforcement)
    let responses = vec![
        memory_tool_msg("t4", "store", Some("identity/values")),
        end_turn(),
    ];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysApprove);
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    if let Some(Message::ToolResult { content, .. }) = result {
        assert_ne!(
            content, "action not permitted",
            "approved identity write should not return 'action not permitted'"
        );
    }
}

#[tokio::test]
async fn forget_escalates_and_deny_rejects() {
    // forget is commit-tier → escalates → deny → "action not permitted"
    let responses = vec![
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t5".to_owned(),
                name: "memory".to_owned(),
                input: json!({"action": "forget", "id": "00000000-0000-7000-8000-000000000001"}),
            }],
            stop_reason: StopReason::ToolUse,
        },
        end_turn(),
    ];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysDeny);
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { is_error: true, .. }));
    assert!(result.is_some());
    if let Some(Message::ToolResult { content, .. }) = result {
        assert_eq!(content, "action not permitted");
    }
}

// ---------------------------------------------------------------------------
// Tests: unknown actions are rejected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_action_rejected() {
    let responses = vec![memory_tool_msg("t6", "inject_persona", None), end_turn()];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysApprove);
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { is_error: true, .. }));
    assert!(result.is_some(), "unknown action should be rejected");
    if let Some(Message::ToolResult { content, .. }) = result {
        assert_eq!(content, "action not permitted");
    }
}

#[tokio::test]
async fn memory_tool_not_in_policy_rejected() {
    // Use a policy that doesn't include memory tool.
    let policy_str = r#"
[tools.bash]
enabled = true
[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
"#;
    let policy = Policy::from_str(policy_str).unwrap();
    let provider = MockProvider::new(vec![memory_tool_msg("t7", "recall", None), end_turn()]);
    let registry = ToolRegistry::new();
    let approval_gate = MockApprovalGate {
        policy: MockApprovalPolicy::AlwaysDeny,
    };
    let mut agent = AgentLoop::new(
        policy,
        provider,
        registry,
        "test".to_owned(),
        approval_gate,
        NullSink,
        "test_user",
    );
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { is_error: true, .. }));
    assert!(
        result.is_some(),
        "memory tool not in policy should be rejected"
    );
    if let Some(Message::ToolResult { content, .. }) = result {
        assert_eq!(content, "action not permitted");
    }
}

// ---------------------------------------------------------------------------
// Tests: prompt injection attempts via memory action field
// ---------------------------------------------------------------------------

#[tokio::test]
async fn injection_in_action_field_rejected() {
    // Adversary tries to inject a bash command in the action field.
    let responses = vec![
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t8".to_owned(),
                name: "memory".to_owned(),
                input: json!({"action": "recall; rm -rf /", "path": "preferences/"}),
            }],
            stop_reason: StopReason::ToolUse,
        },
        end_turn(),
    ];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysApprove);
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { is_error: true, .. }));
    // "recall; rm -rf /" with path "preferences/" produces "recall; rm -rf /:preferences/"
    // which won't match any pattern → rejected.
    assert!(result.is_some(), "injected action should be rejected");
    if let Some(Message::ToolResult { content, .. }) = result {
        assert_eq!(content, "action not permitted");
    }
}
