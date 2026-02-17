use std::collections::HashMap;

use teloxide::prelude::*;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::enforcement::policy::Policy;
use crate::providers::UserContent;
use crate::providers::anthropic::AnthropicProvider;
use crate::runtime::AgentLoop;
use crate::runtime::prompt::build_system_prompt;
use crate::tools::ToolRegistry;

use super::approval::{ApprovalMessage, TelegramApprovalGate};
use super::output::TelegramSink;

/// Inbound message from a Telegram user to be routed to the appropriate session.
pub struct InboundMessage {
    pub content: Vec<UserContent>,
}

/// Configuration for creating new per-chat agent sessions.
pub struct SessionConfig {
    pub bot: Bot,
    pub policy: Policy,
    pub model: String,
    pub max_tokens: u32,
    pub api_key: secrecy::SecretString,
}

/// Message sent to the session manager from the connector.
pub enum SessionCommand {
    /// Route an inbound message to the appropriate chat session.
    Message {
        chat_id: ChatId,
        message: InboundMessage,
    },
    /// Route an approval callback to the approval manager.
    ApprovalCallback { id: u64, approved: bool },
}

/// Session manager task. Owns all per-chat sessions and approval routing.
/// Communicates via channels — no Arc<Mutex>.
pub async fn session_manager(
    mut rx: mpsc::Receiver<SessionCommand>,
    config: SessionConfig,
    approval_tx: mpsc::Sender<ApprovalMessage>,
) {
    let mut chat_senders: HashMap<ChatId, mpsc::Sender<InboundMessage>> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            SessionCommand::Message { chat_id, message } => {
                // Get or create the per-chat sender.
                let sender = chat_senders.entry(chat_id).or_insert_with(|| {
                    info!(chat_id = %chat_id, "creating new chat session");

                    let (tx, rx) = mpsc::channel::<InboundMessage>(32);

                    // Spawn a per-chat task.
                    let chat_config = SessionConfig {
                        bot: config.bot.clone(),
                        policy: config.policy.clone(),
                        model: config.model.clone(),
                        max_tokens: config.max_tokens,
                        api_key: config.api_key.clone(),
                    };
                    let approval_tx = approval_tx.clone();

                    tokio::spawn(async move {
                        chat_session(rx, chat_id, chat_config, approval_tx).await;
                    });

                    tx
                });

                if sender.send(message).await.is_err() {
                    warn!(chat_id = %chat_id, "chat session channel closed, removing");
                    chat_senders.remove(&chat_id);
                }
            }
            SessionCommand::ApprovalCallback { id, approved } => {
                let _ = approval_tx
                    .send(ApprovalMessage::Resolve { id, approved })
                    .await;
            }
        }
    }
}

/// Per-chat session task. Owns an AgentLoop and processes messages sequentially.
async fn chat_session(
    mut rx: mpsc::Receiver<InboundMessage>,
    chat_id: ChatId,
    config: SessionConfig,
    approval_tx: mpsc::Sender<ApprovalMessage>,
) {
    let provider = match AnthropicProvider::new(config.api_key, &config.model, config.max_tokens) {
        Ok(p) => p,
        Err(e) => {
            warn!(chat_id = %chat_id, error = %e, "failed to create provider");
            return;
        }
    };

    let registry = ToolRegistry::new();

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_owned());
    let system_prompt = build_system_prompt(&cwd);

    let output = TelegramSink::new(config.bot.clone(), chat_id);
    let approval_gate = TelegramApprovalGate::new(config.bot, chat_id, approval_tx);

    let mut agent = AgentLoop::new(
        config.policy,
        provider,
        registry,
        system_prompt,
        approval_gate,
        output,
    );

    while let Some(msg) = rx.recv().await {
        if let Err(e) = agent.run_turn(msg.content).await {
            warn!(chat_id = %chat_id, error = %e, "agent turn error");
        }
    }
}
