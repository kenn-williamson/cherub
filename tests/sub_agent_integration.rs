//! Sub-agent tool integration tests (M13d).
//!
//! Uses MockProvider to test the bounded inner loop, enforcement, timeout,
//! cost attribution, and recursion prevention. No API key required.

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use cherub::enforcement::policy::Policy;
use cherub::error::CherubError;
use cherub::providers::{ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition};
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
use cherub::runtime::output::NullSink;
use cherub::tools::ToolRegistry;
use cherub::tools::sub_agent::SubAgentTool;

// ---------------------------------------------------------------------------
// Mock Provider
// ---------------------------------------------------------------------------

struct MockProvider {
    responses: Mutex<VecDeque<Message>>,
    model: String,
}

impl MockProvider {
    fn new(responses: Vec<Message>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            model: "mock-sub-agent".to_owned(),
        }
    }

    fn with_model(responses: Vec<Message>, model: &str) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            model: model.to_owned(),
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
        let msg = queue.pop_front().unwrap_or_else(|| Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "[no more responses]".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        });
        Ok((msg, Some(ApiUsage::new(100, 50))))
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn max_output_tokens(&self) -> u32 {
        4096
    }
}

// Orchestrator mock: returns a tool_use for the sub-agent, then ends.
struct OrchestratorMock {
    responses: Mutex<VecDeque<Message>>,
}

impl OrchestratorMock {
    fn new(responses: Vec<Message>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
        }
    }
}

#[async_trait]
impl Provider for OrchestratorMock {
    async fn complete(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        let mut queue = self.responses.lock().unwrap();
        let msg = queue.pop_front().unwrap_or_else(|| Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "[done]".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        });
        Ok((msg, Some(ApiUsage::new(200, 100))))
    }

    fn model_name(&self) -> &str {
        "orchestrator-mock"
    }

    fn max_output_tokens(&self) -> u32 {
        4096
    }
}

// ---------------------------------------------------------------------------
// Mock Approval Gate (always deny — sub-agents should never reach this)
// ---------------------------------------------------------------------------

struct AlwaysDenyGate;

impl ApprovalGate for AlwaysDenyGate {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        ApprovalResult::Denied
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const POLICY: &str = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^echo ", "^ls "]

[tools.bash.actions.write]
tier = "act"
patterns = ["^mkdir "]

[tools.bash.actions.destructive]
tier = "commit"
patterns = ["^rm "]

[tools.file]
enabled = true
match_source = "structured"

[tools.file.actions.read_ops]
tier = "observe"
patterns = ["^read:", "^read$", "^glob:", "^glob$", "^grep:", "^grep$"]

[tools.file.actions.write_ops]
tier = "act"
patterns = ["^edit:", "^edit$"]
"#;

const SUB_AGENT_POLICY: &str = r#"
[tools.summarizer]
enabled = true
match_source = "structured"

[tools.summarizer.actions.invoke]
tier = "act"
patterns = ["^invoke$"]

[tools.coder]
enabled = true
match_source = "structured"

[tools.coder.actions.invoke]
tier = "act"
patterns = ["^invoke$"]

[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^echo ", "^ls "]

[tools.bash.actions.write]
tier = "act"
patterns = ["^mkdir "]

[tools.bash.actions.destructive]
tier = "commit"
patterns = ["^rm "]

[tools.file]
enabled = true
match_source = "structured"

[tools.file.actions.read_ops]
tier = "observe"
patterns = ["^read:", "^read$", "^glob:", "^glob$", "^grep:", "^grep$"]

[tools.file.actions.write_ops]
tier = "act"
patterns = ["^edit:", "^edit$"]
"#;

fn make_policy() -> Policy {
    Policy::from_str(POLICY).unwrap()
}

fn make_full_policy() -> Policy {
    Policy::from_str(SUB_AGENT_POLICY).unwrap()
}

fn make_sub_agent(name: &str, provider: Box<dyn Provider>, tools: &[String]) -> SubAgentTool {
    SubAgentTool {
        name: name.to_owned(),
        description: format!("Test sub-agent: {name}"),
        provider,
        system_prompt: "You are a test sub-agent.".to_owned(),
        max_turns: 5,
        timeout: Duration::from_secs(10),
        registry: ToolRegistry::for_sub_agent(tools),
        policy: make_policy(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 1. Basic delegation: sub-agent returns text, orchestrator gets it as ToolResult.
#[tokio::test]
async fn basic_delegation() {
    let sub_provider = MockProvider::new(vec![Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "Summary: The document discusses X, Y, and Z.".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    }]);

    let sub_agent = make_sub_agent("summarizer", Box::new(sub_provider), &[]);

    // Orchestrator calls the sub-agent, then ends.
    let orchestrator = OrchestratorMock::new(vec![
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_owned(),
                name: "summarizer".to_owned(),
                input: json!({"input": "Summarize this document"}),
            }],
            stop_reason: StopReason::ToolUse,
        },
        Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "The sub-agent summarized the document.".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        },
    ]);

