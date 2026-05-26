use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;
use uuid::Uuid;

use crate::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};
#[cfg(feature = "postgres")]
use crate::storage::{NewTask, TaskStore};

const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Callback data format for blocking approval buttons (interactive turns).
fn approve_data(id: u64) -> String {
    format!("approve:{id}")
}

fn deny_data(id: u64) -> String {
    format!("deny:{id}")
}

/// Parse blocking approval callback data into (approval_id, approved).
pub fn parse_callback_data(data: &str) -> Option<(u64, bool)> {
    if let Some(id_str) = data.strip_prefix("approve:") {
        id_str.parse().ok().map(|id| (id, true))
    } else if let Some(id_str) = data.strip_prefix("deny:") {
        id_str.parse().ok().map(|id| (id, false))
    } else {
        None
    }
}

/// Parse task queue callback data into (task_id, approved).
/// Format: "task_approve:{uuid}" or "task_deny:{uuid}"
pub fn parse_task_callback_data(data: &str) -> Option<(Uuid, bool)> {
    if let Some(id_str) = data.strip_prefix("task_approve:") {
        id_str.parse().ok().map(|id| (id, true))
    } else if let Some(id_str) = data.strip_prefix("task_deny:") {
        id_str.parse().ok().map(|id| (id, false))
    } else {
        None
    }
}

/// A pending approval request waiting for a callback response.
pub struct PendingApproval {
    pub sender: oneshot::Sender<bool>,
}

/// Message sent from TelegramApprovalGate to the session manager to register
/// a pending approval and to resolve it when a callback arrives.
pub enum ApprovalMessage {
    /// Register a new pending approval.
    Register {
        id: u64,
        sender: oneshot::Sender<bool>,
    },
    /// Resolve a pending approval (from callback button press).
    Resolve { id: u64, approved: bool },
}

/// Approval gate for Telegram.
///
/// Handles two modes:
/// - **Interactive** (context.autonomous == false): Sends inline keyboard buttons and
///   waits for callback response. The turn blocks until the user responds or times out.
/// - **Autonomous** (context.autonomous == true, task_store set): Stores the action
///   in `task_queue`, sends an [Approve][Deny] notification, and returns immediately
///   with `ApprovalResult::Queued`. The turn continues with other work.
pub struct TelegramApprovalGate {
    bot: Bot,
    chat_id: ChatId,
    timeout: Duration,
    approval_tx: mpsc::Sender<ApprovalMessage>,
    next_id: AtomicU64,
    /// For autonomous turns: store task and return immediately instead of blocking.
    #[cfg(feature = "postgres")]
    task_store: Option<Arc<dyn TaskStore>>,
    #[cfg(feature = "postgres")]
    user_id: String,
    /// Shared cell filled in by session.rs after with_persistence() attaches a session.
    /// Allows queued tasks to record the originating session_id for audit provenance.
    #[cfg(feature = "postgres")]
    session_id_cell: Option<Arc<std::sync::Mutex<Option<Uuid>>>>,
}

impl TelegramApprovalGate {
    pub fn new(bot: Bot, chat_id: ChatId, approval_tx: mpsc::Sender<ApprovalMessage>) -> Self {
        Self {
            bot,
            chat_id,
            timeout: DEFAULT_APPROVAL_TIMEOUT,
            approval_tx,
            next_id: AtomicU64::new(0),
            #[cfg(feature = "postgres")]
            task_store: None,
            #[cfg(feature = "postgres")]
            user_id: chat_id.to_string(),
            #[cfg(feature = "postgres")]
            session_id_cell: None,
        }
    }

    /// Attach a task store to enable async queuing for autonomous turns.
    ///
    /// `session_id_cell` is a shared cell that session.rs fills in after
    /// `with_persistence()` attaches a session. Queued tasks read the cell at
    /// queue time so they can record the originating session for audit provenance.
    #[cfg(feature = "postgres")]
    pub fn with_task_store(
        mut self,
        store: Arc<dyn TaskStore>,
        user_id: String,
        session_id_cell: Arc<std::sync::Mutex<Option<Uuid>>>,
    ) -> Self {
        self.task_store = Some(store);
        self.user_id = user_id;
        self.session_id_cell = Some(session_id_cell);
        self
    }

