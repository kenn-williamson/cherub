//! Integration tests for M15a/b lifecycle hooks.
//!
//! Uses MockProvider + MockApprovalGate pattern from `tests/output_events.rs`.
//! No API key required — runs as `cargo test`.

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use cherub::enforcement::policy::Policy;
use cherub::error::CherubError;
use cherub::providers::{
    ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition, UserContent,
};
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
use cherub::runtime::hooks::{
    AfterToolCallContext, BeforeToolCallContext, CompactionContext, Hook, HookResult,
    InboundContext, OutputStashingHook, ProviderCallContext, ProviderResponseContext,
};
use cherub::runtime::output::{OutputEvent, OutputSink};
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

fn end_turn() -> Message {
    Message::Assistant {
        content: vec![ContentBlock::Text {
            text: String::new(),
        }],
        stop_reason: StopReason::EndTurn,
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _system: &str,
        messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        let mut queue = self.responses.lock().unwrap();
        // Capture the last user message for inspection by tests.
        let _ = messages;
        Ok((queue.pop_front().unwrap_or_else(end_turn), None))
    }

    fn model_name(&self) -> &str {
        "mock"
    }

    fn max_output_tokens(&self) -> u32 {
        4096
    }
}

/// Mock provider that captures messages sent to it (for verifying hook transforms).
struct CapturingProvider {
    responses: Mutex<VecDeque<Message>>,
    captured_messages: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl CapturingProvider {
    fn new(responses: Vec<Message>, captured: Arc<Mutex<Vec<Vec<Message>>>>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            captured_messages: captured,
        }
    }
}

#[async_trait]
impl Provider for CapturingProvider {
    async fn complete(
        &self,
        _system: &str,
        messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        self.captured_messages
            .lock()
            .unwrap()
            .push(messages.to_vec());
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

// ---------------------------------------------------------------------------
// Mock Approval Gate
// ---------------------------------------------------------------------------

struct MockApprovalGate;

impl ApprovalGate for MockApprovalGate {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        ApprovalResult::Denied
    }
}

// ---------------------------------------------------------------------------
// Null Sink (no-op output)
// ---------------------------------------------------------------------------

struct NullSink;

impl OutputSink for NullSink {
    async fn emit(&self, _event: OutputEvent<'_>) {}
}

// ---------------------------------------------------------------------------
// Recording Sink (captures session messages for verification)
// ---------------------------------------------------------------------------

struct RecordingSink {
    events: Arc<Mutex<Vec<String>>>,
}

impl OutputSink for RecordingSink {
    async fn emit(&self, event: OutputEvent<'_>) {
        let desc = match event {
            OutputEvent::ToolOutput(s) => format!("ToolOutput:{s}"),
            OutputEvent::ToolError(s) => format!("ToolError:{s}"),
            OutputEvent::ToolAllowed { tool, command } => format!("Allowed:{tool}:{command}"),
            OutputEvent::ToolRejected { tool, command } => format!("Rejected:{tool}:{command}"),
            _ => return,
        };
        self.events.lock().unwrap().push(desc);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const DEFAULT_POLICY: &str = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls ", "^cat ", "^echo "]

[tools.bash.actions.write]
tier = "act"
patterns = ["^mkdir "]
"#;

fn make_agent(responses: Vec<Message>) -> AgentLoop<MockApprovalGate, NullSink> {
    let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
    let provider = MockProvider::new(responses);
    let registry = ToolRegistry::new();
    AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "test prompt".to_owned(),
        MockApprovalGate,
        NullSink,
        "test",
    )
}

fn make_agent_with_sink(
    responses: Vec<Message>,
    sink: RecordingSink,
) -> AgentLoop<MockApprovalGate, RecordingSink> {
    let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
    let provider = MockProvider::new(responses);
    let registry = ToolRegistry::new();
    AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "test prompt".to_owned(),
        MockApprovalGate,
        sink,
        "test",
    )
}

fn make_capturing_agent(
    responses: Vec<Message>,
    captured: Arc<Mutex<Vec<Vec<Message>>>>,
) -> AgentLoop<MockApprovalGate, NullSink> {
    let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
    let provider = CapturingProvider::new(responses, captured);
    let registry = ToolRegistry::new();
    AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "test prompt".to_owned(),
        MockApprovalGate,
        NullSink,
        "test",
    )
}

// ---------------------------------------------------------------------------
// Hook implementations for tests
// ---------------------------------------------------------------------------

/// Appends to a shared vec to verify ordering.
struct OrderingHook {
    label: String,
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Hook for OrderingHook {
    async fn before_inbound(&self, _ctx: &mut InboundContext<'_>) -> HookResult {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:before_inbound", self.label));
        Ok(())
    }

    async fn before_provider_call(&self, _ctx: &ProviderCallContext<'_>) -> HookResult {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:before_provider_call", self.label));
        Ok(())
    }

    async fn after_provider_call(&self, _ctx: &ProviderResponseContext<'_>) -> HookResult {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:after_provider_call", self.label));
        Ok(())
    }

    async fn before_tool_call(&self, _ctx: &BeforeToolCallContext<'_>) -> HookResult {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:before_tool_call", self.label));
        Ok(())
    }

    async fn after_tool_call(&self, _ctx: &mut AfterToolCallContext<'_>) -> HookResult {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:after_tool_call", self.label));
        Ok(())
    }

    async fn before_compaction(&self, _ctx: &CompactionContext<'_>) -> HookResult {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:before_compaction", self.label));
        Ok(())
    }
}

/// Uppercases all text content in the inbound message.
struct UppercaseHook;

#[async_trait]
impl Hook for UppercaseHook {
    async fn before_inbound(&self, ctx: &mut InboundContext<'_>) -> HookResult {
        for item in ctx.content.iter_mut() {
            if let UserContent::Text(text) = item {
                *text = text.to_uppercase();
            }
        }
        Ok(())
    }
}

/// Always returns an error.
struct FailingHook;

#[async_trait]
impl Hook for FailingHook {
    async fn before_inbound(&self, _ctx: &mut InboundContext<'_>) -> HookResult {
        Err("intentional hook failure".into())
    }

