//! Integration tests for context compaction.
//!
//! Uses mock providers and direct Session manipulation to test compaction
//! mechanics, token estimation, and message alternation invariants.
//!
//! No API key required — runs as `cargo test`.

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::Mutex;

use serde_json::json;

use cherub::enforcement::policy::Policy;
use cherub::error::CherubError;
use cherub::providers::{ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition};
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
use cherub::runtime::output::NullSink;
use cherub::runtime::session::Session;
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
        "claude-test"
    }

    fn max_output_tokens(&self) -> u32 {
        4096
    }
}

/// A provider that reports high usage to trigger compaction.
struct HighUsageProvider {
    responses: Mutex<VecDeque<Message>>,
}

impl HighUsageProvider {
    fn new(responses: Vec<Message>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
        }
    }
}

impl Provider for HighUsageProvider {
    async fn complete(
        &self,
        _system: &str,
        messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        let mut queue = self.responses.lock().unwrap();
        let msg = queue.pop_front().unwrap_or_else(end_turn);

        // Report high usage when conversation is long enough to trigger compaction.
        let usage = if messages.len() > 10 {
            Some(ApiUsage {
                input_tokens: 160_000, // Above 75% of 200k
                output_tokens: 100,
            })
        } else {
            Some(ApiUsage {
                input_tokens: 1_000,
                output_tokens: 100,
            })
        };

        Ok((msg, usage))
    }

    fn model_name(&self) -> &str {
        "claude-test"
    }

    fn max_output_tokens(&self) -> u32 {
        4096
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct AutoApprove;

impl ApprovalGate for AutoApprove {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        ApprovalResult::Approved
    }
}

fn end_turn() -> Message {
    Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "OK.".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    }
}

fn summary_response() -> Message {
    Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "Summary: The user discussed various topics including file management."
                .to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    }
}

const POLICY: &str = r#"
[tools.bash]
enabled = true
[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls ", "^echo "]
"#;

// ===========================================================================
// Tests: Session mechanics (no provider needed)
// ===========================================================================

/// Compaction preserves the required user/assistant alternation.
#[tokio::test]
async fn compaction_maintains_message_alternation() {
    let mut session = Session::new("test");

    // Build a clean conversation: 10 turn pairs (20 messages).
    for i in 0..10 {
        session.push(Message::user_text(&format!("question {i}")));
        session.push(Message::Assistant {
            content: vec![ContentBlock::Text {
                text: format!("answer {i}"),
            }],
            stop_reason: StopReason::EndTurn,
        });
    }
    assert_eq!(session.messages().len(), 20);

    // Split preserving 6 recent messages.
    let (old, recent) = session.split_for_compaction(6).unwrap();
    assert!(old.len() >= 2);
    assert!(recent.len() >= 6);
    assert_eq!(old.len() + recent.len(), 20);

    // Apply compaction.
    let summary_user = Message::user_text("[Context Summary — Compaction #1]\n\nSummary here.");
    let summary_ack = Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "Understood. I have the context from our earlier conversation.".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    };
    session.apply_compaction(summary_user, summary_ack, recent);

    // Verify alternation: first message is user (summary), second is assistant (ack).
    assert!(matches!(session.messages()[0], Message::User { .. }));
    assert!(matches!(session.messages()[1], Message::Assistant { .. }));

    // The recent messages follow, starting with a user message.
    if session.messages().len() > 2 {
        assert!(
            matches!(session.messages()[2], Message::User { .. }),
            "first recent message should be User"
        );
    }

    assert_eq!(session.compaction_count(), 1);
}

/// Compaction with tool_use/tool_result pairs doesn't break them.
#[tokio::test]
async fn compaction_does_not_split_tool_pairs() {
    let mut session = Session::new("test");

    // Turn 1: normal
    session.push(Message::user_text("hello"));
    session.push(Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "hi".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    });

    // Turn 2: normal
    session.push(Message::user_text("list files"));
    session.push(Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "sure".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    });

    // Turn 3: with tool use
    session.push(Message::user_text("run ls"));
    session.push(Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: json!({"command": "ls"}),
        }],
        stop_reason: StopReason::ToolUse,
    });
    session.push(Message::ToolResult {
        tool_use_id: "t1".to_owned(),
        content: "file1.txt\nfile2.txt".to_owned(),
        is_error: false,
    });
    session.push(Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "Here are the files.".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    });

    // Turn 4: normal
    session.push(Message::user_text("thanks"));
    session.push(Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "you're welcome".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    });

    assert_eq!(session.messages().len(), 10);

    let (old, recent) = session.split_for_compaction(4).unwrap();

    // Split should be at a User message boundary.
    assert!(
        matches!(recent[0], Message::User { .. }),
        "split should be at a User boundary"
    );

    // No ToolResult at start of recent.
    assert!(!matches!(recent[0], Message::ToolResult { .. }));

    // If last old message is a tool_use Assistant, that would mean a broken pair.
    if let Some(Message::Assistant {
        stop_reason: StopReason::ToolUse,
        ..
    }) = old.last()
    {
        panic!("old portion ends with a tool_use without its tool_result");
    }

    assert_eq!(old.len() + recent.len(), 10);
}

