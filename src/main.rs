use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use secrecy::SecretString;
use tracing::info;
use tracing_subscriber::EnvFilter;

use cherub::enforcement::policy::Policy;
use cherub::providers::anthropic::AnthropicProvider;
use cherub::runtime::AgentLoop;
use cherub::runtime::approval::CliApprovalGate;
use cherub::runtime::output::StdoutSink;
use cherub::runtime::prompt::build_system_prompt;
use cherub::tools::ToolRegistry;

const DEFAULT_POLICY_PATH: &str = "config/default_policy.toml";
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";
const DEFAULT_MAX_TOKENS: u32 = 4096;

// ─── CLI argument parsing ─────────────────────────────────────────────────────

/// Top-level command parsed from `std::env::args()`.
enum Command {
    /// Run the interactive agent REPL.
    Agent {
        policy_path: PathBuf,
        model: String,
        /// Optional directory of WASM tools to load (M8).
        #[cfg(feature = "wasm")]
        wasm_tools_dir: Option<PathBuf>,
        /// Optional directory of container tool configs to load (M9).
        #[cfg(feature = "container")]
        container_tools_dir: Option<PathBuf>,
    },
    /// Credential vault management (M7a).
    #[cfg(feature = "credentials")]
    Credential(CredentialSubcommand),
}

#[cfg(feature = "credentials")]
enum CredentialSubcommand {
    /// Store or update a credential (reads value from stdin).
    Store {
        name: String,
        provider: Option<String>,
        host_patterns: Vec<String>,
        capabilities: Vec<String>,
        location: cherub::storage::CredentialLocation,
        expires_days: Option<u64>,
    },
    /// List all credentials for the current user.
    List,
    /// Delete a named credential.
    Delete { name: String },
}

fn parse_args() -> Result<Command> {
    let args: Vec<String> = std::env::args().collect();

    // Check for credential subcommand before agent args.
    #[cfg(feature = "credentials")]
    if args.get(1).map(|s| s.as_str()) == Some("credential") {
        return parse_credential_args(&args[2..]);
    }

    // Default: agent REPL.
    let mut policy_path = PathBuf::from(DEFAULT_POLICY_PATH);
    let mut model = DEFAULT_MODEL.to_owned();
    #[cfg(feature = "wasm")]
    let mut wasm_tools_dir: Option<PathBuf> = None;
    #[cfg(feature = "container")]
    let mut container_tools_dir: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--policy" => {
                i += 1;
                if i < args.len() {
                    policy_path = PathBuf::from(&args[i]);
                }
            }
            "--model" => {
                i += 1;
                if i < args.len() {
                    model = args[i].clone();
                }
            }
            #[cfg(feature = "wasm")]
            "--wasm-tools-dir" => {
                i += 1;
                if i < args.len() {
                    wasm_tools_dir = Some(PathBuf::from(&args[i]));
                }
            }
            #[cfg(feature = "container")]
            "--container-tools-dir" => {
                i += 1;
                if i < args.len() {
                    container_tools_dir = Some(PathBuf::from(&args[i]));
                }
            }
            _ => {}
        }
        i += 1;
    }

    Ok(Command::Agent {
        policy_path,
        model,
        #[cfg(feature = "wasm")]
        wasm_tools_dir,
        #[cfg(feature = "container")]
        container_tools_dir,
    })
}

#[cfg(feature = "credentials")]
fn parse_credential_args(args: &[String]) -> Result<Command> {
    use cherub::storage::CredentialLocation;

    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "store" => {
            let name = args
                .get(1)
                .cloned()
                .context("usage: cherub credential store <name> [--provider <p>] [--host-patterns <h,...>] [--capabilities <c,...>] [--expires-days <n>] [--location bearer|header:<name>|query:<name>]")?;

            let mut provider = None;
            let mut host_patterns = Vec::new();
            let mut capabilities = Vec::new();
            let mut expires_days: Option<u64> = None;
            let mut location = CredentialLocation::AuthorizationBearer;

            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--provider" => {
                        i += 1;
                        provider = args.get(i).cloned();
                    }
                    "--host-patterns" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            host_patterns = v.split(',').map(|s| s.trim().to_owned()).collect();
                        }
                    }
                    "--capabilities" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            capabilities = v.split(',').map(|s| s.trim().to_owned()).collect();
                        }
                    }
                    "--expires-days" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            expires_days =
                                Some(v.parse().context("--expires-days must be a number")?);
                        }
                    }
                    "--location" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            location = parse_location(v)?;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }

            Ok(Command::Credential(CredentialSubcommand::Store {
                name,
                provider,
                host_patterns,
                capabilities,
                location,
                expires_days,
            }))
        }
        "list" => Ok(Command::Credential(CredentialSubcommand::List)),
        "delete" => {
            let name = args
                .get(1)
                .cloned()
                .context("usage: cherub credential delete <name>")?;
            Ok(Command::Credential(CredentialSubcommand::Delete { name }))
        }
        _ => bail!(
            "unknown credential subcommand '{}'. Available: store, list, delete",
            sub
        ),
    }
}