    async fn before_provider_call(&self, _ctx: &ProviderCallContext<'_>) -> HookResult {
        Err("intentional hook failure".into())
    }

    async fn after_provider_call(&self, _ctx: &ProviderResponseContext<'_>) -> HookResult {
        Err("intentional hook failure".into())
    }

    async fn before_tool_call(&self, _ctx: &BeforeToolCallContext<'_>) -> HookResult {
        Err("intentional hook failure".into())
    }

    async fn after_tool_call(&self, _ctx: &mut AfterToolCallContext<'_>) -> HookResult {
        Err("intentional hook failure".into())
    }

    async fn before_compaction(&self, _ctx: &CompactionContext<'_>) -> HookResult {
        Err("intentional hook failure".into())
    }
}

/// Appends a suffix to tool output.
struct SuffixHook {
    suffix: String,
}

#[async_trait]
impl Hook for SuffixHook {
    async fn after_tool_call(&self, ctx: &mut AfterToolCallContext<'_>) -> HookResult {
        ctx.result.push_str(&self.suffix);
        Ok(())
    }
}

/// Records tool names from before_tool_call.
struct ToolCallRecorder {
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Hook for ToolCallRecorder {
    async fn before_tool_call(&self, ctx: &BeforeToolCallContext<'_>) -> HookResult {
        self.log
            .lock()
            .unwrap()
            .push(format!("before_tool:{}", ctx.tool));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hook_ordering_sequential() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let response = Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "done".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    };

    let mut agent = make_agent(vec![response]);
    agent.with_hook(Box::new(OrderingHook {
        label: "A".to_owned(),
        log: Arc::clone(&log),
    }));
    agent.with_hook(Box::new(OrderingHook {
        label: "B".to_owned(),
        log: Arc::clone(&log),
    }));
    agent.run_turn_text("test").await.unwrap();

    let entries = log.lock().unwrap();
    // Both hooks should fire before_inbound in order: A then B.
    let inbound_indices: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, s)| s.contains("before_inbound"))
        .map(|(i, s)| (i, s.clone()))
        .collect();
    assert_eq!(inbound_indices.len(), 2);
    assert!(inbound_indices[0].1.starts_with("A:"));
    assert!(inbound_indices[1].1.starts_with("B:"));
}

#[tokio::test]
async fn hook_before_inbound_modifies_content() {
    let captured: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));

    let response = Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "done".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    };

    let mut agent = make_capturing_agent(vec![response], Arc::clone(&captured));
    agent.with_hook(Box::new(UppercaseHook));
    agent.run_turn_text("hello world").await.unwrap();

    let calls = captured.lock().unwrap();
    assert!(!calls.is_empty());
    // The first message sent to the provider should contain the uppercased content.
    let first_call_messages = &calls[0];
    let user_msg = &first_call_messages[0];
    match user_msg {
        Message::User { content } => {
            if let UserContent::Text(text) = &content[0] {
                assert_eq!(text, "HELLO WORLD");
            } else {
                panic!("expected text content");
            }
        }
        _ => panic!("expected user message"),
    }
}

