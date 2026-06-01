//! Integration tests for M6d: Proactive Memory Injection.
//!
//! Verifies that:
//! 1. The runtime injects relevant memories into the *user message* before each turn,
//!    leaving the system prompt byte-stable (so the prompt-cache prefix keeps hitting).
//! 2. The agent cannot suppress injection — it's runtime-controlled context.
//! 3. No injection occurs when no store is attached.
//! 4. No injection occurs when the search returns no results.
//! 5. No injection occurs for very short queries (< INJECTION_MIN_QUERY_LEN).
//!
//! Uses a mock provider that captures both the system prompt and the user-message
//! text it receives, and an in-memory `MemoryStore` that bypasses PostgreSQL entirely.

#![cfg(feature = "memory")]

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use uuid::{NoContext, Timestamp, Uuid};

use cherub::enforcement::policy::Policy;
use cherub::error::CherubError;
use cherub::providers::{
    ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition, UserContent,
};
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
use cherub::runtime::output::NullSink;
use cherub::storage::{
    Memory, MemoryCategory, MemoryFilter, MemoryScope, MemoryStore, MemoryUpdate, NewMemory,
    SourceType,
};
use cherub::tools::ToolRegistry;

// ─── Mock Provider that captures system prompts + user-message text ────────────

/// Shared capture buffers — tests hold a clone to inspect results after a turn.
#[derive(Clone)]
struct Captures {
    /// The system prompt passed on each provider call.
    systems: Arc<Mutex<Vec<String>>>,
    /// The concatenated text of all user messages passed on each provider call.
    user_texts: Arc<Mutex<Vec<String>>>,
}

/// Records the system prompt and user-message text it receives on each call.
/// Drains canned responses from a queue.
struct CapturingProvider {
    responses: Mutex<VecDeque<Message>>,
    captures: Captures,
}

impl CapturingProvider {
    /// Returns `(provider, captures)`. Pass `provider` to AgentLoop, inspect
    /// `captures` after the turn runs.
    fn new(responses: Vec<Message>) -> (Self, Captures) {
        let captures = Captures {
            systems: Arc::new(Mutex::new(Vec::new())),
            user_texts: Arc::new(Mutex::new(Vec::new())),
        };
        let provider = Self {
            responses: Mutex::new(VecDeque::from(responses)),
            captures: captures.clone(),
        };
        (provider, captures)
    }
}

/// Flatten the text of every user message in a slice into one searchable string.
fn user_text_of(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.as_str()),
                        UserContent::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Provider for CapturingProvider {
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        self.captures
            .systems
            .lock()
            .unwrap()
            .push(system.to_owned());
        self.captures
            .user_texts
            .lock()
            .unwrap()
            .push(user_text_of(messages));
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

fn end_turn() -> Message {
    Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "done".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    }
}

// ─── Auto-approve gate ────────────────────────────────────────────────────────

struct AutoApprove;

impl ApprovalGate for AutoApprove {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        ApprovalResult::Approved
    }
}

// ─── In-memory MemoryStore (no DB) ────────────────────────────────────────────

/// Simple in-memory store. `search()` returns memories whose content contains
/// any word from the query (case-insensitive). Sufficient for injection tests.
#[derive(Default)]
struct InMemoryStore {
    memories: Mutex<Vec<Memory>>,
}

impl InMemoryStore {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn add(&self, m: Memory) {
        self.memories.lock().unwrap().push(m);
    }
}

fn new_uuid() -> Uuid {
    Uuid::new_v7(Timestamp::now(NoContext))
}

fn make_memory_row(content: &str, source_type: SourceType, confidence: f32) -> Memory {
    Memory {
        id: new_uuid(),
        user_id: "test_user".to_owned(),
        scope: MemoryScope::User,
        category: MemoryCategory::Preference,
        path: "test/path".to_owned(),
        content: content.to_owned(),
        structured: None,
        source_session_id: None,
        source_turn_number: None,
        source_type,
        confidence,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        last_referenced_at: None,
        superseded_by: None,
    }
}

