use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use teloxide::prelude::*;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::enforcement::policy::Policy;
use crate::providers::UserContent;
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::config::ProvidersConfig;
use crate::providers::openai::OpenAiProvider;

use super::approval::{ApprovalMessage, TelegramApprovalGate};
use super::output::TelegramSink;

/// Inbound message routed to a per-chat session task.
pub enum InboundMessage {
    /// A message to process with the agent.
    /// `autonomous = true` for cron-triggered turns (commit-tier actions are queued).
    /// `autonomous = false` for user-initiated turns (commit-tier actions block).
    User {
        content: Vec<UserContent>,
        autonomous: bool,
    },
    /// Drain any approved tasks from the task queue for this chat.
    /// Sent by the callback handler when the user approves a queued task.
    DrainApprovedTasks,
    /// Switch the model. The string is the resolved model ID.
    ModelSwitch { model: String },
    /// Clear conversation history (preserves memories and files).
    ClearSession,
    /// Cancel the current turn's iteration loop.
    StopTurn,
}

/// Configuration for creating new per-chat agent sessions.
pub struct SessionConfig {
    pub bot: Bot,
    pub policy: Policy,
    pub model: String,
    pub max_tokens: u32,
    pub api_key: Option<secrecy::SecretString>,
    /// Provider backend: "anthropic" or "openai".
    pub provider_type: String,
    /// Custom base URL for OpenAI-compatible endpoints.
    pub base_url: Option<String>,
    /// Optional providers config (overrides provider_type/api_key/base_url).
    pub providers_config: Option<ProvidersConfig>,
    /// PostgreSQL connection pool for session persistence, memory, and/or task queue.
    /// Present when `sessions`, `memory`, or `postgres` feature is enabled.
    #[cfg(any(feature = "sessions", feature = "memory", feature = "postgres"))]
    pub db_pool: Option<crate::storage::Pool>,
    /// Shared, build-once tool registry + memory store. The expensive tool
    /// backends (MCP processes, Docker runtime, WASM modules) live here and are
    /// shared across every chat — never spawned per chat.
    pub shared: Arc<crate::app::SharedAgentServices>,
    /// Extended thinking budget in tokens (Anthropic-only, M14a).
    pub thinking_budget: Option<u32>,
    /// Verbose output: send events immediately instead of batching per turn (M14d).
    pub verbose: bool,
    /// Custom system prompt (overrides default coding-assistant prompt).
    pub system_prompt_override: Option<String>,
    /// Task queue store for async approval (autonomous turns).
    /// When set, commit-tier actions during autonomous turns are queued instead of blocking.
    #[cfg(feature = "postgres")]
    pub task_store: Option<std::sync::Arc<dyn crate::storage::TaskStore>>,
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
    /// Route a task queue callback: mark the task approved/rejected and drain.
    #[cfg(feature = "postgres")]
    TaskCallback {
        chat_id: ChatId,
        task_id: uuid::Uuid,
        approved: bool,
    },
}

