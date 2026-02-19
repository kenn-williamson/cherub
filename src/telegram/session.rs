use std::collections::HashMap;
#[cfg(feature = "memory")]
use std::sync::Arc;

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
    /// PostgreSQL connection pool for session persistence and/or memory.
    /// Present when `sessions` or `memory` feature is enabled.
    #[cfg(any(feature = "sessions", feature = "memory"))]
    pub db_pool: Option<deadpool_postgres::Pool>,
    /// Embedding provider for hybrid memory search (M6c).
    /// `None` = FTS-only search.
    #[cfg(feature = "memory")]
    pub embedder: Option<Arc<dyn crate::storage::embedding::EmbeddingProvider>>,
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

                    let chat_config = SessionConfig {
                        bot: config.bot.clone(),
                        policy: config.policy.clone(),
                        model: config.model.clone(),
                        max_tokens: config.max_tokens,
                        api_key: config.api_key.clone(),
                        #[cfg(any(feature = "sessions", feature = "memory"))]
                        db_pool: config.db_pool.clone(),
                        #[cfg(feature = "memory")]
                        embedder: config.embedder.clone(),
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

    // Derive user identity from the Telegram chat ID (unique per chat channel).
    let user_id = chat_id.to_string();

    // Build ToolRegistry — attach memory store if available.
    // The store is Arc so it can be shared between the tool registry and injection.
    #[cfg(feature = "memory")]
    let (registry, memory_store_for_injection) = {
        if let Some(ref pool) = config.db_pool {
            use crate::storage::MemoryStore;
            use crate::storage::pg_memory_store::PgMemoryStore;

            let store: Arc<dyn MemoryStore> = match config.embedder.clone() {
                Some(embedder) => Arc::new(PgMemoryStore::with_embedder(pool.clone(), embedder)),
                None => Arc::new(PgMemoryStore::new(pool.clone())),
            };
            let registry = ToolRegistry::with_memory(Arc::clone(&store));
            (registry, Some(store))
        } else {
            (ToolRegistry::new(), None)
        }
    };
    #[cfg(not(feature = "memory"))]
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
        &user_id,
    );

    // Attach proactive memory injection if store is available (M6d).
    #[cfg(feature = "memory")]
    if let Some(store) = memory_store_for_injection {
        agent.with_memory_injection(store);
        info!(chat_id = %chat_id, "proactive memory injection enabled");
    }

    // Attach session persistence per chat if a pool is available.
    #[cfg(feature = "sessions")]
    if let Some(pool) = config.db_pool {
        use crate::storage::pg_session_store::PgSessionStore;
        let store = Box::new(PgSessionStore::new(pool));
        let connector_id = chat_id.to_string();
        match agent
            .with_persistence(store, "telegram", &connector_id)
            .await
        {
            Ok(()) => {
                let msg_count = agent.session_messages().len();
                info!(
                    chat_id = %chat_id,
                    session_id = %agent.session_id(),
                    message_count = msg_count,
                    "session persistence attached"
                );
            }
            Err(e) => {
                warn!(chat_id = %chat_id, error = %e, "session persistence unavailable, running ephemeral");
            }
        }
    }

    while let Some(msg) = rx.recv().await {
        if let Err(e) = agent.run_turn(msg.content).await {
            warn!(chat_id = %chat_id, error = %e, "agent turn error");
        }
    }
}
