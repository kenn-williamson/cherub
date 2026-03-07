//! Integration tests for M14b/c/d output event behavior.
//!
//! Uses a RecordingSink to verify which OutputEvents are emitted by the agent
//! loop. No API key required — runs as `cargo test`.

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde_json::json;

use async_trait::async_trait;

use cherub::enforcement::policy::Policy;
use cherub::error::CherubError;
use cherub::providers::{ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition};
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
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
// Recording Sink
// ---------------------------------------------------------------------------

/// Recorded variant of OutputEvent (owned strings).
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum RecordedEvent {
    Text(String),
    Recapitulation(String),
    ToolAllowed { tool: String, command: String },
    ToolRejected { tool: String, command: String },
    Warning(String),
    Thinking(String),
    Progress { tool: String, status: String },
    ProgressClear,
    TurnStart,
    TurnEnd,
}

/// Shared event log for the recording sink.
type EventLog = Arc<Mutex<Vec<RecordedEvent>>>;

struct RecordingSink {
    events: EventLog,
}

impl RecordingSink {
    fn new(events: EventLog) -> Self {
        Self { events }
    }
}

impl OutputSink for RecordingSink {
    async fn emit(&self, event: OutputEvent<'_>) {
        let recorded = match event {
            OutputEvent::Text(t) => RecordedEvent::Text(t.to_owned()),
            OutputEvent::Recapitulation(t) => RecordedEvent::Recapitulation(t.to_owned()),
            OutputEvent::ToolAllowed { tool, command } => RecordedEvent::ToolAllowed {
                tool: tool.to_owned(),
                command: command.to_owned(),
            },
            OutputEvent::ToolRejected { tool, command } => RecordedEvent::ToolRejected {
                tool: tool.to_owned(),
                command: command.to_owned(),
            },
            OutputEvent::ToolApproved { .. } | OutputEvent::ToolDenied { .. } => return,
            OutputEvent::ToolOutput(_) | OutputEvent::ToolError(_) => return,
            OutputEvent::Warning(m) => RecordedEvent::Warning(m.to_owned()),
            OutputEvent::Thinking(t) => RecordedEvent::Thinking(t.to_owned()),
            OutputEvent::Progress { tool, status } => RecordedEvent::Progress {
                tool: tool.to_owned(),
                status: status.to_owned(),
            },
            OutputEvent::ProgressClear => RecordedEvent::ProgressClear,
        };
        self.events.lock().unwrap().push(recorded);
    }

    async fn turn_start(&self) {
        self.events.lock().unwrap().push(RecordedEvent::TurnStart);
    }

    async fn turn_end(&self) {
        self.events.lock().unwrap().push(RecordedEvent::TurnEnd);
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

fn make_agent(
    responses: Vec<Message>,
    events: EventLog,
) -> AgentLoop<MockApprovalGate, RecordingSink> {
    let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
    let provider = MockProvider::new(responses);
    let registry = ToolRegistry::new();
    AgentLoop::new(
        policy,
        Box::new(provider),
        registry,
        "test prompt".to_owned(),
        MockApprovalGate,
        RecordingSink::new(events),
        "test",
    )
}

// ---------------------------------------------------------------------------
// M14b: Recapitulation tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_text_before_tool_use_is_recapitulation() {
    let response = Message::Assistant {
        content: vec![
            ContentBlock::Text {
                text: "I'll list the files for you.".to_owned(),
            },
            ContentBlock::ToolUse {
                id: "1".to_owned(),
                name: "bash".to_owned(),
                input: json!({"command": "ls src/"}),
            },
        ],
        stop_reason: StopReason::ToolUse,
    };

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let mut agent = make_agent(vec![response, end_turn()], Arc::clone(&events));
    agent.run_turn_text("list files").await.unwrap();

    let log = events.lock().unwrap();
    let recap = log
        .iter()
        .find(|e| matches!(e, RecordedEvent::Recapitulation(_)));
    assert!(recap.is_some(), "should emit Recapitulation");
    if let RecordedEvent::Recapitulation(text) = recap.unwrap() {
        assert_eq!(text, "I'll list the files for you.");
    }
}

#[tokio::test]
async fn text_only_response_first_is_recapitulation() {
    let response = Message::Assistant {
        content: vec![
            ContentBlock::Text {
                text: "Hello!".to_owned(),
            },
            ContentBlock::Text {
                text: "How can I help?".to_owned(),
            },
        ],
        stop_reason: StopReason::EndTurn,
    };

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let mut agent = make_agent(vec![response], Arc::clone(&events));
    agent.run_turn_text("hi").await.unwrap();

    let log = events.lock().unwrap();
    let recaps: Vec<_> = log
        .iter()
        .filter(|e| matches!(e, RecordedEvent::Recapitulation(_)))
        .collect();
    let texts: Vec<_> = log
        .iter()
        .filter(|e| matches!(e, RecordedEvent::Text(_)))
        .collect();
    assert_eq!(recaps.len(), 1, "first text should be recapitulation");
    assert_eq!(texts.len(), 1, "second text should be regular text");
}

#[tokio::test]
async fn tool_use_only_no_recapitulation() {
    let response = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "1".to_owned(),
            name: "bash".to_owned(),
            input: json!({"command": "ls src/"}),
        }],
        stop_reason: StopReason::ToolUse,
    };

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let mut agent = make_agent(vec![response, end_turn()], Arc::clone(&events));
    agent.run_turn_text("test").await.unwrap();

    let log = events.lock().unwrap();
    let recaps: Vec<_> = log
        .iter()
        .filter(|e| matches!(e, RecordedEvent::Recapitulation(_)))
        .collect();
    assert!(recaps.is_empty(), "no text means no recapitulation");
}

