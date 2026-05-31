//! Integration tests for M6 contradiction detection.
//!
//! Verifies the full flow: model proposes memory store → contradiction detected →
//! blocked → model retries with confirmed=true → succeeds.
//!
//! Uses a mock provider and in-memory MemoryStore (no DB needed).

#![cfg(feature = "memory")]

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
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

// ─── Mock Provider ───────────────────────────────────────────────────────────

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

fn end_turn() -> Message {
    Message::Assistant {
        content: vec![ContentBlock::Text {
            text: String::new(),
        }],
        stop_reason: StopReason::EndTurn,
    }
}

// ─── Auto-approve gate ──────────────────────────────────────────────────────

struct AutoApprove;

impl ApprovalGate for AutoApprove {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        ApprovalResult::Approved
    }
}

// ─── In-memory MemoryStore ──────────────────────────────────────────────────

#[derive(Default)]
struct InMemoryStore {
    memories: Mutex<Vec<Memory>>,
}

impl InMemoryStore {
    fn new_arc() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn add(&self, m: Memory) {
        self.memories.lock().unwrap().push(m);
    }

    fn count(&self) -> usize {
        self.memories.lock().unwrap().len()
    }
}

fn new_uuid() -> Uuid {
    Uuid::new_v7(Timestamp::now(NoContext))
}

fn make_memory(content: &str, user_id: &str) -> Memory {
    Memory {
        id: new_uuid(),
        user_id: user_id.to_owned(),
        scope: MemoryScope::User,
        category: MemoryCategory::Preference,
        path: "preferences/food".to_owned(),
        content: content.to_owned(),
        structured: None,
        source_session_id: None,
        source_turn_number: None,
        source_type: SourceType::Explicit,
        confidence: 1.0,
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
        let results = memories
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
        let query_words: Vec<&str> = query_lower
            .split_whitespace()
            .filter(|w| w.len() >= 4)
            .collect();
        let results = memories
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

// ─── Policy ─────────────────────────────────────────────────────────────────

const POLICY: &str = r#"
[tools.memory]
enabled = true
match_source = "structured"

[tools.memory.actions.read]
tier = "observe"
patterns = ["^recall", "^search"]

[tools.memory.actions.write]
tier = "act"
patterns = ["^store:", "^update:"]

[tools.memory.actions.delete]
tier = "commit"
patterns = ["^forget"]
"#;

// ─── Tests ──────────────────────────────────────────────────────────────────

/// Full flow: model stores a memory that conflicts → blocked → retries with confirmed → succeeds.
#[tokio::test]
async fn full_flow_contradiction_then_confirmed() {
    let store = InMemoryStore::new_arc();
    store.add(make_memory("Prefers Italian food", "test_user"));
    let initial_count = store.count();

    // Model's first attempt: store without confirmed (will be blocked).
    let first_store = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "t1".to_owned(),
            name: "memory".to_owned(),
            input: json!({
                "action": "store",
                "content": "Prefers Thai food",
                "category": "preference",
                "path": "preferences/food",
            }),
        }],
        stop_reason: StopReason::ToolUse,
    };

    // Model's second attempt: store with confirmed=true (will succeed).
    let confirmed_store = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "t2".to_owned(),
            name: "memory".to_owned(),
            input: json!({
                "action": "store",
                "content": "Prefers Thai food",
                "category": "preference",
                "path": "preferences/food",
                "confirmed": true,
            }),
        }],
        stop_reason: StopReason::ToolUse,
    };

    let provider = MockProvider::new(vec![first_store, confirmed_store, end_turn()]);
    let policy = Policy::from_str(POLICY).unwrap();
    let registry = ToolRegistry::with_memory(Arc::clone(&store) as Arc<dyn MemoryStore>);

    let mut agent = AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "test".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );

    agent.run_turn_text("test").await.unwrap();

    let msgs = agent.session_messages();

    // Find the first tool result — should be the contradiction block.
    let tool_results: Vec<&Message> = msgs
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .collect();

    assert!(
        tool_results.len() >= 2,
        "expected at least 2 tool results, got {}",
        tool_results.len(),
    );

    // First tool result: blocked by contradiction check.
    if let Message::ToolResult { content, .. } = tool_results[0] {
        assert!(
            content.contains("Similar memories exist"),
            "first store should be blocked: {content}",
        );
    }

    // Second tool result: confirmed store succeeded.
    if let Message::ToolResult { content, .. } = tool_results[1] {
        assert!(
            content.contains("stored:"),
            "confirmed store should succeed: {content}",
        );
    }

    // The store should have gained exactly one new memory.
    assert_eq!(
        store.count(),
        initial_count + 1,
        "exactly one new memory should be stored",
    );
}

/// Store with no conflicting memories goes through immediately.
#[tokio::test]
async fn store_no_conflict_succeeds_immediately() {
    let store = InMemoryStore::new_arc();

    let store_msg = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "t1".to_owned(),
            name: "memory".to_owned(),
            input: json!({
                "action": "store",
                "content": "Prefers dark mode",
                "category": "preference",
                "path": "preferences/ui",
            }),
        }],
        stop_reason: StopReason::ToolUse,
    };

    let provider = MockProvider::new(vec![store_msg, end_turn()]);
    let policy = Policy::from_str(POLICY).unwrap();
    let registry = ToolRegistry::with_memory(Arc::clone(&store) as Arc<dyn MemoryStore>);

    let mut agent = AgentLoop::new(
        policy,
        Arc::new(provider),
        Arc::new(registry),
        "test".to_owned(),
        AutoApprove,
        NullSink,
        "test_user",
    );

    agent.run_turn_text("test").await.unwrap();

    let msgs = agent.session_messages();
    let tool_result = msgs
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }));

    if let Some(Message::ToolResult { content, .. }) = tool_result {
        assert!(
            content.starts_with("stored:"),
            "should store immediately: {content}",
        );
    } else {
        panic!("expected a tool result");
    }

    assert_eq!(store.count(), 1);
}
