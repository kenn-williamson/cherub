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
use crate::runtime::AgentLoop;
use crate::runtime::prompt::build_system_prompt;
use crate::tools::ToolRegistry;

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
    /// Embedding provider for hybrid memory search (M6c).
    /// `None` = FTS-only search.
    #[cfg(feature = "memory")]
    pub embedder: Option<Arc<dyn crate::storage::embedding::EmbeddingProvider>>,
    /// Container runtime for sandbox bash. When `Some`, in-process bash is
    /// replaced by a container-sandboxed equivalent.
    #[cfg(feature = "container")]
    pub sandbox_bash_runtime: Option<Arc<dyn crate::tools::container::ContainerRuntime>>,
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
                        #[cfg(feature = "memory")]
                        embedder: config.embedder.clone(),
                        #[cfg(feature = "container")]
                        sandbox_bash_runtime: config.sandbox_bash_runtime.clone(),
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
                                #[cfg(feature = "memory")]
                                embedder: config.embedder.clone(),
                                #[cfg(feature = "container")]
                                sandbox_bash_runtime: config.sandbox_bash_runtime.clone(),
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
    // Keep clones for model-switch later — initial provider construction moves these.
    let saved_api_key = config.api_key.clone();
    let saved_base_url = config.base_url.clone();

    let provider: Box<dyn crate::providers::Provider> = if let Some(ref providers_config) =
        config.providers_config
    {
        // Use config file — instantiate the "default" provider (supports failover).
        if !providers_config.providers.contains_key("default") {
            warn!(chat_id = %chat_id, "providers config missing [providers.default]");
            return;
        }
        match crate::providers::config::instantiate_named_provider(
            providers_config,
            "default",
            &mut Vec::new(),
        ) {
            Ok(p) => p,
            Err(e) => {
                warn!(chat_id = %chat_id, error = %e, "failed to create provider from config");
                return;
            }
        }
    } else {
        match config.provider_type.as_str() {
            "openai" => {
                match OpenAiProvider::new(config.api_key, &config.model, config.max_tokens) {
                    Ok(mut p) => {
                        if let Some(url) = config.base_url {
                            p = p.with_base_url(url);
                        }
                        Box::new(p)
                    }
                    Err(e) => {
                        warn!(chat_id = %chat_id, error = %e, "failed to create OpenAI provider");
                        return;
                    }
                }
            }
            _ => {
                // Default to Anthropic. api_key is required for Anthropic.
                let api_key = match config.api_key {
                    Some(k) => k,
                    None => {
                        warn!(chat_id = %chat_id, "ANTHROPIC_API_KEY required for anthropic provider");
                        return;
                    }
                };
                match AnthropicProvider::new(api_key, &config.model, config.max_tokens) {
                    Ok(p) => {
                        let p = if let Some(budget) = config.thinking_budget {
                            p.with_thinking_budget(budget)
                        } else {
                            p
                        };
                        Box::new(p)
                    }
                    Err(e) => {
                        warn!(chat_id = %chat_id, error = %e, "failed to create Anthropic provider");
                        return;
                    }
                }
            }
        }
    };

    // Derive user identity from the Telegram chat ID (unique per chat channel).
    let user_id = chat_id.to_string();

    // Should we replace in-process bash with container-sandboxed bash?
    #[cfg(feature = "container")]
    let skip_builtin_bash = config.sandbox_bash_runtime.is_some();
    #[cfg(not(feature = "container"))]
    let skip_builtin_bash = false;

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
            let registry = if skip_builtin_bash {
                ToolRegistry::with_memory_no_bash(Arc::clone(&store))
            } else {
                ToolRegistry::with_memory(Arc::clone(&store))
            };
            (registry, Some(store))
        } else if skip_builtin_bash {
            (ToolRegistry::new_without_bash(), None)
        } else {
            (ToolRegistry::new(), None)
        }
    };
    #[cfg(not(feature = "memory"))]
    let registry = if skip_builtin_bash {
        ToolRegistry::new_without_bash()
    } else {
        ToolRegistry::new()
    };

    // Add container-sandboxed bash if runtime is available.
    #[cfg(feature = "container")]
    let (registry, _sandbox_bash_ipc_dir) = {
        if let Some(ref rt) = config.sandbox_bash_runtime {
            let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
            let (bash_tool, ipc_dir) =
                crate::tools::container_bash::build(Arc::clone(rt), workspace);
            let dev_env =
                crate::tools::dev_environment::DevEnvironmentTool::new(Arc::clone(&bash_tool));
            let registry = registry
                .with_container(vec![bash_tool])
                .with_dev_environment(dev_env);
            info!(chat_id = %chat_id, "sandbox bash enabled for chat session");
            (registry, Some(ipc_dir))
        } else {
            (registry, None)
        }
    };

    // Wire sub-agent tools from providers config (M13d).
    let registry = if let Some(ref providers_config) = config.providers_config {
        use crate::providers::config::instantiate_named_provider;
        use crate::tools::sub_agent::SubAgentTool;

        let mut sub_agents: Vec<SubAgentTool> = Vec::new();
        for (agent_name, agent_def) in &providers_config.agents {
            match instantiate_named_provider(providers_config, &agent_def.provider, &mut Vec::new())
            {
                Ok(agent_provider) => {
                    let sub_registry = ToolRegistry::for_sub_agent(&agent_def.tools);
                    sub_agents.push(SubAgentTool {
                        name: agent_name.clone(),
                        description: agent_def.description.clone(),
                        provider: agent_provider,
                        system_prompt: agent_def.system_prompt.clone(),
                        max_turns: agent_def.max_turns,
                        timeout: std::time::Duration::from_secs(agent_def.timeout_secs),
                        registry: sub_registry,
                        policy: config.policy.clone(),
                    });
                    info!(chat_id = %chat_id, agent = %agent_name, "sub-agent tool registered");
                }
                Err(e) => {
                    warn!(
                        chat_id = %chat_id,
                        agent = %agent_name,
                        error = %e,
                        "failed to create sub-agent provider, skipping"
                    );
                }
            }
        }
        if sub_agents.is_empty() {
            registry
        } else {
            registry.with_sub_agents(sub_agents)
        }
    } else {
        registry
    };

    let cwd = std::env::var("CHERUB_WORKSPACE").unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_owned())
    });
    let system_prompt = config
        .system_prompt_override
        .unwrap_or_else(|| build_system_prompt(&cwd));

    let output = TelegramSink::new(config.bot.clone(), chat_id, config.verbose);

    // Shared cell filled in after with_persistence() so queued tasks record the
    // correct session_id for audit provenance.
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

    let mut agent = AgentLoop::new(
        config.policy,
        provider,
        registry,
        system_prompt,
        approval_gate,
        output,
        &user_id,
    );
    agent.with_cancel_flag(cancel_flag);

    // Attach output stashing hook (M15b).
    agent.with_hook(Box::new(crate::runtime::hooks::OutputStashingHook::new(
        std::path::Path::new(&cwd),
    )));

    // Attach proactive memory injection if store is available (M6d).
    #[cfg(feature = "memory")]
    if let Some(store) = memory_store_for_injection {
        agent.with_memory_injection(store);
        info!(chat_id = %chat_id, "proactive memory injection enabled");
    }

    // Attach audit log + cost tracking + pricing table if a pool is available (M10, M12).
    #[cfg(feature = "postgres")]
    if let Some(ref pool) = config.db_pool {
        use crate::storage::PricingStore;
        use crate::storage::pg_audit_store::PgAuditStore;
        use crate::storage::pg_cost_store::PgCostStore;
        use crate::storage::pg_pricing_store::PgPricingStore;

        let audit_store: Arc<dyn crate::storage::AuditStore> =
            Arc::new(PgAuditStore::new(pool.clone()));
        agent.with_audit_log(audit_store);

        let cost_store: Arc<dyn crate::storage::CostStore> =
            Arc::new(PgCostStore::new(pool.clone()));
        agent.with_cost_tracking(cost_store);

        let pricing_store = PgPricingStore::new(pool.clone());
        let pricing_table = pricing_store
            .list()
            .await
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|e| (e.model_pattern.clone(), e.to_model_pricing()))
                    .collect()
            })
            .unwrap_or_default();
        agent.with_pricing_table(pricing_table);
    }

    // Attach task queue store for async approval.
    #[cfg(feature = "postgres")]
    if let Some(ref store) = config.task_store {
        agent.with_task_store(std::sync::Arc::clone(store));
        info!(chat_id = %chat_id, "task queue store attached (async approval enabled)");
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
                let sid = agent.session_id();
                let msg_count = agent.session_messages().len();
                info!(
                    chat_id = %chat_id,
                    session_id = %sid,
                    message_count = msg_count,
                    "session persistence attached"
                );
                // Fill in the shared cell so queued tasks record the correct session_id.
                *session_id_cell.lock().expect("session_id_cell poisoned") = Some(sid);
            }
            Err(e) => {
                warn!(chat_id = %chat_id, error = %e, "session persistence unavailable, running ephemeral");
            }
        }
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