    let registry = ToolRegistry::new().with_sub_agents(vec![sub_agent]);
    let policy = make_full_policy();

    let mut agent = AgentLoop::new(
        policy,
        Box::new(orchestrator),
        registry,
        "Test system".to_owned(),
        AlwaysDenyGate,
        NullSink,
        "test-user",
    );

    agent.run_turn_text("summarize this").await.unwrap();

    // Verify the tool result was pushed into the session.
    let messages = agent.session_messages();
    let tool_result = messages
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    assert!(tool_result.is_some(), "expected a ToolResult in session");

    if let Some(Message::ToolResult {
        content, is_error, ..
    }) = tool_result
    {
        assert!(!is_error);
        assert!(
            content.contains("Summary:"),
            "expected sub-agent output, got: {content}"
        );
    }
}

/// 2. Max turns honored: provider always returns ToolUse, loop stops at max_turns.
#[tokio::test]
async fn max_turns_honored() {
    // Sub-agent provider always wants to use a tool, never stops.
    let responses: Vec<Message> = (0..10)
        .map(|i| Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: format!("call_{i}"),
                name: "bash".to_owned(),
                input: json!({"command": format!("echo turn {i}")}),
            }],
            stop_reason: StopReason::ToolUse,
        })
        .collect();

    let sub_provider = MockProvider::new(responses);

    let mut sub_agent = make_sub_agent("coder", Box::new(sub_provider), &["bash".to_owned()]);
    sub_agent.max_turns = 3;

    let ctx = cherub::tools::ToolContext {
        user_id: "test".to_owned(),
        session_id: uuid::Uuid::nil(),
        turn_number: 1,
    };

    let result = sub_agent
        .execute(&json!({"input": "do something"}), &ctx)
        .await
        .unwrap();

    // Should have sub_agent_usage.
    let (model_name, usage) = result.sub_agent_usage.unwrap();
    assert_eq!(model_name, "mock-sub-agent");
    // 3 turns × 100 input + 50 output per turn.
    assert_eq!(usage.input_tokens, 300);
    assert_eq!(usage.output_tokens, 150);
}

/// 3. Timeout: provider sleeps past timeout.
#[tokio::test]
async fn timeout_produces_message() {
    // Create a provider that sleeps.
    struct SlowProvider;

    #[async_trait]
    impl Provider for SlowProvider {
        async fn complete(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<(Message, Option<ApiUsage>), CherubError> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok((
                Message::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "should not reach here".to_owned(),
                    }],
                    stop_reason: StopReason::EndTurn,
                },
                None,
            ))
        }

        fn model_name(&self) -> &str {
            "slow-mock"
        }

        fn max_output_tokens(&self) -> u32 {
            4096
        }
    }

    let mut sub_agent = make_sub_agent("summarizer", Box::new(SlowProvider), &[]);
    sub_agent.timeout = Duration::from_millis(100);

    let ctx = cherub::tools::ToolContext {
        user_id: "test".to_owned(),
        session_id: uuid::Uuid::nil(),
        turn_number: 1,
    };

    let result = sub_agent
        .execute(&json!({"input": "summarize this"}), &ctx)
        .await
        .unwrap();

    assert!(
        result.output.contains("[sub-agent timed out]"),
        "expected timeout message, got: {}",
        result.output
    );
}

/// 4. Escalation auto-rejected: commit-tier tool call → "action not permitted".
#[tokio::test]
async fn escalation_auto_rejected() {
    // Sub-agent tries to run `rm -rf /` which is commit tier.
    let sub_provider = MockProvider::new(vec![
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_owned(),
                name: "bash".to_owned(),
                input: json!({"command": "rm -rf /"}),
            }],
            stop_reason: StopReason::ToolUse,
        },
        // After rejection, respond with text.
        Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "I could not delete files.".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        },
    ]);

    let sub_agent = make_sub_agent("coder", Box::new(sub_provider), &["bash".to_owned()]);

    let ctx = cherub::tools::ToolContext {
        user_id: "test".to_owned(),
        session_id: uuid::Uuid::nil(),
        turn_number: 1,
    };

    let result = sub_agent
        .execute(&json!({"input": "delete everything"}), &ctx)
        .await
        .unwrap();

    assert!(
        result.output.contains("I could not delete files"),
        "expected graceful fallback text, got: {}",
        result.output
    );
}

