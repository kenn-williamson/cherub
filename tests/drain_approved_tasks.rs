//! Integration tests for `AgentLoop::drain_approved_tasks()`.
//!
//! Verifies the drain cycle without a real database:
//! - Empty queue returns 0
//! - Approved commit-tier tasks are claimed, executed, and marked done
//! - Race (mark_running returns false) causes skip
//! - Policy-changed tasks (action no longer Escalate) are marked failed
//! - Two-task mix: one success, one policy-changed
//!
//! Feature requirement: `--features sessions` (implies `postgres` for TaskStore).
//! No real database — uses `MockTaskStore`. No API key required.

#![cfg(feature = "postgres")]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use cherub::enforcement::policy::Policy;
use cherub::error::CherubError;
use cherub::providers::{ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition};
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
use cherub::runtime::output::{OutputEvent, OutputSink};
use cherub::storage::{NewTask, Task, TaskStore};
use cherub::tools::ToolRegistry;

// ---------------------------------------------------------------------------
// Mock TaskStore
// ---------------------------------------------------------------------------

/// In-memory TaskStore for testing drain_approved_tasks().
///
/// Tasks are stored with mutable status. `mark_running()` claims a task only if
/// its current status is "approved" — simulating the atomic race guard.
struct MockTaskStore {
    user_id: String,
    tasks: Mutex<Vec<Task>>,
}

impl MockTaskStore {
    fn new(user_id: &str, tasks: Vec<Task>) -> Self {
        Self {
            user_id: user_id.to_owned(),
            tasks: Mutex::new(tasks),
        }
    }

    /// Snapshot of all task statuses for post-drain assertions.
    fn statuses(&self) -> Vec<(Uuid, String)> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .map(|t| (t.id, t.status.clone()))
            .collect()
    }

    /// Error message recorded for the given task id, if any.
    fn error_for(&self, id: Uuid) -> Option<String> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.error_message.clone())
    }
}

#[async_trait]
impl TaskStore for MockTaskStore {
    async fn create(&self, _task: NewTask) -> Result<Uuid, CherubError> {
        unimplemented!("not used in drain tests")
    }

    async fn set_tg_message_id(&self, _id: Uuid, _tg_message_id: &str) -> Result<(), CherubError> {
        unimplemented!("not used in drain tests")
    }

    async fn list_approved(&self, user_id: &str) -> Result<Vec<Task>, CherubError> {
        if user_id != self.user_id {
            return Ok(vec![]);
        }
        Ok(self
            .tasks
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.status == "approved")
            .cloned()
            .collect())
    }

    async fn list_pending(&self, _user_id: &str) -> Result<Vec<Task>, CherubError> {
        unimplemented!("not used in drain tests")
    }

    async fn mark_approved(&self, _id: Uuid) -> Result<(), CherubError> {
        unimplemented!("not used in drain tests")
    }

    async fn mark_rejected(&self, _id: Uuid) -> Result<(), CherubError> {
        unimplemented!("not used in drain tests")
    }

    async fn mark_running(&self, id: Uuid) -> Result<bool, CherubError> {
        let mut tasks = self.tasks.lock().unwrap();
        match tasks.iter_mut().find(|t| t.id == id) {
            Some(t) if t.status == "approved" => {
                t.status = "running".to_owned();
                Ok(true)
            }
            // Already claimed (or wrong status) — simulate concurrent drain race.
            _ => Ok(false),
        }
    }

    async fn mark_done(&self, id: Uuid, output: &str) -> Result<(), CherubError> {
        let mut tasks = self.tasks.lock().unwrap();
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.status = "done".to_owned();
            t.result_output = Some(output.to_owned());
        }
        Ok(())
    }

    async fn mark_failed(&self, id: Uuid, error: &str) -> Result<(), CherubError> {
        let mut tasks = self.tasks.lock().unwrap();
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.status = "failed".to_owned();
            t.error_message = Some(error.to_owned());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Recording Sink
// ---------------------------------------------------------------------------

/// Captures OutputEvent::Text strings for assertion.
#[derive(Clone, Default)]
struct RecordingSink {
    texts: Arc<Mutex<Vec<String>>>,
}

impl OutputSink for RecordingSink {
    async fn emit(&self, event: OutputEvent<'_>) {
        if let OutputEvent::Text(s) = event {
            self.texts.lock().unwrap().push(s.to_owned());
        }
    }
}

// ---------------------------------------------------------------------------
// Mock Provider (not called during drain, but required by AgentLoop::new)
// ---------------------------------------------------------------------------

struct MockProvider {
    responses: Mutex<VecDeque<Message>>,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
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
        Ok((
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Message::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "done".to_owned(),
                    }],
                    stop_reason: StopReason::EndTurn,
                }),
            None,
        ))
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

struct DenyGate;