#[async_trait]
impl MemoryStore for InMemoryStore {
    async fn store(&self, memory: NewMemory) -> Result<Uuid, CherubError> {
        let id = new_uuid();
        let row = Memory {
            id,
            user_id: memory.user_id,
            scope: memory.scope,
            category: memory.category,
            path: memory.path,
            content: memory.content,
            structured: memory.structured,
            source_session_id: memory.source_session_id,
            source_turn_number: memory.source_turn_number,
            source_type: memory.source_type,
            confidence: memory.confidence,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_referenced_at: None,
            superseded_by: None,
        };
        self.memories.lock().unwrap().push(row);
        Ok(id)
    }

    async fn recall(&self, filter: MemoryFilter) -> Result<Vec<Memory>, CherubError> {
        let memories = self.memories.lock().unwrap();
        let results: Vec<Memory> = memories
            .iter()
            .filter(|m| filter.user_id.as_deref().is_none_or(|uid| m.user_id == uid))
            .filter(|m| filter.scope.is_none_or(|s| m.scope == s))
            .filter(|m| filter.category.is_none_or(|c| m.category == c))
            .cloned()
            .collect();
        Ok(results)
    }

    async fn search(
        &self,
        query: &str,
        _scope: Option<MemoryScope>,
        user_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Memory>, CherubError> {
        let memories = self.memories.lock().unwrap();
        let query_lower = query.to_lowercase();
        // Match if any significant word (≥4 chars) from the query appears in the content.
        let query_words: Vec<&str> = query_lower
            .split_whitespace()
            .filter(|w| w.len() >= 4)
            .collect();
        let results: Vec<Memory> = memories
            .iter()
            .filter(|m| user_id.is_none_or(|uid| m.user_id == uid))
            .filter(|m| {
                let content_lower = m.content.to_lowercase();
                query_words.iter().any(|w| content_lower.contains(*w))
            })
            .take(limit as usize)
            .cloned()
            .collect();
        Ok(results)
    }

    async fn update(&self, id: Uuid, _changes: MemoryUpdate) -> Result<Uuid, CherubError> {
        Ok(id)
    }

    async fn forget(&self, _id: Uuid) -> Result<(), CherubError> {
        Ok(())
    }

    async fn touch(&self, _id: Uuid) -> Result<(), CherubError> {
        Ok(())
    }
}

// ─── Policy ───────────────────────────────────────────────────────────────────

const POLICY: &str = r#"
[tools.bash]
enabled = true
[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
"#;

// ─── Tests ────────────────────────────────────────────────────────────────────

/// When memories match the user message, they are injected into the user message
/// while the system prompt stays byte-stable (preserving the prompt-cache prefix).
#[tokio::test]
async fn relevant_memories_injected_into_user_message() {
    let store = InMemoryStore::new();
    store.add(make_memory_row(
        "User is allergic to peanuts",
        SourceType::Explicit,
        1.0,
    ));

    let (provider, captures) = CapturingProvider::new(vec![end_turn()]);
    let policy = Policy::from_str(POLICY).unwrap();
    let registry = ToolRegistry::with_memory(Arc::clone(&store) as Arc<dyn MemoryStore>);
    let mut agent = AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "base system prompt".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );
    agent.with_memory_injection(Arc::clone(&store) as Arc<dyn MemoryStore>);

    agent.run_turn_text("I like peanut butter").await.unwrap();

    let systems = captures.systems.lock().unwrap().clone();
    let user_texts = captures.user_texts.lock().unwrap().clone();
    assert_eq!(systems.len(), 1, "provider should be called once");
    // System block stays static so the prompt-cache prefix keeps hitting.
    assert_eq!(
        systems[0], "base system prompt",
        "system prompt must stay byte-stable (no injection appended)"
    );
    // Injection now rides the user message.
    let user_text = &user_texts[0];
    assert!(
        user_text.contains("## Relevant memories"),
        "should contain injection section in the user message: {user_text}"
    );
    assert!(
        user_text.contains("User is allergic to peanuts"),
        "should contain the injected memory: {user_text}"
    );
    assert!(
        user_text.contains("I like peanut butter"),
        "should still contain the user's own message: {user_text}"
    );
}

/// When no memories match, the system prompt is passed through unchanged.
#[tokio::test]
async fn no_matching_memories_no_injection() {
    let store = InMemoryStore::new();
    store.add(make_memory_row(
        "User prefers dark mode",
        SourceType::Explicit,
        1.0,
    ));

    let (provider, captures) = CapturingProvider::new(vec![end_turn()]);
    let policy = Policy::from_str(POLICY).unwrap();
    let registry = ToolRegistry::with_memory(Arc::clone(&store) as Arc<dyn MemoryStore>);
    let mut agent = AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "base system prompt".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );
    agent.with_memory_injection(Arc::clone(&store) as Arc<dyn MemoryStore>);

