use std::collections::HashMap;
#[cfg(any(feature = "postgres", feature = "memory", feature = "container"))]
use std::sync::Arc;

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
    pub api_key: Option<secrecy::SecretString>,
    /// Provider backend: "anthropic" or "openai".
    pub provider_type: String,
    /// Custom base URL for OpenAI-compatible endpoints.
    pub base_url: Option<String>,
    /// Optional providers config (overrides provider_type/api_key/base_url).
    pub providers_config: Option<ProvidersConfig>,
    /// PostgreSQL connection pool for session persistence and/or memory.
    /// Present when `sessions` or `memory` feature is enabled.
    #[cfg(any(feature = "sessions", feature = "memory"))]
    pub db_pool: Option<deadpool_postgres::Pool>,
    /// Embedding provider for hybrid memory search (M6c).
    /// `None` = FTS-only search.
    #[cfg(feature = "memory")]
    pub embedder: Option<Arc<dyn crate::storage::embedding::EmbeddingProvider>>,
    /// Container runtime for sandbox bash. When `Some`, in-process bash is
    /// replaced by a container-sandboxed equivalent.
    #[cfg(feature = "container")]
    pub sandbox_bash_runtime: Option<Arc<dyn crate::tools::container::ContainerRuntime>>,
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
                        provider_type: config.provider_type.clone(),
                        base_url: config.base_url.clone(),
                        providers_config: config.providers_config.clone(),
                        #[cfg(any(feature = "sessions", feature = "memory"))]
                        db_pool: config.db_pool.clone(),
                        #[cfg(feature = "memory")]
                        embedder: config.embedder.clone(),
                        #[cfg(feature = "container")]
                        sandbox_bash_runtime: config.sandbox_bash_runtime.clone(),
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
    let provider: Box<dyn crate::providers::Provider> = if let Some(ref providers_config) =
        config.providers_config
    {
        // Use config file — instantiate the "default" provider.
        let default_def = match providers_config.providers.get("default") {
            Some(def) => def,
            None => {
                warn!(chat_id = %chat_id, "providers config missing [providers.default]");
                return;
            }
        };
        match crate::providers::config::instantiate_provider(default_def) {
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
                    Ok(p) => Box::new(p),
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