#[cfg(feature = "credentials")]
fn parse_location(s: &str) -> Result<cherub::storage::CredentialLocation> {
    use cherub::storage::CredentialLocation;
    if s == "bearer" {
        Ok(CredentialLocation::AuthorizationBearer)
    } else if let Some(name) = s.strip_prefix("header:") {
        Ok(CredentialLocation::Header {
            name: name.to_owned(),
            prefix: None,
        })
    } else if let Some(name) = s.strip_prefix("query:") {
        Ok(CredentialLocation::QueryParam {
            name: name.to_owned(),
        })
    } else {
        bail!(
            "unknown location '{}'. Use: bearer | header:<name> | query:<name>",
            s
        )
    }
}

// ─── Credential subcommand handlers ──────────────────────────────────────────

#[cfg(feature = "credentials")]
async fn run_credential_command(sub: CredentialSubcommand) -> Result<()> {
    use std::io::{self, BufRead, Write};

    use cherub::storage::pg_credential_store::PgCredentialStore;
    use cherub::storage::{CredentialStore, NewCredential};
    use std::sync::Arc;

    let db_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL must be set for credential management")?;
    let master_key_raw = std::env::var("CHERUB_MASTER_KEY")
        .context("CHERUB_MASTER_KEY must be set for credential management")?;
    let user_id = std::env::var("USER").unwrap_or_else(|_| "local".to_owned());

    let pool = cherub::storage::connect(SecretString::from(db_url))
        .await
        .context("failed to connect to PostgreSQL")?;

    let store = Arc::new(
        PgCredentialStore::new(pool, SecretString::from(master_key_raw))
            .context("failed to initialize credential store — check CHERUB_MASTER_KEY")?,
    );

    match sub {
        CredentialSubcommand::Store {
            name,
            provider,
            host_patterns,
            capabilities,
            location,
            expires_days,
        } => {
            print!("Enter credential value for '{name}': ");
            io::stdout().flush()?;
            let mut value = String::new();
            io::stdin().lock().read_line(&mut value)?;
            let value = value.trim().to_owned();
            if value.is_empty() {
                bail!("credential value cannot be empty");
            }

            let expires_at =
                expires_days.map(|days| chrono::Utc::now() + chrono::Duration::days(days as i64));

            let id = store
                .store(NewCredential {
                    user_id: user_id.clone(),
                    name: name.clone(),
                    value,
                    provider: provider.clone(),
                    capabilities: capabilities.clone(),
                    host_patterns: host_patterns.clone(),
                    location,
                    expires_at,
                })
                .await
                .context("failed to store credential")?;

            println!("Stored credential '{name}' (id: {id}).");
            if !host_patterns.is_empty() {
                println!("  host patterns: {}", host_patterns.join(", "));
            }
            if !capabilities.is_empty() {
                println!("  capabilities: {}", capabilities.join(", "));
            }
            if let Some(p) = provider {
                println!("  provider: {p}");
            }
        }

        CredentialSubcommand::List => {
            let refs = store
                .list(&user_id)
                .await
                .context("failed to list credentials")?;
            if refs.is_empty() {
                println!("No credentials stored for user '{user_id}'.");
            } else {
                println!("Credentials for '{user_id}':");
                for r in &refs {
                    let provider_str = r.provider.as_deref().unwrap_or("-");
                    let caps = if r.capabilities.is_empty() {
                        "any".to_owned()
                    } else {
                        r.capabilities.join(", ")
                    };
                    let hosts = if r.host_patterns.is_empty() {
                        "any".to_owned()
                    } else {
                        r.host_patterns.join(", ")
                    };
                    println!(
                        "  {:<30} provider={provider_str}  caps=[{caps}]  hosts=[{hosts}]",
                        r.name
                    );
                }
            }
        }

        CredentialSubcommand::Delete { name } => {
            store
                .delete(&user_id, &name)
                .await
                .context(format!("failed to delete credential '{name}'"))?;
            println!("Deleted credential '{name}'.");
        }
    }

    Ok(())
}

