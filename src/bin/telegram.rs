use std::sync::Arc;

use anyhow::{Context, Result, bail};
use secrecy::SecretString;
use teloxide::prelude::*;
use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::EnvFilter;

use cherub::enforcement::policy::Policy;
use cherub::telegram::approval::{self, ApprovalMessage};
use cherub::telegram::connector;
use cherub::telegram::session::{SessionCommand, SessionConfig};

const DEFAULT_POLICY_PATH: &str = "config/default_policy.toml";
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";
const DEFAULT_MAX_TOKENS: u32 = 4096;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "cherub=info".into()))
        .init();

    // Load bot token
    let bot_token_raw = std::env::var("TELEGRAM_BOT_TOKEN")
        .context("TELEGRAM_BOT_TOKEN environment variable not set")?;
    if bot_token_raw.is_empty() {
        bail!("TELEGRAM_BOT_TOKEN is empty");
    }
    // Note: teloxide Bot::new() requires a plain String; SecretString cannot be used here.

    // Determine provider type.
    let provider_type = std::env::var("CHERUB_PROVIDER").unwrap_or_else(|_| "anthropic".to_owned());
    let base_url = std::env::var("CHERUB_BASE_URL").ok();

    // Load API key — required for Anthropic, optional for OpenAI (local providers).
    let api_key: Option<SecretString> = if provider_type == "openai" {
        std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .map(SecretString::from)
    } else {
        let api_key_raw = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY environment variable not set")?;
        if api_key_raw.is_empty() {
            bail!("ANTHROPIC_API_KEY is empty");
        }
        Some(SecretString::from(api_key_raw))
    };

    // Load policy
    let policy_path = std::env::var("CHERUB_POLICY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(DEFAULT_POLICY_PATH));
    let policy = Policy::load(&policy_path).map_err(|e| {
        anyhow::anyhow!("failed to load policy from {}: {e}", policy_path.display())
    })?;
    info!(policy = %policy_path.display(), "policy loaded");

    // Parse allowed chats (required for security — deny by default).
    let allowed_chats_raw = std::env::var("TELEGRAM_ALLOWED_CHATS")
        .context("TELEGRAM_ALLOWED_CHATS is required. Set to comma-separated chat IDs, or '*' to allow all (not recommended).")?;
    if allowed_chats_raw.is_empty() {
        bail!(
            "TELEGRAM_ALLOWED_CHATS is empty. Set to comma-separated chat IDs, or '*' to allow all."
        );
    }
    let allowed_chats: Option<Vec<i64>> = if allowed_chats_raw.trim() == "*" {
        tracing::warn!("TELEGRAM_ALLOWED_CHATS=* — bot is open to ALL Telegram users");
        None
    } else {
        let ids: Vec<i64> = allowed_chats_raw
            .split(',')
            .map(|s| {
                s.trim()
                    .parse::<i64>()
                    .with_context(|| format!("invalid chat ID: {s:?}"))
            })
            .collect::<Result<Vec<_>>>()?;
        if ids.is_empty() {
            bail!("TELEGRAM_ALLOWED_CHATS parsed to zero chat IDs");
        }
        info!(count = ids.len(), "chat allowlist loaded");
        Some(ids)
    };

    let model = std::env::var("CHERUB_MODEL").unwrap_or_else(|_| {
        if provider_type == "openai" {
            "gpt-4o".to_owned()
        } else {
            DEFAULT_MODEL.to_owned()
        }
    });

    // Connect to PostgreSQL if DATABASE_URL is set (sessions and/or memory).
    #[cfg(any(feature = "sessions", feature = "memory"))]
    let db_pool = {
        match std::env::var("DATABASE_URL") {
            Ok(db_url_raw) => {
                match cherub::storage::connect(secrecy::SecretString::from(db_url_raw)).await {
                    Ok(pool) => {
                        info!("database connected");
                        Some(pool)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "database connection failed, running without persistence");
                        None
                    }
                }
            }
            Err(_) => {
                info!("DATABASE_URL not set, running without persistence");
                None
            }
        }
    };

    // Build embedding provider if OPENAI_API_KEY is set (M6c hybrid search).
    #[cfg(feature = "memory")]
    let embedder: Option<std::sync::Arc<dyn cherub::storage::embedding::EmbeddingProvider>> = {
        match std::env::var("OPENAI_API_KEY") {
            Ok(key_raw) if !key_raw.is_empty() => {
                use cherub::storage::embedding::OpenAiEmbeddingProvider;
                match OpenAiEmbeddingProvider::new(secrecy::SecretString::from(key_raw)) {
                    Ok(e) => {
                        info!("embedding provider configured (hybrid search enabled)");
                        Some(std::sync::Arc::new(e))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to create embedding provider, using FTS-only search");
                        None
                    }
                }
            }
            _ => {
                info!("OPENAI_API_KEY not set, using FTS-only memory search");
                None
            }
        }
    };

    // Check for sandbox bash (container-sandboxed bash replacement).
    #[cfg(feature = "container")]
    let sandbox_bash_runtime: Option<
        std::sync::Arc<dyn cherub::tools::container::ContainerRuntime>,
    > = {
        use cherub::tools::container::BollardRuntime;
        if std::env::var("CHERUB_SANDBOX_BASH").is_ok() {
            let runtime = BollardRuntime::new()
                .context("CHERUB_SANDBOX_BASH requires Docker — failed to connect")?;
            let rt: std::sync::Arc<dyn cherub::tools::container::ContainerRuntime> =
                std::sync::Arc::new(runtime);
            if !rt.is_available().await {
                bail!("CHERUB_SANDBOX_BASH requires Docker but the daemon is not reachable");
            }
            info!("sandbox bash enabled — bash commands will run in isolated containers");
            Some(rt)
        } else {
            None
        }
    };

    // Load providers config if CHERUB_PROVIDERS_CONFIG is set.
    let providers_config = match std::env::var("CHERUB_PROVIDERS_CONFIG") {
        Ok(path) => {
            let config =
                cherub::providers::config::ProvidersConfig::load(std::path::Path::new(&path))
                    .map_err(|e| anyhow::anyhow!("failed to load providers config: {e}"))?;
            info!(config = %path, "providers config loaded");
            Some(config)
        }
        Err(_) => None,
    };

    let bot = Bot::new(&bot_token_raw);
    info!(model = %model, "cherub-telegram starting");

    // Create channels
    let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(256);
    let (approval_tx, approval_rx) = mpsc::channel::<ApprovalMessage>(64);

    // Session config
    let config = SessionConfig {
        bot: bot.clone(),
        policy,
        model,
        max_tokens: DEFAULT_MAX_TOKENS,
        api_key,
        provider_type,
        base_url,
        providers_config,
        #[cfg(any(feature = "sessions", feature = "memory"))]
        db_pool,
        #[cfg(feature = "memory")]
        embedder,
        #[cfg(feature = "container")]
        sandbox_bash_runtime,
    };

    // Spawn session manager and approval manager tasks.
    tokio::spawn(cherub::telegram::session::session_manager(
        session_rx,
        config,
        approval_tx.clone(),
    ));
    tokio::spawn(approval::approval_manager(approval_rx));

    // Set up teloxide dispatcher.
    let allowed_chats = Arc::new(allowed_chats);

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint({
            let session_tx = session_tx.clone();
            let allowed_chats = Arc::clone(&allowed_chats);
            move |bot: Bot, msg: Message| {
                let session_tx = session_tx.clone();
                let allowed_chats = (*allowed_chats).clone();
                async move { connector::handle_message(bot, msg, session_tx, allowed_chats).await }
            }
        }))
        .branch(Update::filter_callback_query().endpoint({
            let session_tx = session_tx.clone();
            move |bot: Bot, query: CallbackQuery| {
                let session_tx = session_tx.clone();
                async move { connector::handle_callback(bot, query, session_tx).await }
            }
        }));

    info!("dispatcher ready, polling for updates...");

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}