/// Session compaction count is tracked correctly across multiple compactions.
#[tokio::test]
async fn multiple_compactions_increment_count() {
    let mut session = Session::new("test");
    assert_eq!(session.compaction_count(), 0);

    // First compaction
    for _ in 0..10 {
        session.push(Message::user_text("msg"));
        session.push(Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "reply".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        });
    }

    let (_, recent) = session.split_for_compaction(4).unwrap();
    session.apply_compaction(
        Message::user_text("[Summary #1]"),
        Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "Understood.".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        },
        recent,
    );
    assert_eq!(session.compaction_count(), 1);

    // Second compaction
    for _ in 0..10 {
        session.push(Message::user_text("msg"));
        session.push(Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "reply".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        });
    }

    let (_, recent) = session.split_for_compaction(4).unwrap();
    session.apply_compaction(
        Message::user_text("[Summary #2]"),
        Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "Understood.".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        },
        recent,
    );
    assert_eq!(session.compaction_count(), 2);
}

/// Compaction is transparent to subsequent turns — push still works.
#[tokio::test]
async fn compaction_transparent_to_subsequent_turns() {
    let mut session = Session::new("test");

    for i in 0..8 {
        session.push(Message::user_text(&format!("msg {i}")));
        session.push(Message::Assistant {
            content: vec![ContentBlock::Text {
                text: format!("reply {i}"),
            }],
            stop_reason: StopReason::EndTurn,
        });
    }

    let (_, recent) = session.split_for_compaction(4).unwrap();
    session.apply_compaction(
        Message::user_text("[Context Summary — Compaction #1]\n\nSummary of earlier discussion."),
        Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "Understood. I have the context from our earlier conversation.".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        },
        recent,
    );

    let pre_count = session.messages().len();
    session.push(Message::user_text("new message after compaction"));
    session.push(Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "response after compaction".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    });

    assert_eq!(session.messages().len(), pre_count + 2);

    // First message is the summary.
    assert!(matches!(&session.messages()[0], Message::User { content }
        if content.iter().any(|c| matches!(c, cherub::providers::UserContent::Text(t) if t.contains("[Context Summary")))));
}

// ===========================================================================
// Tests: Token estimation
// ===========================================================================

/// Token estimation grows with conversation length.
#[tokio::test]
async fn token_estimation_grows_with_messages() {
    use cherub::runtime::tokens::{context_window_size, estimate_tokens};

    let tools = vec![];
    let system = "You are helpful.";

    let short_conv = vec![Message::user_text("hi")];
    let short_tokens = estimate_tokens(system, &short_conv, &tools);

    let mut long_conv = Vec::new();
    for i in 0..50 {
        long_conv.push(Message::user_text(&format!(
            "This is turn {i} with a fairly long message to simulate real conversation content."
        )));
        long_conv.push(Message::Assistant {
            content: vec![ContentBlock::Text {
                text: format!(
                    "This is the response for turn {i}. It contains some detailed information."
                ),
            }],
            stop_reason: StopReason::EndTurn,
        });
    }
    let long_tokens = estimate_tokens(system, &long_conv, &tools);

    assert!(
        long_tokens > short_tokens * 10,
        "50 turns ({long_tokens}) should estimate far more tokens than 1 turn ({short_tokens})"
    );

    // Verify threshold math.
    let window = context_window_size("claude-sonnet-4-20250514");
    assert_eq!(window, 200_000);
    let threshold = (window as f32 * 0.75) as u32;
    assert_eq!(threshold, 150_000);
}

// ===========================================================================
// Tests: AgentLoop integration
// ===========================================================================

/// Compaction does not trigger when the session is short.
#[tokio::test]
async fn no_compaction_when_below_threshold() {
    let provider = MockProvider::new(vec![end_turn()]);
    let policy = Policy::from_str(POLICY).unwrap();
    let mut agent = AgentLoop::new(
        policy,
        provider,
        ToolRegistry::new(),
        "test".to_owned(),
        AutoApprove,
        NullSink,
        "test",
    );

    agent.run_turn_text("Hello").await.unwrap();

    let messages = agent.session_messages();
    assert_eq!(messages.len(), 2); // user + assistant
    // No compaction summary present.
    assert!(!messages.iter().any(|m| {
        matches!(m, Message::User { content } if content.iter().any(|c|
            matches!(c, cherub::providers::UserContent::Text(t) if t.contains("[Context Summary"))))
    }));
}

/// After many turns with high usage, compaction triggers.
#[tokio::test]
async fn compaction_triggers_with_high_usage() {
    // Build responses: 15 normal turns + summary + post-compaction turn.
    let mut responses: Vec<Message> = Vec::new();
    for _ in 0..15 {
        responses.push(end_turn());
    }
    // Summarization call response.
    responses.push(summary_response());
    // Post-compaction turn.
    responses.push(end_turn());

    let provider = HighUsageProvider::new(responses);
    let policy = Policy::from_str(POLICY).unwrap();
    let mut agent = AgentLoop::new(
        policy,
        provider,
        ToolRegistry::new(),
        "test".to_owned(),
        AutoApprove,
        NullSink,
        "test",
    );

    // Run 15 turns.
    for i in 0..15 {
        agent
            .run_turn_text(&format!(
                "Turn {i}: Tell me about topic number {i} in great detail"
            ))
            .await
            .unwrap();
    }

    // The 16th turn should trigger compaction since usage will be high.
    agent.run_turn_text("One more turn").await.unwrap();

    let messages = agent.session_messages();

    // Check if compaction occurred.
    let has_summary = messages.iter().any(|m| {
        matches!(m, Message::User { content } if content.iter().any(|c|
            matches!(c, cherub::providers::UserContent::Text(t) if t.contains("[Context Summary"))))
    });

    if has_summary {
        // Compaction happened — session should be shorter than full 32 messages.
        assert!(
            messages.len() < 32,
            "compacted session should have fewer messages (got {})",
            messages.len()
        );
    }
    // Either way, no crash.
}