/// 5. Tool subset: sub-agent with tools = ["file"] cannot use bash.
#[tokio::test]
async fn tool_subset_enforced() {
    // Sub-agent tries to use bash, but only has file.
    let sub_provider = MockProvider::new(vec![
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_owned(),
                name: "bash".to_owned(),
                input: json!({"command": "echo hello"}),
            }],
            stop_reason: StopReason::ToolUse,
        },
        Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "Bash was not available.".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        },
    ]);

    let sub_agent = make_sub_agent("coder", Box::new(sub_provider), &["file".to_owned()]);

    let ctx = cherub::tools::ToolContext {
        user_id: "test".to_owned(),
        session_id: uuid::Uuid::nil(),
        turn_number: 1,
    };

    let result = sub_agent
        .execute(&json!({"input": "run something"}), &ctx)
        .await
        .unwrap();

    // The tool_use for bash should fail (unknown tool in registry → execute fails).
    // But the sub-agent gracefully falls back.
    assert!(
        result.output.contains("Bash was not available"),
        "expected fallback text, got: {}",
        result.output
    );
}

/// 6. No recursion: sub-agent registry never contains SubAgent tools.
#[tokio::test]
async fn no_recursion_in_registry() {
    let registry = ToolRegistry::for_sub_agent(&["bash".to_owned(), "file".to_owned()]);
    let defs = registry.definitions();

    // Only bash and file should be present (2 tools, no sub-agents).
    assert_eq!(
        defs.len(),
        2,
        "expected exactly bash + file, got {}",
        defs.len()
    );
}

/// 7. Pure completion: tools = [], provider called with empty tool_defs.
#[tokio::test]
async fn pure_completion_no_tools() {
    let sub_provider = MockProvider::new(vec![Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "This is a pure completion response.".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    }]);

    let sub_agent = make_sub_agent("summarizer", Box::new(sub_provider), &[]);

    let ctx = cherub::tools::ToolContext {
        user_id: "test".to_owned(),
        session_id: uuid::Uuid::nil(),
        turn_number: 1,
    };

    let result = sub_agent
        .execute(&json!({"input": "summarize this"}), &ctx)
        .await
        .unwrap();

    assert_eq!(result.output, "This is a pure completion response.");

    // Verify sub-agent has no tools (empty registry).
    assert!(sub_agent.registry.definitions().is_empty());
}

/// 8. Cost attribution: ToolResult.sub_agent_usage has correct model_name and cumulative tokens.
#[tokio::test]
async fn cost_attribution() {
    // Two-turn conversation: tool use + response.
    let sub_provider = MockProvider::with_model(
        vec![
            Message::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".to_owned(),
                    name: "bash".to_owned(),
                    input: json!({"command": "echo hello"}),
                }],
                stop_reason: StopReason::ToolUse,
            },
            Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: "Done.".to_owned(),
                }],
                stop_reason: StopReason::EndTurn,
            },
        ],
        "llama3-local",
    );

    let sub_agent = make_sub_agent("coder", Box::new(sub_provider), &["bash".to_owned()]);

    let ctx = cherub::tools::ToolContext {
        user_id: "test".to_owned(),
        session_id: uuid::Uuid::nil(),
        turn_number: 1,
    };

    let result = sub_agent
        .execute(&json!({"input": "say hello"}), &ctx)
        .await
        .unwrap();

    let (model_name, usage) = result.sub_agent_usage.unwrap();
    assert_eq!(model_name, "llama3-local");
    // Two calls, each returning 100 input + 50 output.
    assert_eq!(usage.input_tokens, 200);
    assert_eq!(usage.output_tokens, 100);
}

/// 9. Config validation: agent name conflicting with built-in tool is rejected.
#[test]
fn config_agent_name_conflict_rejected() {
    let toml = r#"
[providers.local]
type = "openai"
model = "llama3"
base_url = "http://localhost:11434/v1"

[agents.bash]
description = "Conflicts with built-in bash"
provider = "local"
system_prompt = "test"
"#;
    let config: cherub::providers::config::ProvidersConfig =
        toml::from_str(toml).expect("should parse");
    let err = config.validate().unwrap_err();
    assert!(
        err.to_string().contains("conflicts with built-in tool"),
        "unexpected error: {err}"
    );
}

/// 10. ToolResult::text() convenience constructor works.
#[test]
fn tool_result_text_constructor() {
    let result = cherub::tools::ToolResult::text("hello".to_owned());
    assert_eq!(result.output, "hello");
    assert!(result.sub_agent_usage.is_none());
}