#[tokio::test]
async fn hook_error_does_not_block_agent() {
    let response = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "1".to_owned(),
            name: "bash".to_owned(),
            input: json!({"command": "echo hello"}),
        }],
        stop_reason: StopReason::ToolUse,
    };

    let mut agent = make_agent(vec![response, end_turn()]);
    agent.with_hook(Box::new(FailingHook));
    // Agent should complete despite hook failures.
    agent.run_turn_text("test").await.unwrap();
}

#[tokio::test]
async fn hook_after_tool_call_transforms_output() {
    let response = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "1".to_owned(),
            name: "bash".to_owned(),
            input: json!({"command": "echo hello"}),
        }],
        stop_reason: StopReason::ToolUse,
    };

    let mut agent = make_agent(vec![response, end_turn()]);
    agent.with_hook(Box::new(SuffixHook {
        suffix: " [TRANSFORMED]".to_owned(),
    }));
    agent.run_turn_text("test").await.unwrap();

    // The last ToolResult message in the session should have the suffix.
    let messages = agent.session_messages();
    let tool_result = messages
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }))
        .expect("should have a tool result");
    match tool_result {
        Message::ToolResult { content, .. } => {
            assert!(
                content.ends_with(" [TRANSFORMED]"),
                "output should have suffix, got: {content}"
            );
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn hook_before_tool_call_fires_for_allowed() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let response = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "1".to_owned(),
            name: "bash".to_owned(),
            input: json!({"command": "echo hello"}),
        }],
        stop_reason: StopReason::ToolUse,
    };

    let mut agent = make_agent(vec![response, end_turn()]);
    agent.with_hook(Box::new(ToolCallRecorder {
        log: Arc::clone(&log),
    }));
    agent.run_turn_text("test").await.unwrap();

    let entries = log.lock().unwrap();
    assert!(
        entries.iter().any(|s| s == "before_tool:bash"),
        "before_tool_call should fire for allowed tool"
    );
}

#[tokio::test]
async fn hook_before_tool_call_skipped_for_rejected() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // Use a command that doesn't match any policy patterns → rejected.
    let response = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "1".to_owned(),
            name: "bash".to_owned(),
            input: json!({"command": "rm -rf /"}),
        }],
        stop_reason: StopReason::ToolUse,
    };

    let mut agent = make_agent(vec![response, end_turn()]);
    agent.with_hook(Box::new(ToolCallRecorder {
        log: Arc::clone(&log),
    }));
    agent.run_turn_text("test").await.unwrap();

    let entries = log.lock().unwrap();
    assert!(
        !entries.iter().any(|s| s.contains("before_tool")),
        "before_tool_call should NOT fire for rejected tool"
    );
}

#[tokio::test]
async fn hook_no_hooks_no_overhead() {
    let response = Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "done".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    };

    // Agent with no hooks should work fine.
    let mut agent = make_agent(vec![response]);
    agent.run_turn_text("test").await.unwrap();
}

#[tokio::test]
async fn stash_hook_integration() {
    let dir = tempfile::tempdir().unwrap();

    let response = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "1".to_owned(),
            name: "bash".to_owned(),
            input: json!({"command": "echo hello"}),
        }],
        stop_reason: StopReason::ToolUse,
    };

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = RecordingSink {
        events: Arc::clone(&events),
    };
    let mut agent = make_agent_with_sink(vec![response, end_turn()], sink);
    // Use a very low threshold so the echo output exceeds it.
    agent.with_hook(Box::new(
        OutputStashingHook::new(dir.path()).with_threshold(1),
    ));
    agent.run_turn_text("test").await.unwrap();

    // Verify the session has a stash reference.
    let messages = agent.session_messages();
    let tool_result = messages
        .iter()
        .find(|m| {
            matches!(
                m,
                Message::ToolResult {
                    is_error: false,
                    ..
                }
            )
        })
        .expect("should have a tool result");
    match tool_result {
        Message::ToolResult { content, .. } => {
            assert!(
                content.contains("[Output stashed:"),
                "should contain stash notice, got: {content}"
            );
            assert!(content.contains(".cherub/stash/"));
        }
        _ => unreachable!(),
    }

    // Verify the stash file exists.
    assert!(
        dir.path().join(".cherub/stash").exists(),
        "stash directory should exist"
    );
    let stash_files: Vec<_> = std::fs::read_dir(dir.path().join(".cherub/stash"))
        .unwrap()
        .collect();
    assert_eq!(stash_files.len(), 1, "should have exactly one stash file");
}