    agent
        .run_turn_text("What is the weather like today?")
        .await
        .unwrap();

    let systems = captures.systems.lock().unwrap().clone();
    let user_texts = captures.user_texts.lock().unwrap().clone();
    assert_eq!(systems.len(), 1);
    assert_eq!(
        systems[0], "base system prompt",
        "system prompt should be unchanged when no memories match"
    );
    assert!(
        !user_texts[0].contains("## Relevant memories"),
        "no injection block when no memories match: {}",
        user_texts[0]
    );
}

/// When no memory store is attached, the system prompt is passed through unchanged.
#[tokio::test]
async fn no_store_no_injection() {
    let (provider, captures) = CapturingProvider::new(vec![end_turn()]);
    let policy = Policy::from_str(POLICY).unwrap();
    let registry = ToolRegistry::new();
    // No with_memory_injection() call — agent has no store.
    let mut agent = AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "base system prompt".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );

    agent
        .run_turn_text("Tell me about preferences")
        .await
        .unwrap();

    let systems = captures.systems.lock().unwrap().clone();
    let user_texts = captures.user_texts.lock().unwrap().clone();
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0], "base system prompt");
    assert!(!user_texts[0].contains("## Relevant memories"));
}

/// Very short queries (< 3 chars) skip injection entirely.
#[tokio::test]
async fn short_query_skips_injection() {
    let store = InMemoryStore::new();
    store.add(make_memory_row("hi note", SourceType::Explicit, 1.0));

    let (provider, captures) = CapturingProvider::new(vec![end_turn()]);
    let policy = Policy::from_str(POLICY).unwrap();
    let registry = ToolRegistry::with_memory(Arc::clone(&store) as Arc<dyn MemoryStore>);
    let mut agent = AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "base system prompt".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );
    agent.with_memory_injection(Arc::clone(&store) as Arc<dyn MemoryStore>);

    // "hi" is only 2 chars — below INJECTION_MIN_QUERY_LEN (3).
    agent.run_turn_text("hi").await.unwrap();

    let systems = captures.systems.lock().unwrap().clone();
    let user_texts = captures.user_texts.lock().unwrap().clone();
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0], "base system prompt");
    assert!(
        !user_texts[0].contains("## Relevant memories"),
        "short query should not trigger injection: {}",
        user_texts[0]
    );
}