/// Session manager task. Owns all per-chat sessions and approval routing.
/// Communicates via channels — no Arc<Mutex>.
pub async fn session_manager(
    mut rx: mpsc::Receiver<SessionCommand>,
    config: SessionConfig,
    approval_tx: mpsc::Sender<ApprovalMessage>,
) {
    let mut chat_senders: HashMap<ChatId, mpsc::Sender<InboundMessage>> = HashMap::new();
    let mut chat_cancel_flags: HashMap<ChatId, Arc<AtomicBool>> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            SessionCommand::Message {
                chat_id,
                message: InboundMessage::StopTurn,
            } => {
                if let Some(flag) = chat_cancel_flags.get(&chat_id) {
                    flag.store(true, Ordering::Relaxed);
                    info!(chat_id = %chat_id, "stop signal sent to running turn");
                }
            }
            SessionCommand::Message {
                chat_id,
                message: InboundMessage::ClearSession,
            } => {
                // Set cancel flag first so any running turn exits.
                if let Some(flag) = chat_cancel_flags.get(&chat_id) {
                    flag.store(true, Ordering::Relaxed);
                }
                // Queue the actual clear for after the turn finishes.
                if let Some(sender) = chat_senders.get(&chat_id) {
                    let _ = sender.send(InboundMessage::ClearSession).await;
                }
            }
            SessionCommand::Message { chat_id, message } => {
                // Get or create the per-chat sender.
                let sender = chat_senders.entry(chat_id).or_insert_with(|| {
                    info!(chat_id = %chat_id, "creating new chat session");

                    let (tx, rx) = mpsc::channel::<InboundMessage>(32);
                    let cancel_flag = Arc::new(AtomicBool::new(false));
                    chat_cancel_flags.insert(chat_id, Arc::clone(&cancel_flag));

                    let chat_config = SessionConfig {
                        bot: config.bot.clone(),
                        policy: config.policy.clone(),
                        model: config.model.clone(),
                        max_tokens: config.max_tokens,
                        api_key: config.api_key.clone(),
                        provider_type: config.provider_type.clone(),
                        base_url: config.base_url.clone(),
                        providers_config: config.providers_config.clone(),
                        #[cfg(any(
                            feature = "sessions",
                            feature = "memory",
                            feature = "postgres"
                        ))]
                        db_pool: config.db_pool.clone(),
                        shared: Arc::clone(&config.shared),
                        thinking_budget: config.thinking_budget,
                        verbose: config.verbose,
                        system_prompt_override: config.system_prompt_override.clone(),
                        #[cfg(feature = "postgres")]
                        task_store: config.task_store.clone(),
                    };
                    let approval_tx = approval_tx.clone();

                    tokio::spawn(async move {
                        chat_session(rx, chat_id, chat_config, approval_tx, cancel_flag).await;
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
            #[cfg(feature = "postgres")]
            SessionCommand::TaskCallback {
                chat_id,
                task_id,
                approved,
            } => {
                // Update the task status in the store.
                if let Some(ref store) = config.task_store {
                    let result = if approved {
                        store.mark_approved(task_id).await
                    } else {
                        store.mark_rejected(task_id).await
                    };
                    if let Err(e) = result {
                        warn!(
                            %task_id,
                            error = %e,
                            "failed to update task status (non-fatal)"
                        );
                    }
                }

                // If approved, send a DrainApprovedTasks signal to the chat.
                if approved {
                    if let Some(sender) = chat_senders.get(&chat_id) {
                        let _ = sender.send(InboundMessage::DrainApprovedTasks).await;
                    } else {
                        // No active session yet — create one so it can drain.
                        let sender = chat_senders.entry(chat_id).or_insert_with(|| {
                            info!(chat_id = %chat_id, "creating chat session for task drain");
                            let (tx, rx) = mpsc::channel::<InboundMessage>(32);
                            let cancel_flag = Arc::new(AtomicBool::new(false));
                            chat_cancel_flags.insert(chat_id, Arc::clone(&cancel_flag));
                            let chat_config = SessionConfig {
                                bot: config.bot.clone(),
                                policy: config.policy.clone(),
                                model: config.model.clone(),
                                max_tokens: config.max_tokens,
                                api_key: config.api_key.clone(),
                                provider_type: config.provider_type.clone(),
                                base_url: config.base_url.clone(),
                                providers_config: config.providers_config.clone(),
                                #[cfg(any(
                                    feature = "sessions",
                                    feature = "memory",
                                    feature = "postgres"
                                ))]
                                db_pool: config.db_pool.clone(),
                                shared: Arc::clone(&config.shared),
                                thinking_budget: config.thinking_budget,
                                verbose: config.verbose,
                                system_prompt_override: config.system_prompt_override.clone(),
                                #[cfg(feature = "postgres")]
                                task_store: config.task_store.clone(),
                            };
                            let approval_tx = approval_tx.clone();
                            tokio::spawn(async move {
                                chat_session(rx, chat_id, chat_config, approval_tx, cancel_flag)
                                    .await;
                            });
                            tx
                        });
                        let _ = sender.send(InboundMessage::DrainApprovedTasks).await;
                    }
                }
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
    cancel_flag: Arc<AtomicBool>,
) {
    // Saved for the /model hot-swap path below. The provider itself is built
    // once at startup and shared via `config.shared.provider`.
    let saved_api_key = config.api_key.clone();
    let saved_base_url = config.base_url.clone();

    // Derive user identity from the Telegram chat ID (unique per chat channel).
    let user_id = chat_id.to_string();

    let output = TelegramSink::new(config.bot.clone(), chat_id, config.verbose);

    // Cell shared with the approval gate; filled after build() so queued tasks
    // record the resolved session_id for audit provenance.
    #[cfg(feature = "postgres")]
    let session_id_cell: Arc<std::sync::Mutex<Option<uuid::Uuid>>> =
        Arc::new(std::sync::Mutex::new(None));

    #[cfg(feature = "postgres")]
    let approval_gate = {
        let gate = TelegramApprovalGate::new(config.bot.clone(), chat_id, approval_tx);
        if let Some(ref store) = config.task_store {
            gate.with_task_store(
                std::sync::Arc::clone(store),
                user_id.clone(),
                Arc::clone(&session_id_cell),
            )
        } else {
            gate
        }
    };
    #[cfg(not(feature = "postgres"))]
    let approval_gate = TelegramApprovalGate::new(config.bot.clone(), chat_id, approval_tx);

    // Per-session agent — provider/registry/stores come from the shared services
    // (built once at startup); this layers on the per-chat cancel flag, task
    // store, and persistence slot, then runs the shared `with_*` chain.
    let builder = crate::app::AgentBuilder::new(&config.shared, user_id.clone())
        .cancel_flag(cancel_flag)
        .persistence(crate::app::PersistenceId::Telegram(chat_id.0));
    #[cfg(feature = "postgres")]
    let builder = match &config.task_store {
        Some(store) => builder.task_store(std::sync::Arc::clone(store)),
        None => builder,
    };
    let mut agent = builder.build(approval_gate, output).await;

    // Record the resolved session_id so queued tasks attribute to the right one.
    #[cfg(feature = "postgres")]
    {
        *session_id_cell.lock().expect("session_id_cell poisoned") = Some(agent.session_id());
    }

    while let Some(msg) = rx.recv().await {
        match msg {
            InboundMessage::User {
                content,
                autonomous,
            } => {
                // Set autonomous mode before the turn; it resets to false after.
                #[cfg(feature = "postgres")]
                if autonomous {
                    agent.set_autonomous_mode(true);
                    // Drain any tasks approved since the last cycle before processing new work.
                    let drained = agent.drain_approved_tasks().await;
                    if drained > 0 {
                        info!(chat_id = %chat_id, count = drained, "drained approved tasks before cron turn");
                    }
                }
                if let Err(e) = agent.run_turn(content).await {
                    warn!(chat_id = %chat_id, error = %e, "agent turn error");
                }
            }
            InboundMessage::DrainApprovedTasks => {
                #[cfg(feature = "postgres")]
                {
                    let drained = agent.drain_approved_tasks().await;
                    info!(chat_id = %chat_id, count = drained, "drained approved tasks on callback");
                }
                #[cfg(not(feature = "postgres"))]
                {
                    warn!(chat_id = %chat_id, "DrainApprovedTasks received but postgres feature not enabled");
                }
            }
            InboundMessage::ClearSession => {
                agent.clear_session().await;
                let _ = config
                    .bot
                    .send_message(
                        chat_id,
                        "Session cleared. Memories and files are preserved.",
                    )
                    .await;
            }
            InboundMessage::ModelSwitch { model } => {
                let api_key = saved_api_key.clone();
                let new_provider: Result<Box<dyn crate::providers::Provider>, _> =
                    match config.provider_type.as_str() {
                        "openai" => {
                            OpenAiProvider::new(api_key, &model, config.max_tokens).map(|mut p| {
                                if let Some(url) = &saved_base_url {
                                    p = p.with_base_url(url.clone());
                                }
                                Box::new(p) as Box<dyn crate::providers::Provider>
                            })
                        }
                        _ => match api_key {
                            Some(key) => AnthropicProvider::new(key, &model, config.max_tokens)
                                .map(|p| {
                                    let p = if let Some(budget) = config.thinking_budget {
                                        p.with_thinking_budget(budget)
                                    } else {
                                        p
                                    };
                                    Box::new(p) as Box<dyn crate::providers::Provider>
                                }),
                            None => Err(crate::error::CherubError::Provider(
                                "no API key for model switch".to_owned(),
                            )),
                        },
                    };
                match new_provider {
                    Ok(p) => {
                        let name = p.model_name().to_owned();
                        agent.swap_provider(p);
                        let _ = config
                            .bot
                            .send_message(chat_id, format!("Switched to {name}"))
                            .await;
                    }
                    Err(e) => {
                        warn!(chat_id = %chat_id, error = %e, "model switch failed");
                        let _ = config
                            .bot
                            .send_message(chat_id, format!("Model switch failed: {e}"))
                            .await;
                    }
                }
            }
            InboundMessage::StopTurn => {
                // Handled by session_manager before dispatch.
            }
        }
    }
}