    /// Queue mode: store the action in task_queue and notify the user non-blocking.
    #[cfg(feature = "postgres")]
    async fn queue_approval(&self, context: &EscalationContext<'_>) -> ApprovalResult {
        let description = format!("{}: {}", context.tool, context.command);
        let Some(store) = self.task_store.as_ref() else {
            warn!("queue_approval called without task_store — falling back to deny");
            return ApprovalResult::Denied;
        };

        let session_id = self
            .session_id_cell
            .as_ref()
            .and_then(|cell| *cell.lock().expect("session_id_cell poisoned"));

        let task_id = match store
            .create(NewTask {
                user_id: self.user_id.clone(),
                session_id,
                tool: context.tool.to_owned(),
                action: Some(context.command.to_owned()),
                params: context.params.clone(),
                tier: "commit".to_owned(),
                description: description.clone(),
            })
            .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "failed to queue task, falling back to deny");
                return ApprovalResult::Denied;
            }
        };

        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("Approve", format!("task_approve:{task_id}")),
            InlineKeyboardButton::callback("Deny", format!("task_deny:{task_id}")),
        ]]);

        let msg = format!(
            "\u{23f3} Approval needed\n\n{}\n\nApprove to allow this action.",
            description
        );

        match self
            .bot
            .send_message(self.chat_id, &msg)
            .reply_markup(keyboard)
            .await
        {
            Ok(sent_msg) => {
                let tg_id = sent_msg.id.to_string();
                if let Err(e) = store.set_tg_message_id(task_id, &tg_id).await {
                    tracing::warn!(error = %e, "failed to store tg_message_id (non-fatal)");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to send task approval notification");
            }
        }

        ApprovalResult::Queued(task_id)
    }
}

impl ApprovalGate for TelegramApprovalGate {
    /// Request approval for an escalated action.
    ///
    /// In autonomous mode (context.autonomous == true) with a task store attached:
    ///   → queue to DB, send Telegram notification, return Queued immediately.
    ///
    /// In interactive mode (context.autonomous == false):
    ///   → send inline keyboard, block waiting for callback, return Approved/Denied.
    async fn request_approval(&self, context: &EscalationContext<'_>) -> ApprovalResult {
        // Autonomous path: queue instead of block.
        #[cfg(feature = "postgres")]
        if context.autonomous && self.task_store.is_some() {
            return self.queue_approval(context).await;
        }

        // Interactive path: send inline keyboard and wait.
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("Allow", approve_data(id)),
            InlineKeyboardButton::callback("Deny", deny_data(id)),
        ]]);

        let msg = format!(
            "[ESCALATION] {} wants to execute: {}\nAllow? ({}s timeout)",
            context.tool,
            context.command,
            self.timeout.as_secs()
        );

        // Send the approval prompt with inline keyboard.
        if self
            .bot
            .send_message(self.chat_id, &msg)
            .reply_markup(keyboard)
            .await
            .is_err()
        {
            return ApprovalResult::Denied;
        }

        // Create a oneshot channel and register it with the session manager.
        let (tx, rx) = oneshot::channel();
        if self
            .approval_tx
            .send(ApprovalMessage::Register { id, sender: tx })
            .await
            .is_err()
        {
            return ApprovalResult::Denied;
        }

        // Wait for approval response with timeout.
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(true)) => ApprovalResult::Approved,
            _ => ApprovalResult::Denied,
        }
    }
}

/// Manages pending approvals. Runs as a task, receiving messages via channel.
pub async fn approval_manager(mut rx: mpsc::Receiver<ApprovalMessage>) {
    let mut pending: HashMap<u64, oneshot::Sender<bool>> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        match msg {
            ApprovalMessage::Register { id, sender } => {
                pending.insert(id, sender);
            }
            ApprovalMessage::Resolve { id, approved } => {
                if let Some(sender) = pending.remove(&id) {
                    let _ = sender.send(approved);
                }
            }
        }
    }
}