/// Injection is computed once per turn and lives in the message history, so it's
/// seen identically across all provider calls within the same turn (when tool calls
/// trigger multiple iterations). The system prompt stays static throughout.
#[tokio::test]
async fn injection_consistent_across_iterations() {
    let store = InMemoryStore::new();
    store.add(make_memory_row(
        "User is left-handed",
        SourceType::Confirmed,
        1.0,
    ));

    // Two provider responses: first triggers a bash tool call, second ends the turn.
    let tool_response = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "ls /tmp"}),
        }],
        stop_reason: StopReason::ToolUse,
    };

    let (provider, captures) = CapturingProvider::new(vec![tool_response, end_turn()]);
    let policy = Policy::from_str(POLICY).unwrap();
    let registry = ToolRegistry::with_memory(Arc::clone(&store) as Arc<dyn MemoryStore>);
    let mut agent = AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "base system prompt".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );
    agent.with_memory_injection(Arc::clone(&store) as Arc<dyn MemoryStore>);

    agent
        .run_turn_text("User is left-handed, list temp files")
        .await
        .unwrap();

    let systems = captures.systems.lock().unwrap().clone();
    let user_texts = captures.user_texts.lock().unwrap().clone();
    // Provider is called twice (tool use + end turn).
    assert_eq!(systems.len(), 2, "provider should be called twice");
    // System prompt is static across iterations — never carries injection.
    assert_eq!(systems[0], "base system prompt");
    assert_eq!(systems[1], "base system prompt");
    // The injected memory lives in history, so it's identical in both calls.
    assert_eq!(
        user_texts[0], user_texts[1],
        "user content (incl. injection) should be identical across iterations"
    );
    assert!(
        user_texts[0].contains("## Relevant memories"),
        "injection should be present in both calls: {}",
        user_texts[0]
    );
    assert!(user_texts[0].contains("User is left-handed"));
}

/// Inferred memories appear in the Inferred subsection with confidence labels.
#[tokio::test]
async fn inferred_memory_has_confidence_label() {
    let store = InMemoryStore::new();
    store.add(make_memory_row(
        "User usually shops on Sundays",
        SourceType::Inferred,
        0.65,
    ));

    let (provider, captures) = CapturingProvider::new(vec![end_turn()]);
    let policy = Policy::from_str(POLICY).unwrap();
    let registry = ToolRegistry::with_memory(Arc::clone(&store) as Arc<dyn MemoryStore>);
    let mut agent = AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "base system prompt".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );
    agent.with_memory_injection(Arc::clone(&store) as Arc<dyn MemoryStore>);

    agent
        .run_turn_text("User usually shops on weekends")
        .await
        .unwrap();

    let systems = captures.systems.lock().unwrap().clone();
    let user_texts = captures.user_texts.lock().unwrap().clone();
    assert_eq!(
        systems[0], "base system prompt",
        "system prompt stays static"
    );
    let user_text = &user_texts[0];
    assert!(
        user_text.contains("### Inferred (lower confidence)"),
        "inferred section should be present: {user_text}"
    );
    assert!(
        user_text.contains("confidence: 0.65"),
        "confidence label should be present: {user_text}"
    );
    assert!(
        !user_text.contains("### Verified"),
        "verified section should be absent when no verified memories: {user_text}"
    );
}

/// The agent cannot suppress injection. Injection is purely runtime-controlled —
/// there is no tool call, no model output, and no API that lets the model skip it.
/// This test verifies the injection is present even when the agent responds normally.
#[tokio::test]
async fn agent_cannot_suppress_injection() {
    let store = InMemoryStore::new();
    store.add(make_memory_row(
        "User's name is Alice",
        SourceType::Explicit,
        1.0,
    ));

    // Agent responds with a simple text message (no tool use, no special handling).
    let (provider, captures) = CapturingProvider::new(vec![Message::Assistant {
        content: vec![ContentBlock::Text {
            text: "Hello, how can I help?".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
    }]);
    let policy = Policy::from_str(POLICY).unwrap();
    let registry = ToolRegistry::with_memory(Arc::clone(&store) as Arc<dyn MemoryStore>);
    let mut agent = AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "base system prompt".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );
    agent.with_memory_injection(Arc::clone(&store) as Arc<dyn MemoryStore>);

    agent
        .run_turn_text("User's name is Alice, who am I talking to?")
        .await
        .unwrap();

    let user_texts = captures.user_texts.lock().unwrap().clone();
    // Regardless of what the model outputs, the injection was in the user message.
    assert!(
        user_texts[0].contains("## Relevant memories"),
        "injection must be present regardless of agent response: {}",
        user_texts[0]
    );
    assert!(user_texts[0].contains("User's name is Alice"));
}