impl ApprovalGate for DenyGate {
    async fn request_approval(&self, _context: &EscalationContext<'_>) -> ApprovalResult {
        ApprovalResult::Denied
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const USER_ID: &str = "drain-test-user";

fn default_policy() -> Policy {
    Policy::load(std::path::Path::new("config/default_policy.toml")).unwrap()
}

/// Build a Task pre-set to `approved` status with the given bash command.
///
/// The task stores `params["command"]` so the enforcement layer's
/// `MatchSource::Command` extractor can evaluate it against bash patterns.
fn approved_bash_task(command: &str) -> Task {
    Task {
        id: Uuid::now_v7(),
        user_id: USER_ID.to_owned(),
        session_id: None,
        status: "approved".to_owned(),
        tool: "bash".to_owned(),
        action: Some(command.to_owned()),
        params: json!({ "command": command }),
        tier: "commit".to_owned(),
        description: format!("bash: {command}"),
        result_output: None,
        error_message: None,
        created_at: Utc::now(),
    }
}

/// Build an `AgentLoop` pre-wired with the given `MockTaskStore` and `RecordingSink`.
fn make_agent(
    store: Arc<MockTaskStore>,
    sink: RecordingSink,
) -> AgentLoop<DenyGate, RecordingSink> {
    let mut agent = AgentLoop::new(
        default_policy(),
        Arc::new(MockProvider::new()),
        Arc::new(ToolRegistry::new()),
        "test".to_owned(),
        DenyGate,
        sink,
        USER_ID,
    );
    let task_store: Arc<dyn cherub::storage::TaskStore> = store;
    agent.with_task_store(task_store);
    agent
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// No approved tasks → drain returns 0 immediately, no DB mutations.
#[tokio::test]
async fn drain_empty_queue() {
    let store = Arc::new(MockTaskStore::new(USER_ID, vec![]));
    let mut agent = make_agent(store.clone(), RecordingSink::default());

    let executed = agent.drain_approved_tasks().await;

    assert_eq!(executed, 0);
    assert!(store.statuses().is_empty());
}

/// One approved commit-tier bash command (rm -f on a nonexistent file).
///
/// `rm -f` exits 0 even when the file is absent, so `mark_done` is called.
/// Drain returns 1.
#[tokio::test]
async fn drain_executes_approved_task() {
    let task = approved_bash_task("rm -f /tmp/cherub-drain-test-nonexistent");
    let task_id = task.id;
    let store = Arc::new(MockTaskStore::new(USER_ID, vec![task]));
    let sink = RecordingSink::default();
    let mut agent = make_agent(store.clone(), sink.clone());

    let executed = agent.drain_approved_tasks().await;

    assert_eq!(executed, 1);
    let statuses = store.statuses();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].1, "done", "task should be marked done");

    // Output should contain the "✓ Completed:" prefix.
    let texts = sink.texts.lock().unwrap();
    assert!(
        texts.iter().any(|t| t.contains("Completed:")),
        "expected '✓ Completed:' in output, got: {texts:?}",
    );
    drop(texts);

    // task_id should still be tracked
    assert_eq!(statuses[0].0, task_id);
}

/// When `mark_running` returns false (task already claimed by another drainer),
/// the task is skipped and drain returns 0.
#[tokio::test]
async fn drain_skips_already_claimed_task() {
    // Pre-set task to "running" so mark_running returns false.
    let mut task = approved_bash_task("rm -f /tmp/cherub-drain-race-test");
    task.status = "running".to_owned(); // already claimed
    let store = Arc::new(MockTaskStore::new(USER_ID, vec![task]));

    // list_approved filters for "approved" — so this task won't even appear.
    let mut agent = make_agent(store.clone(), RecordingSink::default());
    let executed = agent.drain_approved_tasks().await;

    assert_eq!(executed, 0);
    // Status unchanged — drain never touched it.
    assert_eq!(store.statuses()[0].1, "running");
}

/// A task whose command evaluates to `Allow` (not `Escalate`) under the current
/// policy simulates a policy change since the task was queued.
///
/// `echo hello` matches the observe tier → `evaluate()` returns `Allow` → drain
/// marks the task failed with a "policy changed" message.
#[tokio::test]
async fn drain_policy_changed_marks_failed() {
    // "echo hello" matches the observe tier (Allow), not commit (Escalate).
    let task = approved_bash_task("echo hello");
    let task_id = task.id;
    let store = Arc::new(MockTaskStore::new(USER_ID, vec![task]));
    let mut agent = make_agent(store.clone(), RecordingSink::default());

    let executed = agent.drain_approved_tasks().await;

    assert_eq!(
        executed, 0,
        "policy-changed task should not count as executed"
    );
    assert_eq!(store.statuses()[0].1, "failed");

    let err = store.error_for(task_id).unwrap_or_default();
    assert!(
        err.contains("policy changed"),
        "error should mention 'policy changed', got: {err}",
    );
}

/// Two tasks: one commit-tier (success), one observe-tier (policy changed).
/// Drain returns 1 (only the successfully executed task counts).
#[tokio::test]
async fn drain_mixed_success_and_policy_changed() {
    let task_exec = approved_bash_task("rm -f /tmp/cherub-drain-mix-exec");
    let task_policy = approved_bash_task("echo check policy");
    let exec_id = task_exec.id;
    let policy_id = task_policy.id;

    let store = Arc::new(MockTaskStore::new(USER_ID, vec![task_exec, task_policy]));
    let sink = RecordingSink::default();
    let mut agent = make_agent(store.clone(), sink.clone());

    let executed = agent.drain_approved_tasks().await;

    assert_eq!(executed, 1);

    let statuses: std::collections::HashMap<Uuid, String> = store.statuses().into_iter().collect();
    assert_eq!(statuses[&exec_id], "done");
    assert_eq!(statuses[&policy_id], "failed");

    let policy_err = store.error_for(policy_id).unwrap_or_default();
    assert!(
        policy_err.contains("policy changed"),
        "policy-changed task should mention 'policy changed': {policy_err}",
    );
}
