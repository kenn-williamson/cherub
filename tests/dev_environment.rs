//! Tests for the dev_environment tool.
//!
//! Unit tests for language validation, image tagging, and enforcement.
//! Docker integration test (ignored by default) builds and verifies a real image.
//!
//! Run:
//!   cargo nextest run --features container --test dev_environment
//!   cargo nextest run --features container --test dev_environment -- --ignored  # Docker e2e

#![cfg(feature = "container")]

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
use cherub::tools::dev_environment::{
    ALLOWED_LANGUAGES, image_tag, parse_languages, validate_languages,
};

// ─── Unit tests: language validation ────────────────────────────────────────

#[test]
fn validate_languages_accepts_known() {
    for lang in ALLOWED_LANGUAGES {
        let langs = vec![lang.to_string()];
        assert!(validate_languages(&langs).is_ok(), "should accept '{lang}'");
    }
}

#[test]
fn validate_languages_accepts_multiple() {
    let langs: Vec<String> = ALLOWED_LANGUAGES.iter().map(|s| s.to_string()).collect();
    assert!(validate_languages(&langs).is_ok());
}

#[test]
fn validate_languages_accepts_empty() {
    assert!(validate_languages(&[]).is_ok());
}

#[test]
fn validate_languages_rejects_unknown() {
    let langs = vec!["python".to_owned()];
    assert!(validate_languages(&langs).is_err());
}

#[test]
fn validate_languages_rejects_mixed() {
    let langs = vec!["rust".to_owned(), "java".to_owned()];
    assert!(validate_languages(&langs).is_err());
}

// ─── Unit tests: image tag generation ───────────────────────────────────────

#[test]
fn image_tag_deterministic() {
    let a = image_tag(&["node".to_owned(), "rust".to_owned()]);
    let b = image_tag(&["rust".to_owned(), "node".to_owned()]);
    assert_eq!(a, b, "order should not matter");
    assert_eq!(a, "cherub-sandbox-bash:node-rust");
}

#[test]
fn image_tag_empty_is_base() {
    assert_eq!(image_tag(&[]), "cherub-sandbox-bash:base");
}

#[test]
fn image_tag_single() {
    assert_eq!(image_tag(&["go".to_owned()]), "cherub-sandbox-bash:go");
}

#[test]
fn image_tag_all_languages() {
    let all: Vec<String> = ALLOWED_LANGUAGES.iter().map(|s| s.to_string()).collect();
    assert_eq!(image_tag(&all), "cherub-sandbox-bash:go-node-rust");
}

#[test]
fn image_tag_deduplicates() {
    let langs = vec!["rust".to_owned(), "rust".to_owned()];
    assert_eq!(image_tag(&langs), "cherub-sandbox-bash:rust");
}

// ─── Unit tests: parse_languages ────────────────────────────────────────────

#[test]
fn parse_languages_from_array() {
    let params = json!({"action": "setup", "languages": ["rust", "node"]});
    let langs = parse_languages(&params).unwrap();
    assert_eq!(langs, vec!["rust", "node"]);
}

#[test]
fn parse_languages_empty_array() {
    let params = json!({"action": "setup", "languages": []});
    let langs = parse_languages(&params).unwrap();
    assert!(langs.is_empty());
}

#[test]
fn parse_languages_missing_field() {
    let params = json!({"action": "setup"});
    let langs = parse_languages(&params).unwrap();
    assert!(langs.is_empty());
}

#[test]
fn parse_languages_null_field() {
    let params = json!({"action": "setup", "languages": null});
    let langs = parse_languages(&params).unwrap();
    assert!(langs.is_empty());
}

#[test]
fn parse_languages_non_array_errors() {
    let params = json!({"action": "setup", "languages": "rust"});
    assert!(parse_languages(&params).is_err());
}

#[test]
fn parse_languages_non_string_element_errors() {
    let params = json!({"action": "setup", "languages": [42]});
    assert!(parse_languages(&params).is_err());
}

#[test]
fn parse_languages_normalizes_to_lowercase() {
    let params = json!({"action": "setup", "languages": ["Rust", "NODE"]});
    let langs = parse_languages(&params).unwrap();
    assert_eq!(langs, vec!["rust", "node"]);
}

// ─── Enforcement tests ──────────────────────────────────────────────────────

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

