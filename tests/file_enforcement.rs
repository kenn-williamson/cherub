//! Enforcement tests for the file tool.
//!
//! Tests that file operations route to the correct tier (Observe/Act)
//! based on the structured match_source and the configured patterns.
//!
//! Does not require a database or filesystem operations — uses the mock provider
//! + in-memory enforcement. The tool will error on execute (missing file) but
//! the enforcement decision is what we're testing.

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::Mutex;

use serde_json::json;

use async_trait::async_trait;

use cherub::enforcement::policy::Policy;
use cherub::error::CherubError;
use cherub::providers::pricing::ModelPricing;
use cherub::providers::{ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition};
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
use cherub::runtime::output::NullSink;
use cherub::tools::ToolRegistry;

// ---------------------------------------------------------------------------
// Mock infrastructure
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
        Ok((queue.pop_front().unwrap_or_else(end_turn), None))
    }

    fn model_name(&self) -> &str {
        "mock"
    }

    fn max_output_tokens(&self) -> u32 {
        4096
    }

    fn pricing(&self) -> Option<ModelPricing> {
        None
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

fn file_tool_msg(id: &str, action: &str, path: Option<&str>) -> Message {
    let mut input = json!({"action": action});
    if let Some(p) = path {
        input["path"] = json!(p);
    }
    // Add required fields for specific actions.
    if action == "read" && path.is_none() {
        input["path"] = json!("test.txt");
    }
    if action == "edit" {
        if path.is_none() {
            input["path"] = json!("test.txt");
        }
        input["old_string"] = json!("old");
        input["new_string"] = json!("new");
    }
    if action == "grep" {
        input["pattern"] = json!("test");
    }
    if action == "glob" {
        input["pattern"] = json!("*.rs");
    }
    Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: id.to_owned(),
            name: "file".to_owned(),
            input,
        }],
        stop_reason: StopReason::ToolUse,
    }
}

const FILE_POLICY: &str = r#"
[tools.file]
enabled = true
match_source = "structured"

[tools.file.actions.read_ops]
tier = "observe"
patterns = [
    "^read:",
    "^read$",
    "^glob:",
    "^glob$",
    "^grep:",
    "^grep$",
]

[tools.file.actions.write_ops]
tier = "act"
patterns = [
    "^edit:",
    "^edit$",
]
"#;

fn make_agent(
    responses: Vec<Message>,
    approval_policy: MockApprovalPolicy,
) -> AgentLoop<MockApprovalGate, NullSink> {
    let policy = Policy::from_str(FILE_POLICY).unwrap();
    let provider = MockProvider::new(responses);
    let registry = ToolRegistry::new();
    let approval_gate = MockApprovalGate {
        policy: approval_policy,
    };
    AgentLoop::new(
        policy,
        Box::new(provider),
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
async fn read_is_allowed() {
    let responses = vec![file_tool_msg("t1", "read", Some("src/main.rs")), end_turn()];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysDeny);
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let tool_result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    if let Some(Message::ToolResult { content, .. }) = tool_result {
        assert_ne!(
            content, "action not permitted",
            "read should not be rejected by policy"
        );
    }
}

#[tokio::test]
async fn glob_is_allowed() {
    let responses = vec![file_tool_msg("t2", "glob", None), end_turn()];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysDeny);
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let tool_result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    if let Some(Message::ToolResult { content, .. }) = tool_result {
        assert_ne!(
            content, "action not permitted",
            "glob should not be rejected by policy"
        );
    }
}

#[tokio::test]
async fn grep_is_allowed() {
    let responses = vec![file_tool_msg("t3", "grep", None), end_turn()];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysDeny);
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let tool_result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    if let Some(Message::ToolResult { content, .. }) = tool_result {
        assert_ne!(
            content, "action not permitted",
            "grep should not be rejected by policy"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests: edit is act-tier (requires act or higher)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_is_allowed_at_act_tier() {
    // Act tier is auto-allowed in the default CLI flow, so even with AlwaysDeny
    // (which only blocks Commit-tier escalations), act tier should pass.
    let responses = vec![file_tool_msg("t4", "edit", Some("src/main.rs")), end_turn()];
    let mut agent = make_agent(responses, MockApprovalPolicy::AlwaysDeny);
    agent.run_turn_text("test").await.unwrap();
    let msgs = agent.session_messages();
    let tool_result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    if let Some(Message::ToolResult { content, .. }) = tool_result {
        assert_ne!(
            content, "action not permitted",
            "edit at act tier should not be rejected"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests: unknown actions are rejected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_action_rejected() {
    let responses = vec![file_tool_msg("t5", "delete", None), end_turn()];
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
async fn file_tool_not_in_policy_rejected() {
    let policy_str = r#"
[tools.bash]
enabled = true
[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
"#;
    let policy = Policy::from_str(policy_str).unwrap();
    let provider = MockProvider::new(vec![
        file_tool_msg("t6", "read", Some("test.txt")),
        end_turn(),
    ]);
    let registry = ToolRegistry::new();
    let approval_gate = MockApprovalGate {
        policy: MockApprovalPolicy::AlwaysDeny,
    };
    let mut agent = AgentLoop::new(
        policy,
        Box::new(provider),
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
        "file tool not in policy should be rejected"
    );
    if let Some(Message::ToolResult { content, .. }) = result {
        assert_eq!(content, "action not permitted");
    }
}

// ---------------------------------------------------------------------------
// Tests: injection attempts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn injection_in_action_field_rejected() {
    let responses = vec![
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t7".to_owned(),
                name: "file".to_owned(),
                input: json!({"action": "read; rm -rf /", "path": "test.txt"}),
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
    assert!(result.is_some(), "injected action should be rejected");
    if let Some(Message::ToolResult { content, .. }) = result {
        assert_eq!(content, "action not permitted");
    }
}
