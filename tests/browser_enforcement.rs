//! Enforcement tests for the browser tool.
//!
//! Tests that browser actions route to the correct tier based on
//! BrowserStructured match_source and configured patterns.
//!
//! No Docker, no browser, no network — uses mock provider + in-memory enforcement.

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::Mutex;

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
// Mock infrastructure (same pattern as file_enforcement.rs)
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
}

struct MockApprovalGate {
    always_approve: bool,
}

impl ApprovalGate for MockApprovalGate {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        if self.always_approve {
            ApprovalResult::Approved
        } else {
            ApprovalResult::Denied
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

fn browser_msg(id: &str, action: &str, url: Option<&str>) -> Message {
    let mut input = json!({"action": action});
    if let Some(u) = url {
        input["url"] = json!(u);
    }
    Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: id.to_owned(),
            name: "browser".to_owned(),
            input,
        }],
        stop_reason: StopReason::ToolUse,
    }
}

const BROWSER_POLICY: &str = r#"
[tools.browser]
enabled = true
match_source = "browser_structured"

[tools.browser.actions.navigate]
tier = "act"
patterns = [
    "^browse:sos\\.state\\.co\\.us$",
    "^browse:www\\.example\\.com$",
]

[tools.browser.actions.interact]
tier = "act"
patterns = [
    "^click$",
    "^fill$",
    "^select$",
    "^wait_for$",
    "^scroll$",
]

[tools.browser.actions.read]
tier = "observe"
patterns = [
    "^get_text$",
    "^get_url$",
    "^screenshot$",
]

[tools.browser.actions.execute_js]
tier = "commit"
patterns = ["^evaluate$"]
"#;

fn make_agent(
    responses: Vec<Message>,
    always_approve: bool,
) -> AgentLoop<MockApprovalGate, NullSink> {
    let policy = Policy::from_str(BROWSER_POLICY).unwrap();
    let provider = MockProvider::new(responses);
    // Browser tool isn't in the registry (no Docker), but enforcement still evaluates.
    let registry = ToolRegistry::new();
    let approval_gate = MockApprovalGate { always_approve };
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

fn find_tool_result(msgs: &[Message]) -> Option<&str> {
    msgs.iter().find_map(|m| match m {
        Message::ToolResult { content, .. } => Some(content.as_str()),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn browse_allowed_domain_permitted() {
    let responses = vec![
        browser_msg("t1", "browse", Some("https://sos.state.co.us/biz/search")),
        end_turn(),
    ];
    let mut agent = make_agent(responses, false);
    agent.run_turn_text("search for LLC").await.unwrap();
    let result = find_tool_result(agent.session_messages());
    // The tool itself will fail (no actual browser), but enforcement should allow it.
    // "action not permitted" means enforcement rejected; anything else means it passed.
    if let Some(content) = result {
        assert_ne!(
            content, "action not permitted",
            "browse to allowed domain should pass enforcement"
        );
    }
}

#[tokio::test]
async fn browse_disallowed_domain_rejected() {
    let responses = vec![
        browser_msg("t1", "browse", Some("https://evil.com/steal")),
        end_turn(),
    ];
    let mut agent = make_agent(responses, false);
    agent.run_turn_text("test").await.unwrap();
    let result = find_tool_result(agent.session_messages());
    assert_eq!(
        result,
        Some("action not permitted"),
        "browse to unlisted domain should be rejected"
    );
}

#[tokio::test]
async fn click_without_url_permitted() {
    let responses = vec![browser_msg("t1", "click", None), end_turn()];
    let mut agent = make_agent(responses, false);
    agent.run_turn_text("test").await.unwrap();
    let result = find_tool_result(agent.session_messages());
    if let Some(content) = result {
        assert_ne!(
            content, "action not permitted",
            "click should pass enforcement"
        );
    }
}

#[tokio::test]
async fn fill_without_url_permitted() {
    let responses = vec![browser_msg("t1", "fill", None), end_turn()];
    let mut agent = make_agent(responses, false);
    agent.run_turn_text("test").await.unwrap();
    let result = find_tool_result(agent.session_messages());
    if let Some(content) = result {
        assert_ne!(
            content, "action not permitted",
            "fill should pass enforcement"
        );
    }
}

#[tokio::test]
async fn get_text_observe_tier_permitted() {
    let responses = vec![browser_msg("t1", "get_text", None), end_turn()];
    let mut agent = make_agent(responses, false);
    agent.run_turn_text("test").await.unwrap();
    let result = find_tool_result(agent.session_messages());
    if let Some(content) = result {
        assert_ne!(
            content, "action not permitted",
            "get_text should pass enforcement"
        );
    }
}

#[tokio::test]
async fn screenshot_observe_tier_permitted() {
    let responses = vec![browser_msg("t1", "screenshot", None), end_turn()];
    let mut agent = make_agent(responses, false);
    agent.run_turn_text("test").await.unwrap();
    let result = find_tool_result(agent.session_messages());
    if let Some(content) = result {
        assert_ne!(
            content, "action not permitted",
            "screenshot should pass enforcement"
        );
    }
}

#[tokio::test]
async fn evaluate_requires_approval_denied() {
    // Commit tier + denial gate → "action not permitted"
    let responses = vec![browser_msg("t1", "evaluate", None), end_turn()];
    let mut agent = make_agent(responses, false);
    agent.run_turn_text("test").await.unwrap();
    let result = find_tool_result(agent.session_messages());
    assert_eq!(
        result,
        Some("action not permitted"),
        "evaluate should escalate and be denied"
    );
}

#[tokio::test]
async fn evaluate_requires_approval_approved() {
    // Commit tier + approval gate → should pass enforcement (tool may still fail)
    let responses = vec![browser_msg("t1", "evaluate", None), end_turn()];
    let mut agent = make_agent(responses, true);
    agent.run_turn_text("test").await.unwrap();
    let result = find_tool_result(agent.session_messages());
    if let Some(content) = result {
        assert_ne!(
            content, "action not permitted",
            "evaluate should pass after approval"
        );
    }
}

#[tokio::test]
async fn unknown_action_rejected() {
    let responses = vec![browser_msg("t1", "delete", None), end_turn()];
    let mut agent = make_agent(responses, false);
    agent.run_turn_text("test").await.unwrap();
    let result = find_tool_result(agent.session_messages());
    assert_eq!(
        result,
        Some("action not permitted"),
        "unknown action should be rejected"
    );
}

#[tokio::test]
async fn browse_without_url_rejected() {
    // browse without url → BrowserStructured extracts just "browse" (no host)
    // → doesn't match "^browse:sos\\.state\\.co\\.us$" → rejected
    let responses = vec![browser_msg("t1", "browse", None), end_turn()];
    let mut agent = make_agent(responses, false);
    agent.run_turn_text("test").await.unwrap();
    let result = find_tool_result(agent.session_messages());
    assert_eq!(
        result,
        Some("action not permitted"),
        "browse without url should not match domain patterns"
    );
}
