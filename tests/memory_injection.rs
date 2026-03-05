//! Integration tests for M6d: Proactive Memory Injection.
//!
//! Verifies that:
//! 1. The runtime injects relevant memories into the system prompt before each turn.
//! 2. The agent cannot suppress injection — it's runtime-controlled context.
//! 3. No injection occurs when no store is attached.
//! 4. No injection occurs when the search returns no results.
//! 5. No injection occurs for very short queries (< INJECTION_MIN_QUERY_LEN).
//!
//! Uses a mock provider that captures the system prompt it receives, and an
//! in-memory `MemoryStore` that bypasses PostgreSQL entirely.

#![cfg(feature = "memory")]

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use uuid::{NoContext, Timestamp, Uuid};

use cherub::enforcement::policy::Policy;
use cherub::error::CherubError;
use cherub::providers::{ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition};
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
use cherub::runtime::output::NullSink;
use cherub::storage::{
    Memory, MemoryCategory, MemoryFilter, MemoryScope, MemoryStore, MemoryUpdate, NewMemory,
    SourceType,
};
use cherub::tools::ToolRegistry;

// ─── Mock Provider that captures system prompts ───────────────────────────────

/// Records every system prompt it receives into a shared captures buffer.
/// Drains canned responses from a queue.
struct CapturingProvider {
    responses: Mutex<VecDeque<Message>>,
    /// Shared captures — tests hold a clone of this Arc to inspect results.
    captures: Arc<Mutex<Vec<String>>>,
}

impl CapturingProvider {
    /// Returns `(provider, captures_handle)`. Pass `provider` to AgentLoop,
    /// inspect `captures_handle` after the turn runs.
    fn new(responses: Vec<Message>) -> (Self, Arc<Mutex<Vec<String>>>) {
        let captures = Arc::new(Mutex::new(Vec::new()));
        let provider = Self {
            responses: Mutex::new(VecDeque::from(responses)),
            captures: Arc::clone(&captures),
        };
        (provider, captures)
    }
}

#[async_trait]
impl Provider for CapturingProvider {
    async fn complete(
        &self,
        system: &str,
        _messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        self.captures.lock().unwrap().push(system.to_owned());
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
            .filter(|m| {
                filter
                    .user_id
                    .as_deref()
                    .map_or(true, |uid| m.user_id == uid)
            })
            .filter(|m| filter.scope.map_or(true, |s| m.scope == s))
            .filter(|m| filter.category.map_or(true, |c| m.category == c))
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
            .filter(|m| user_id.map_or(true, |uid| m.user_id == uid))
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

/// When memories match the user message, they are injected into the system prompt.
#[tokio::test]
async fn relevant_memories_injected_into_system_prompt() {
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
        Box::new(provider),
        registry,
        "base system prompt".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );
    agent.with_memory_injection(Arc::clone(&store) as Arc<dyn MemoryStore>);

    agent.run_turn_text("I like peanut butter").await.unwrap();

    let systems = captures.lock().unwrap().clone();
    assert_eq!(systems.len(), 1, "provider should be called once");
    let system = &systems[0];
    assert!(
        system.starts_with("base system prompt"),
        "should start with base prompt"
    );
    assert!(
        system.contains("## Relevant memories"),
        "should contain injection section: {system}"
    );
    assert!(
        system.contains("User is allergic to peanuts"),
        "should contain the injected memory: {system}"
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
        Box::new(provider),
        registry,
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

    let systems = captures.lock().unwrap().clone();
    assert_eq!(systems.len(), 1);
    assert_eq!(
        systems[0], "base system prompt",
        "system prompt should be unchanged when no memories match"
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
        Box::new(provider),
        registry,
        "base system prompt".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );

    agent
        .run_turn_text("Tell me about preferences")
        .await
        .unwrap();

    let systems = captures.lock().unwrap().clone();
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0], "base system prompt");
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
        Box::new(provider),
        registry,
        "base system prompt".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );
    agent.with_memory_injection(Arc::clone(&store) as Arc<dyn MemoryStore>);

    // "hi" is only 2 chars — below INJECTION_MIN_QUERY_LEN (3).
    agent.run_turn_text("hi").await.unwrap();

    let systems = captures.lock().unwrap().clone();
    assert_eq!(systems.len(), 1);
    assert_eq!(
        systems[0], "base system prompt",
        "short query should not trigger injection"
    );
}

/// Injection is computed once per turn and reused across all provider calls within
/// the same turn (when tool calls trigger multiple iterations).
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
        Box::new(provider),
        registry,
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

    let systems = captures.lock().unwrap().clone();
    // Provider is called twice (tool use + end turn), both with the same effective system.
    assert_eq!(systems.len(), 2, "provider should be called twice");
    assert_eq!(
        systems[0], systems[1],
        "system prompt should be identical across iterations of the same turn"
    );
    assert!(
        systems[0].contains("## Relevant memories"),
        "injection should be present in both calls"
    );
    assert!(systems[0].contains("User is left-handed"));
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
        Box::new(provider),
        registry,
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

    let systems = captures.lock().unwrap().clone();
    let system = &systems[0];
    assert!(
        system.contains("### Inferred (lower confidence)"),
        "inferred section should be present: {system}"
    );
    assert!(
        system.contains("confidence: 0.65"),
        "confidence label should be present: {system}"
    );
    assert!(
        !system.contains("### Verified"),
        "verified section should be absent when no verified memories: {system}"
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
        Box::new(provider),
        registry,
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

    let systems = captures.lock().unwrap().clone();
    // Regardless of what the model outputs, the injection was in the system prompt.
    assert!(
        systems[0].contains("## Relevant memories"),
        "injection must be present regardless of agent response"
    );
    assert!(systems[0].contains("User's name is Alice"));
}
