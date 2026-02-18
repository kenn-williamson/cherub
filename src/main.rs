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

fn parse_args() -> (PathBuf, String) {
    let args: Vec<String> = std::env::args().collect();
    let mut policy_path = PathBuf::from(DEFAULT_POLICY_PATH);
    let mut model = DEFAULT_MODEL.to_owned();

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
            _ => {}
        }
        i += 1;
    }

    (policy_path, model)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "cherub=info".into()))
        .init();

    let (policy_path, model) = parse_args();

    // Load API key
    let api_key_raw = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable not set")?;
    if api_key_raw.is_empty() {
        bail!("ANTHROPIC_API_KEY is empty");
    }
    let api_key = SecretString::from(api_key_raw);

    // Load policy
    let policy = Policy::load(&policy_path).map_err(|e| {
        anyhow::anyhow!("failed to load policy from {}: {e}", policy_path.display())
    })?;
    info!(policy = %policy_path.display(), "policy loaded");

    // Build components
    let provider = AnthropicProvider::new(api_key, &model, DEFAULT_MAX_TOKENS)
        .map_err(|e| anyhow::anyhow!("failed to create provider: {e}"))?;
    let registry = ToolRegistry::new();

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_owned());
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
    );

    // Attach session persistence if DATABASE_URL is set and the `sessions` feature is compiled in.
    #[cfg(feature = "sessions")]
    {
        if let Ok(db_url_raw) = std::env::var("DATABASE_URL") {
            match cherub::storage::connect(SecretString::from(db_url_raw)).await {
                Ok(pool) => {
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
                Err(e) => {
                    eprintln!(
                        "[warn] database connection failed, running without persistence: {e}"
                    );
                }
            }
        }
    }

    info!(model = %model, "cherub started");
    println!("cherub: secure agent runtime (model: {model})");
    println!("Type a message, Ctrl-D to exit, Ctrl-C to cancel input.\n");

    // REPL
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