struct AlwaysDenyGate;
impl ApprovalGate for AlwaysDenyGate {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        ApprovalResult::Denied
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

const DEV_ENV_POLICY: &str = r#"
[tools.dev_environment]
enabled = true
match_source = "structured"

[tools.dev_environment.actions.setup]
tier = "act"
patterns = ["^setup$"]
"#;

fn make_agent(responses: Vec<Message>) -> AgentLoop<AlwaysDenyGate, NullSink> {
    let policy = Policy::from_str(DEV_ENV_POLICY).unwrap();
    let provider = MockProvider::new(responses);
    let registry = ToolRegistry::new();
    AgentLoop::new(
        policy,
        Box::new(provider),
        registry,
        "test".to_owned(),
        AlwaysDenyGate,
        NullSink,
        "test_user",
    )
}

/// Setup action at Act tier should be allowed (enforcement passes).
/// The tool is not registered so it will fail at execution, but the key is
/// that enforcement does NOT reject it.
#[tokio::test]
async fn enforcement_setup_allowed() {
    let responses = vec![
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t1".to_owned(),
                name: "dev_environment".to_owned(),
                input: json!({"action": "setup", "languages": ["rust"]}),
            }],
            stop_reason: StopReason::ToolUse,
        },
        end_turn(),
    ];
    let mut agent = make_agent(responses);
    agent.run_turn_text("setup rust").await.unwrap();
    let msgs = agent.session_messages();
    let tool_result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    if let Some(Message::ToolResult { content, .. }) = tool_result {
        // Must NOT be a policy rejection.
        assert_ne!(
            content, "action not permitted",
            "setup should not be rejected by policy"
        );
    }
}

/// Missing action field → enforcement rejects (structured extractor returns None).
#[tokio::test]
async fn enforcement_missing_action_rejected() {
    let responses = vec![
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t2".to_owned(),
                name: "dev_environment".to_owned(),
                input: json!({"languages": ["rust"]}),
            }],
            stop_reason: StopReason::ToolUse,
        },
        end_turn(),
    ];
    let mut agent = make_agent(responses);
    agent.run_turn_text("setup rust").await.unwrap();
    let msgs = agent.session_messages();
    let tool_result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    match tool_result {
        Some(Message::ToolResult { content, .. }) => {
            assert_eq!(
                content, "action not permitted",
                "missing action should be rejected"
            );
        }
        _ => panic!("expected a ToolResult message"),
    }
}

/// Unknown action → enforcement rejects (no pattern match).
#[tokio::test]
async fn enforcement_unknown_action_rejected() {
    let responses = vec![
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t3".to_owned(),
                name: "dev_environment".to_owned(),
                input: json!({"action": "teardown", "languages": []}),
            }],
            stop_reason: StopReason::ToolUse,
        },
        end_turn(),
    ];
    let mut agent = make_agent(responses);
    agent.run_turn_text("teardown").await.unwrap();
    let msgs = agent.session_messages();
    let tool_result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));
    match tool_result {
        Some(Message::ToolResult { content, .. }) => {
            assert_eq!(
                content, "action not permitted",
                "unknown action should be rejected"
            );
        }
        _ => panic!("expected a ToolResult message"),
    }
}

// ─── Docker integration (requires Docker) ───────────────────────────────────

/// End-to-end test: build a sandbox image with Rust, verify it exists.
///
/// Run with: cargo nextest run --features container --test dev_environment -- --ignored
#[tokio::test]
#[ignore]
async fn docker_build_and_verify() {
    use cherub::enforcement::{self, tier::Tier};
    use cherub::tools::ToolContext;
    use cherub::tools::container::BollardRuntime;
    use cherub::tools::container::ContainerRuntime;
    use std::sync::Arc;
    use tokio::time::{Duration, timeout};
    use uuid::Uuid;

    let runtime = match BollardRuntime::new() {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("skipping: Docker not available: {e}");
            return;
        }
    };

    if !runtime.is_available().await {
        eprintln!("skipping: Docker daemon not reachable");
        return;
    }

    let workspace = std::env::current_dir().expect("cwd");
    let rt: Arc<dyn ContainerRuntime> = runtime;
    let (bash_tool, _ipc_dir) = cherub::tools::container_bash::build(Arc::clone(&rt), workspace);

    let dev_env = cherub::tools::dev_environment::DevEnvironmentTool::new(Arc::clone(&bash_tool));

    // Build with rust.
    let token = enforcement::approve_escalation(Tier::Act);
    let result = timeout(
        Duration::from_secs(600),
        dev_env.execute(&json!({"action": "setup", "languages": ["rust"]}), token),
    )
    .await
    .expect("build timeout")
    .expect("build");
    assert!(
        result.output.contains("rust"),
        "should mention rust: {}",
        result.output
    );

    // Verify the bash tool now uses the new image by running cargo --version.
    let token2 = enforcement::approve_escalation(Tier::Observe);
    let ctx = ToolContext {
        user_id: "test-user".to_owned(),
        session_id: Uuid::now_v7(),
        turn_number: 1,
    };
    let result2 = timeout(
        Duration::from_secs(120),
        bash_tool.execute(&json!({"command": "cargo --version"}), token2, &ctx),
    )
    .await
    .expect("execute timeout")
    .expect("execute");
    assert!(
        result2.output.contains("cargo"),
        "cargo should be available: {}",
        result2.output
    );

    bash_tool.shutdown().await;
}