// ─── Agent REPL ───────────────────────────────────────────────────────────────

async fn run_agent(
    policy_path: PathBuf,
    model: String,
    #[cfg(feature = "wasm")] wasm_tools_dir: Option<PathBuf>,
    #[cfg(feature = "container")] container_tools_dir: Option<PathBuf>,
) -> Result<()> {
    let user_id = std::env::var("USER").unwrap_or_else(|_| "local".to_owned());

    // Load API key.
    let api_key_raw = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable not set")?;
    if api_key_raw.is_empty() {
        bail!("ANTHROPIC_API_KEY is empty");
    }
    let api_key = SecretString::from(api_key_raw);

    // Load policy.
    let policy = Policy::load(&policy_path).map_err(|e| {
        anyhow::anyhow!("failed to load policy from {}: {e}", policy_path.display())
    })?;
    info!(policy = %policy_path.display(), "policy loaded");

    let provider = AnthropicProvider::new(api_key, &model, DEFAULT_MAX_TOKENS)
        .map_err(|e| anyhow::anyhow!("failed to create provider: {e}"))?;

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_owned());

    // Connect to PostgreSQL if DATABASE_URL is set (needed for sessions, memory, or credentials).
    #[cfg(any(feature = "sessions", feature = "memory", feature = "credentials"))]
    let db_pool = {
        match std::env::var("DATABASE_URL") {
            Ok(db_url_raw) => {
                match cherub::storage::connect(SecretString::from(db_url_raw)).await {
                    Ok(pool) => Some(pool),
                    Err(e) => {
                        eprintln!(
                            "[warn] database connection failed, running without persistence: {e}"
                        );
                        None
                    }
                }
            }
            Err(_) => None,
        }
    };

    // Build ToolRegistry — attach memory store if available.
    #[cfg(feature = "memory")]
    let (registry, memory_store_for_injection) = {
        if let Some(ref pool) = db_pool {
            use cherub::storage::pg_memory_store::PgMemoryStore;
            use std::sync::Arc;

            let store: Arc<dyn cherub::storage::MemoryStore> = match std::env::var("OPENAI_API_KEY")
            {
                Ok(key_raw) if !key_raw.is_empty() => {
                    use cherub::storage::embedding::OpenAiEmbeddingProvider;
                    match OpenAiEmbeddingProvider::new(SecretString::from(key_raw)) {
                        Ok(embedder) => {
                            info!("embedding provider configured (hybrid search enabled)");
                            Arc::new(PgMemoryStore::with_embedder(
                                pool.clone(),
                                Arc::new(embedder),
                            ))
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to create embedding provider, using FTS-only search");
                            Arc::new(PgMemoryStore::new(pool.clone()))
                        }
                    }
                }
                _ => {
                    info!("OPENAI_API_KEY not set, using FTS-only memory search");
                    Arc::new(PgMemoryStore::new(pool.clone()))
                }
            };

            let registry = ToolRegistry::with_memory(Arc::clone(&store));
            (registry, Some(store))
        } else {
            (ToolRegistry::new(), None)
        }
    };
    #[cfg(not(feature = "memory"))]
    let registry = ToolRegistry::new();

    // Attach credential broker + HTTP tool if credentials feature is active.
    #[cfg(feature = "credentials")]
    let registry = {
        use cherub::storage::pg_credential_store::PgCredentialStore;
        use cherub::tools::credential_broker::CredentialBroker;
        use std::sync::Arc;

        match (std::env::var("CHERUB_MASTER_KEY"), &db_pool) {
            (Ok(key_raw), Some(pool)) if !key_raw.is_empty() => {
                match PgCredentialStore::new(pool.clone(), SecretString::from(key_raw)) {
                    Ok(store) => {
                        let cred_store: Arc<dyn cherub::storage::CredentialStore> = Arc::new(store);
                        let broker = Arc::new(CredentialBroker::new(cred_store));
                        info!("credential broker configured (HTTP tool enabled)");
                        registry.with_credentials(broker)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "credential store init failed, HTTP tool disabled");
                        registry
                    }
                }
            }
            _ => {
                info!("CHERUB_MASTER_KEY not set or DB unavailable, HTTP tool disabled");
                registry
            }
        }
    };

    // Load WASM tools if a directory was specified (M8).
    #[cfg(feature = "wasm")]
    let registry = {
        use cherub::tools::wasm::{WasmToolRuntime, load_from_dir};
        use std::sync::Arc;

        if let Some(ref dir) = wasm_tools_dir {
            match WasmToolRuntime::new() {
                Ok(runtime) => {
                    let rt = Arc::new(runtime);
                    let result = load_from_dir(
                        dir,
                        rt,
                        None,
                        #[cfg(feature = "credentials")]
                        None, // broker wiring deferred to M8c full integration
                    )
                    .await;
                    for err in &result.errors {
                        eprintln!("[warn] WASM tool load error: {err}");
                    }
                    if !result.tools.is_empty() {
                        info!(
                            count = result.tools.len(),
                            dir = %dir.display(),
                            "WASM tools loaded"
                        );
                        registry.with_wasm(result.tools)
                    } else {
                        registry
                    }
                }
                Err(e) => {
                    eprintln!("[warn] WASM runtime init failed: {e}");
                    registry
                }
            }
        } else {
            registry
        }
    };

    // Load container tools if a directory was specified (M9).
    #[cfg(feature = "container")]
    let registry = {
        use cherub::tools::container::{BollardRuntime, load_from_dir};
        use std::sync::Arc;

        if let Some(ref dir) = container_tools_dir {
            match BollardRuntime::new() {
                Ok(runtime) => {
                    let rt: Arc<dyn cherub::tools::container::ContainerRuntime> = Arc::new(runtime);
                    let result = load_from_dir(
                        dir,
                        rt,
                        #[cfg(feature = "credentials")]
                        None, // broker wiring deferred to M9c full integration
                    )
                    .await;
                    for err in &result.errors {
                        eprintln!("[warn] container tool load error: {err}");
                    }
                    if !result.tools.is_empty() {
                        info!(
                            count = result.tools.len(),
                            dir = %dir.display(),
                            "container tools loaded"
                        );
                        registry.with_container(result.tools)
                    } else {
                        registry
                    }
                }
                Err(e) => {
                    eprintln!("[warn] container runtime init failed (Docker unavailable?): {e}");
                    registry
                }
            }
        } else {
            registry
        }
    };

    let system_prompt = build_system_prompt(&cwd);

    let approval_gate = CliApprovalGate::new();
    let output = StdoutSink;
    let mut agent = AgentLoop::new(
        policy,
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
        info!("proactive memory injection enabled");
    }

    // Attach session persistence if available.
    #[cfg(feature = "sessions")]
    {
        if let Some(pool) = db_pool {
            use cherub::storage::pg_session_store::PgSessionStore;
            let store = Box::new(PgSessionStore::new(pool));
            match agent.with_persistence(store, "cli", "default").await {
                Ok(()) => {
                    let msg_count = agent.session_messages().len();
                    if msg_count > 0 {
                        println!(
                            "Resumed session {} ({msg_count} messages).",
                            agent.session_id()
                        );
                    } else {
                        println!("New session {}.", agent.session_id());
                    }
                }
                Err(e) => {
                    eprintln!("[warn] session persistence unavailable: {e}");
                }
            }
        }
    }

    info!(model = %model, user_id = %user_id, "cherub started");
    println!("cherub: secure agent runtime (model: {model})");
    println!("Type a message, Ctrl-D to exit, Ctrl-C to cancel input.\n");

    let mut rl = DefaultEditor::new().context("failed to init readline")?;

    loop {
        match rl.readline("you> ") {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);

                if let Err(e) = agent.run_turn_text(line).await {
                    eprintln!("[error] {e}");
                }
                println!();
            }
            Err(ReadlineError::Interrupted) => {
                println!("(Ctrl-C — type a message or Ctrl-D to exit)");
            }
            Err(ReadlineError::Eof) => {
                println!("Goodbye.");
                break;
            }
            Err(e) => {
                bail!("readline error: {e}");
            }
        }
    }

    Ok(())
}

// ─── Entry point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "cherub=info".into()))
        .init();

    match parse_args()? {
        Command::Agent {
            policy_path,
            model,
            #[cfg(feature = "wasm")]
            wasm_tools_dir,
            #[cfg(feature = "container")]
            container_tools_dir,
        } => {
            run_agent(
                policy_path,
                model,
                #[cfg(feature = "wasm")]
                wasm_tools_dir,
                #[cfg(feature = "container")]
                container_tools_dir,
            )
            .await
        }
        #[cfg(feature = "credentials")]
        Command::Credential(sub) => run_credential_command(sub).await,
    }
}
