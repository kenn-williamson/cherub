use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use tokio::sync::{mpsc, oneshot};

use crate::runtime::approval::{ApprovalGate, ApprovalResult, EscalationContext};

const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Callback data format for approval buttons.
fn approve_data(id: u64) -> String {
    format!("approve:{id}")
}

fn deny_data(id: u64) -> String {
    format!("deny:{id}")
}

/// Parse callback data into (approval_id, approved).
pub fn parse_callback_data(data: &str) -> Option<(u64, bool)> {
    if let Some(id_str) = data.strip_prefix("approve:") {
        id_str.parse().ok().map(|id| (id, true))
    } else if let Some(id_str) = data.strip_prefix("deny:") {
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

/// Approval gate for Telegram. Sends inline keyboard buttons and waits
/// for callback response via a channel.
pub struct TelegramApprovalGate {
    bot: Bot,
    chat_id: ChatId,
    timeout: Duration,
    approval_tx: mpsc::Sender<ApprovalMessage>,
    next_id: AtomicU64,
}

impl TelegramApprovalGate {
    pub fn new(bot: Bot, chat_id: ChatId, approval_tx: mpsc::Sender<ApprovalMessage>) -> Self {
        Self {
            bot,
            chat_id,
            timeout: DEFAULT_APPROVAL_TIMEOUT,
            approval_tx,
            next_id: AtomicU64::new(0),
        }
    }
}

impl ApprovalGate for TelegramApprovalGate {
    async fn request_approval(&self, context: &EscalationContext<'_>) -> ApprovalResult {
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