#[tokio::test]
async fn text_after_tool_use_is_regular_text() {
    let response = Message::Assistant {
        content: vec![
            ContentBlock::ToolUse {
                id: "1".to_owned(),
                name: "bash".to_owned(),
                input: json!({"command": "ls src/"}),
            },
            ContentBlock::Text {
                text: "Here are the results.".to_owned(),
            },
        ],
        stop_reason: StopReason::ToolUse,
    };

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let mut agent = make_agent(vec![response, end_turn()], Arc::clone(&events));
    agent.run_turn_text("test").await.unwrap();

    let log = events.lock().unwrap();
    let recaps: Vec<_> = log
        .iter()
        .filter(|e| matches!(e, RecordedEvent::Recapitulation(_)))
        .collect();
    let texts: Vec<_> = log
        .iter()
        .filter(|e| matches!(e, RecordedEvent::Text(_)))
        .collect();
    assert!(
        recaps.is_empty(),
        "text after tool_use is not recapitulation"
    );
    assert_eq!(texts.len(), 1);
}

// ---------------------------------------------------------------------------
// M14c: Turn lifecycle tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn turn_lifecycle_calls_start_and_end() {
    let response = Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "Done.".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    };

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let mut agent = make_agent(vec![response], Arc::clone(&events));
    agent.run_turn_text("test").await.unwrap();

    let log = events.lock().unwrap();
    assert!(
        matches!(log.first(), Some(RecordedEvent::TurnStart)),
        "first event should be TurnStart"
    );
    assert!(
        matches!(log.last(), Some(RecordedEvent::TurnEnd)),
        "last event should be TurnEnd"
    );
}

#[tokio::test]
async fn turn_end_called_on_early_return() {
    let response = Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "Simple answer.".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    };

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let mut agent = make_agent(vec![response], Arc::clone(&events));
    agent.run_turn_text("question").await.unwrap();

    let log = events.lock().unwrap();
    let starts = log
        .iter()
        .filter(|e| matches!(e, RecordedEvent::TurnStart))
        .count();
    let ends = log
        .iter()
        .filter(|e| matches!(e, RecordedEvent::TurnEnd))
        .count();
    assert_eq!(starts, 1);
    assert_eq!(ends, 1);
}

#[tokio::test]
async fn progress_emitted_during_tool_execution() {
    let response = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "1".to_owned(),
            name: "bash".to_owned(),
            input: json!({"command": "echo hello"}),
        }],
        stop_reason: StopReason::ToolUse,
    };

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let mut agent = make_agent(vec![response, end_turn()], Arc::clone(&events));
    agent.run_turn_text("test").await.unwrap();

    let log = events.lock().unwrap();
    let progress: Vec<_> = log
        .iter()
        .filter(|e| matches!(e, RecordedEvent::Progress { .. }))
        .collect();
    let clears: Vec<_> = log
        .iter()
        .filter(|e| matches!(e, RecordedEvent::ProgressClear))
        .collect();

    assert!(
        !progress.is_empty(),
        "should emit at least one Progress event"
    );
    assert!(
        !clears.is_empty(),
        "should emit ProgressClear after tool execution"
    );
}
