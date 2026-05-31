use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use secrecy::SecretString;
use tracing::info;
use tracing_subscriber::EnvFilter;

use cherub::enforcement::policy::Policy;
use cherub::runtime::approval::CliApprovalGate;
use cherub::runtime::output::StdoutSink;
use cherub::runtime::prompt::build_system_prompt;

const DEFAULT_POLICY_PATH: &str = "config/default_policy.toml";
/// The default policy, embedded at compile time. Lets an installed binary
/// (`cargo install cherub`) start with no config file on disk: when the user
/// passes no `--policy` and `config/default_policy.toml` is absent, this is the
/// fallback. The file ships in the published crate, so it compiles either way.
const EMBEDDED_DEFAULT_POLICY: &str = include_str!("../config/default_policy.toml");
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_MAX_TOKENS: u32 = 4096;

// ─── CLI argument parsing ─────────────────────────────────────────────────────

/// Top-level command parsed from `std::env::args()`.
enum Command {
    /// Run the interactive agent REPL.
    Agent {
        policy_path: PathBuf,
        model: String,
        /// Provider backend: "anthropic" or "openai".
        provider: String,
        /// Custom base URL for OpenAI-compatible endpoints (Ollama, vLLM, etc.).
        base_url: Option<String>,
        /// Provider configuration file (TOML). Overrides --provider/--base-url/--model.
        providers_config: Option<PathBuf>,
        /// Optional directory of WASM tools to load (M8).
        #[cfg(feature = "wasm")]
        wasm_tools_dir: Option<PathBuf>,
        /// Optional directory of container tool configs to load (M9).
        #[cfg(feature = "container")]
        container_tools_dir: Option<PathBuf>,
        /// Replace in-process bash with a container-sandboxed equivalent.
        #[cfg(feature = "container")]
        sandbox_bash: bool,
        /// Enable the Playwright browser tool (container-sandboxed Chromium).
        #[cfg(feature = "browser")]
        browser: bool,
        /// Optional MCP servers config file (M11).
        #[cfg(feature = "mcp")]
        mcp_config: Option<PathBuf>,
        /// Optional schedule config file for cron-triggered turns.
        #[cfg(feature = "schedule")]
        schedule_config: Option<PathBuf>,
        /// Extended thinking budget in tokens (Anthropic-only, M14a).
        thinking_budget: Option<u32>,
        /// Whether to show thinking blocks in output (M14a).
        show_thinking: bool,
    },
    /// Credential vault management (M7a).
    #[cfg(feature = "credentials")]
    Credential(CredentialSubcommand),
    /// Audit log queries (M10).
    #[cfg(feature = "postgres")]
    Audit(AuditSubcommand),
    /// Cost tracking queries (M12).
    #[cfg(feature = "postgres")]
    Cost(CostSubcommand),
    /// Model pricing management (DB-backed pricing table).
    #[cfg(feature = "postgres")]
    Pricing(PricingSubcommand),
}

/// Audit log subcommands.
#[cfg(feature = "postgres")]
enum AuditSubcommand {
    /// List recent audit events with optional filters.
    List {
        tool: Option<String>,
        decision: Option<String>,
        user_id: Option<String>,
        session_id: Option<String>,
        limit: Option<i64>,
    },
}

/// Cost tracking subcommands.
#[cfg(feature = "postgres")]
enum CostSubcommand {
    /// Show cost summary (session, today, this month).
    Summary,
    /// Show daily cost breakdown for the last N days.
    History { days: u32 },
}

/// Model pricing subcommands.
#[cfg(feature = "postgres")]
enum PricingSubcommand {
    /// List all pricing entries.
    List,
    /// Upsert a pricing entry.
    Set {
        pattern: String,
        input: f64,
        output: f64,
        cache_write: f64,
        cache_read: f64,
    },
    /// Delete a pricing entry.
    Delete { pattern: String },
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

    // Check for audit subcommand.
    #[cfg(feature = "postgres")]
    if args.get(1).map(|s| s.as_str()) == Some("audit") {
        return parse_audit_args(&args[2..]);
    }

    // Check for cost subcommand.
    #[cfg(feature = "postgres")]
    if args.get(1).map(|s| s.as_str()) == Some("cost") {
        return parse_cost_args(&args[2..]);
    }

    // Check for pricing subcommand.
    #[cfg(feature = "postgres")]
    if args.get(1).map(|s| s.as_str()) == Some("pricing") {
        return parse_pricing_args(&args[2..]);
    }

    // Default: agent REPL.
    let mut policy_path = PathBuf::from(DEFAULT_POLICY_PATH);
    let mut model: Option<String> = None;
    let mut provider = "anthropic".to_owned();
    let mut base_url: Option<String> = None;
    #[cfg(feature = "wasm")]
    let mut wasm_tools_dir: Option<PathBuf> = None;
    #[cfg(feature = "container")]
    let mut container_tools_dir: Option<PathBuf> = None;
    #[cfg(feature = "container")]
    let mut sandbox_bash = false;
    #[cfg(feature = "browser")]
    let mut browser = false;
    #[cfg(feature = "mcp")]
    let mut mcp_config: Option<PathBuf> = None;
    #[cfg(feature = "schedule")]
    let mut schedule_config: Option<PathBuf> = None;
    let mut providers_config: Option<PathBuf> = None;
    let mut thinking_budget: Option<u32> = None;
    let mut show_thinking = false;

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
                    model = Some(args[i].clone());
                }
            }
            "--provider" => {
                i += 1;
                if i < args.len() {
                    provider = args[i].clone();
                }
            }
            "--base-url" => {
                i += 1;
                if i < args.len() {
                    base_url = Some(args[i].clone());
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
            #[cfg(feature = "container")]
            "--sandbox-bash" => {
                sandbox_bash = true;
            }
            #[cfg(feature = "browser")]
            "--browser" => {
                browser = true;
            }
            #[cfg(feature = "mcp")]
            "--mcp-config" => {
                i += 1;
                if i < args.len() {
                    mcp_config = Some(PathBuf::from(&args[i]));
                }
            }
            #[cfg(feature = "schedule")]
            "--schedule" => {
                i += 1;
                if i < args.len() {
                    schedule_config = Some(PathBuf::from(&args[i]));
                }
            }
            "--providers" => {
                i += 1;
                if i < args.len() {
                    providers_config = Some(PathBuf::from(&args[i]));
                }
            }
            "--thinking-budget" => {
                i += 1;
                if i < args.len() {
                    thinking_budget = args[i].parse().ok();
                }
            }
            "--show-thinking" => {
                show_thinking = true;
            }
            _ => {}
        }
        i += 1;
    }

    // Default model depends on provider.
    let model = model.unwrap_or_else(|| {
        if provider == "openai" {
            "gpt-4o".to_owned()
        } else {
            DEFAULT_MODEL.to_owned()
        }
    });

    Ok(Command::Agent {
        policy_path,
        model,
        provider,
        base_url,
        providers_config,
        #[cfg(feature = "wasm")]
        wasm_tools_dir,
        #[cfg(feature = "container")]
        container_tools_dir,
        #[cfg(feature = "container")]
        sandbox_bash,
        #[cfg(feature = "browser")]
        browser,
        #[cfg(feature = "mcp")]
        mcp_config,
        #[cfg(feature = "schedule")]
        schedule_config,
        thinking_budget,
        show_thinking,
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

// ─── Audit subcommand ─────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
fn parse_audit_args(args: &[String]) -> Result<Command> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "list" => {
            let mut tool: Option<String> = None;
            let mut decision: Option<String> = None;
            let mut user_id: Option<String> = None;
            let mut session_id: Option<String> = None;
            let mut limit: Option<i64> = None;

            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--tool" => {
                        i += 1;
                        tool = args.get(i).cloned();
                    }
                    "--decision" => {
                        i += 1;
                        decision = args.get(i).cloned();
                    }
                    "--user" => {
                        i += 1;
                        user_id = args.get(i).cloned();
                    }
                    "--session" => {
                        i += 1;
                        session_id = args.get(i).cloned();
                    }
                    "--limit" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            limit = Some(v.parse().context("--limit must be a number")?);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }

            Ok(Command::Audit(AuditSubcommand::List {
                tool,
                decision,
                user_id,
                session_id,
                limit,
            }))
        }
        _ => anyhow::bail!("unknown audit subcommand '{}'. Available: list", sub),
    }
}

#[cfg(feature = "postgres")]
async fn run_audit_command(sub: AuditSubcommand) -> Result<()> {
    use cherub::storage::pg_audit_store::PgAuditStore;
    use cherub::storage::{AuditDecision, AuditFilter, AuditStore};
    use std::str::FromStr;
    use std::sync::Arc;
    use uuid::Uuid;

    let db_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL must be set for audit log queries")?;

    let pool = cherub::storage::connect(SecretString::from(db_url))
        .await
        .context("failed to connect to PostgreSQL")?;

    let store: Arc<dyn AuditStore> = Arc::new(PgAuditStore::new(pool));

    match sub {
        AuditSubcommand::List {
            tool,
            decision,
            user_id,
            session_id,
            limit,
        } => {
            let parsed_decision = decision
                .as_deref()
                .map(AuditDecision::from_str)
                .transpose()
                .context("invalid --decision value; use: allow, reject, escalate, approve, deny")?;

            let parsed_session = session_id
                .as_deref()
                .map(Uuid::parse_str)
                .transpose()
                .context("invalid --session value; must be a UUID")?;

            let filter = AuditFilter {
                tool: tool.clone(),
                decision: parsed_decision,
                user_id: user_id.clone(),
                session_id: parsed_session,
                since: None,
                limit,
            };

            let events = store
                .list(filter)
                .await
                .context("failed to query audit log")?;

            if events.is_empty() {
                println!("No audit events found.");
            } else {
                println!(
                    "{:<26}  {:<10}  {:<8}  {:<10}  action",
                    "timestamp", "tool", "decision", "tier"
                );
                println!("{}", "-".repeat(80));
                for ev in &events {
                    let ts = ev.created_at.format("%Y-%m-%d %H:%M:%S%.3f");
                    let tier = ev.tier.as_deref().unwrap_or("-");
                    let action = ev.action.as_deref().unwrap_or("-");
                    println!(
                        "{ts:<26}  {:<10}  {:<8}  {:<10}  {}",
                        ev.tool, ev.decision, tier, action
                    );
                }
                println!("\n{} event(s) shown.", events.len());
            }
        }
    }

    Ok(())
}

// ─── Cost subcommand ──────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
fn parse_cost_args(args: &[String]) -> Result<Command> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "summary" => Ok(Command::Cost(CostSubcommand::Summary)),
        "history" => {
            let mut days: u32 = 7;
            let mut i = 1;
            while i < args.len() {
                if args[i].as_str() == "--days" {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        days = v.parse().context("--days must be a positive number")?;
                    }
                }
                i += 1;
            }
            Ok(Command::Cost(CostSubcommand::History { days }))
        }
        _ => anyhow::bail!(
            "unknown cost subcommand '{}'. Available: summary, history",
            sub
        ),
    }
}

#[cfg(feature = "postgres")]
async fn run_cost_command(sub: CostSubcommand) -> Result<()> {
    use cherub::storage::CostStore;
    use cherub::storage::pg_cost_store::PgCostStore;
    use chrono::Datelike;
    use std::sync::Arc;

    let db_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL must be set for cost queries")?;
    let user_id = std::env::var("USER").unwrap_or_else(|_| "local".to_owned());

    let pool = cherub::storage::connect(SecretString::from(db_url))
        .await
        .context("failed to connect to PostgreSQL")?;

    let store: Arc<dyn CostStore> = Arc::new(PgCostStore::new(pool));

    match sub {
        CostSubcommand::Summary => {
            let today_start = chrono::Utc::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc();
            let month_start = chrono::Utc::now()
                .date_naive()
                .with_day(1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc();

            let today = store
                .period_cost(&user_id, today_start)
                .await
                .context("failed to query today's cost")?;
            let month = store
                .period_cost(&user_id, month_start)
                .await
                .context("failed to query this month's cost")?;

            println!("Cost summary for user '{user_id}':");
            println!(
                "  Today:         ${:.2}  ({} input + {} output tokens, {} calls)",
                today.total_cost_usd,
                format_tokens(today.total_input_tokens),
                format_tokens(today.total_output_tokens),
                today.call_count,
            );
            println!(
                "  This month:    ${:.2}  ({} input + {} output tokens, {} calls)",
                month.total_cost_usd,
                format_tokens(month.total_input_tokens),
                format_tokens(month.total_output_tokens),
                month.call_count,
            );
        }
        CostSubcommand::History { days } => {
            let daily = store
                .daily_costs(&user_id, days)
                .await
                .context("failed to query daily costs")?;

            if daily.is_empty() {
                println!("No cost data found for the last {days} days.");
            } else {
                println!(
                    "{:<12}  {:>5}  {:>14}  {:>14}  {:>10}",
                    "Date", "Calls", "Input Tokens", "Output Tokens", "Cost USD"
                );
                println!("{}", "-".repeat(62));
                for d in &daily {
                    println!(
                        "{:<12}  {:>5}  {:>14}  {:>14}  ${:>9.2}",
                        d.date,
                        d.call_count,
                        format_tokens(d.total_input_tokens),
                        format_tokens(d.total_output_tokens),
                        d.total_cost_usd,
                    );
                }
            }
        }
    }

    Ok(())
}

/// Format token counts with comma separators for readability.
#[cfg(feature = "postgres")]
fn format_tokens(n: i64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

// ─── Pricing subcommand ──────────────────────────────────────────────────

#[cfg(feature = "postgres")]
fn parse_pricing_args(args: &[String]) -> Result<Command> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "list" => Ok(Command::Pricing(PricingSubcommand::List)),
        "set" => {
            let pattern = args
                .get(1)
                .cloned()
                .context("usage: cherub pricing set <pattern> --input <f> --output <f> [--cache-write <f>] [--cache-read <f>]")?;
            let mut input: Option<f64> = None;
            let mut output: Option<f64> = None;
            let mut cache_write: f64 = 0.0;
            let mut cache_read: f64 = 0.0;

            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--input" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            input = Some(v.parse().context("--input must be a number")?);
                        }
                    }
                    "--output" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            output = Some(v.parse().context("--output must be a number")?);
                        }
                    }
                    "--cache-write" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            cache_write = v.parse().context("--cache-write must be a number")?;
                        }
                    }
                    "--cache-read" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            cache_read = v.parse().context("--cache-read must be a number")?;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }

            let input = input.context("--input is required")?;
            let output = output.context("--output is required")?;

            Ok(Command::Pricing(PricingSubcommand::Set {
                pattern,
                input,
                output,
                cache_write,
                cache_read,
            }))
        }
        "delete" => {
            let pattern = args
                .get(1)
                .cloned()
                .context("usage: cherub pricing delete <pattern>")?;
            Ok(Command::Pricing(PricingSubcommand::Delete { pattern }))
        }
        _ => anyhow::bail!(
            "unknown pricing subcommand '{}'. Available: list, set, delete",
            sub
        ),
    }
}

#[cfg(feature = "postgres")]
async fn run_pricing_command(sub: PricingSubcommand) -> Result<()> {
    use cherub::storage::pg_pricing_store::PgPricingStore;
    use cherub::storage::{PricingEntry, PricingStore};

    let db_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL must be set for pricing management")?;

    let pool = cherub::storage::connect(SecretString::from(db_url))
        .await
        .context("failed to connect to PostgreSQL")?;

    let store = PgPricingStore::new(pool);

    match sub {
        PricingSubcommand::List => {
            let entries = store
                .list()
                .await
                .context("failed to list pricing entries")?;
            if entries.is_empty() {
                println!("No pricing entries configured.");
                println!(
                    "Use 'cherub pricing set <pattern> --input <f> --output <f>' to add rates."
                );
            } else {
                println!(
                    "{:<30}  {:>10}  {:>10}  {:>12}  {:>12}",
                    "Model Pattern", "Input/MTok", "Output/MTok", "CacheWr/MTok", "CacheRd/MTok"
                );
                println!("{}", "-".repeat(80));
                for e in &entries {
                    println!(
                        "{:<30}  ${:>9.4}  ${:>9.4}  ${:>11.4}  ${:>11.4}",
                        e.model_pattern,
                        e.input_per_mtok,
                        e.output_per_mtok,
                        e.cache_write_per_mtok,
                        e.cache_read_per_mtok,
                    );
                }
                println!("\n{} entry/entries.", entries.len());
            }
        }
        PricingSubcommand::Set {
            pattern,
            input,
            output,
            cache_write,
            cache_read,
        } => {
            store
                .set(PricingEntry {
                    model_pattern: pattern.clone(),
                    input_per_mtok: input,
                    output_per_mtok: output,
                    cache_write_per_mtok: cache_write,
                    cache_read_per_mtok: cache_read,
                })
                .await
                .context("failed to set pricing entry")?;
            println!(
                "Set pricing for '{}': input=${}/MTok, output=${}/MTok, cache_write=${}/MTok, cache_read=${}/MTok",
                pattern, input, output, cache_write, cache_read
            );
        }
        PricingSubcommand::Delete { pattern } => {
            let deleted = store
                .delete(&pattern)
                .await
                .context("failed to delete pricing entry")?;
            if deleted {
                println!("Deleted pricing for '{pattern}'.");
            } else {
                println!("No pricing entry found for '{pattern}'.");
            }
        }
    }

    Ok(())
}

// ─── Agent REPL ───────────────────────────────────────────────────────────────

// TODO: bundle args into a RunAgentConfig struct
#[allow(clippy::too_many_arguments)]
async fn run_agent(
    policy_path: PathBuf,
    model: String,
    provider_type: String,
    base_url: Option<String>,
    providers_config: Option<PathBuf>,
    #[cfg(feature = "wasm")] wasm_tools_dir: Option<PathBuf>,
    #[cfg(feature = "container")] container_tools_dir: Option<PathBuf>,
    #[cfg(feature = "container")] sandbox_bash: bool,
    #[cfg(feature = "browser")] browser: bool,
    #[cfg(feature = "mcp")] mcp_config: Option<PathBuf>,
    #[cfg(feature = "schedule")] schedule_config: Option<PathBuf>,
    thinking_budget: Option<u32>,
    show_thinking: bool,
) -> Result<()> {
    let user_id = std::env::var("USER").unwrap_or_else(|_| "local".to_owned());

    // Load policy. If a file exists at the path, use it. If not — and the user
    // didn't point at a specific file (still the default path) — fall back to the
    // policy embedded at compile time, so an installed binary run outside a
    // checkout still starts (deny-by-default, never policy-free). An explicitly
    // requested `--policy` file that's missing remains a hard error.
    let policy = if policy_path.exists() {
        let p = Policy::load(&policy_path).map_err(|e| {
            anyhow::anyhow!("failed to load policy from {}: {e}", policy_path.display())
        })?;
        info!(policy = %policy_path.display(), "policy loaded");
        p
    } else if policy_path == Path::new(DEFAULT_POLICY_PATH) {
        let p = EMBEDDED_DEFAULT_POLICY
            .parse::<Policy>()
            .map_err(|e| anyhow::anyhow!("embedded default policy failed to parse: {e}"))?;
        info!("no policy file found; using embedded default policy");
        p
    } else {
        bail!(
            "failed to load policy from {}: no such file",
            policy_path.display()
        );
    };

    // Create provider — from config file if --providers is set, otherwise from CLI flags.
    // Keep the parsed config around for sub-agent wiring (M13d).
    let loaded_providers_config = if let Some(ref config_path) = providers_config {
        use cherub::providers::config::ProvidersConfig;
        let config = ProvidersConfig::load(config_path)
            .map_err(|e| anyhow::anyhow!("failed to load providers config: {e}"))?;
        info!(config = %config_path.display(), "providers config loaded");
        if !config.providers.contains_key("default") {
            bail!("providers config must contain a [providers.default] entry");
        }
        Some(config)
    } else {
        None
    };

    // Provider spec — from config file (failover) or raw flags. Actual provider
    // construction is deferred to SharedAgentServices::build.
    let provider_spec = if let Some(ref config) = loaded_providers_config {
        cherub::app::ProviderSpec::Named(config.clone())
    } else {
        let api_key = match provider_type.as_str() {
            "openai" => {
                // OPENAI_API_KEY is optional for local providers (Ollama, etc.).
                std::env::var("OPENAI_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty())
                    .map(SecretString::from)
            }
            "anthropic" => {
                let api_key_raw = std::env::var("ANTHROPIC_API_KEY")
                    .context("ANTHROPIC_API_KEY environment variable not set")?;
                if api_key_raw.is_empty() {
                    bail!("ANTHROPIC_API_KEY is empty");
                }
                Some(SecretString::from(api_key_raw))
            }
            other => bail!("unknown provider '{other}'. Available: anthropic, openai"),
        };
        cherub::app::ProviderSpec::Flags {
            provider_type: provider_type.clone(),
            model: model.clone(),
            api_key,
            base_url,
            thinking_budget,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    };

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_owned());

    // Connect to PostgreSQL if DATABASE_URL is set (needed for sessions, memory, credentials, or task queue).
    #[cfg(any(
        feature = "sessions",
        feature = "memory",
        feature = "credentials",
        feature = "postgres"
    ))]
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

    // Should we replace in-process bash with container-sandboxed bash?
    #[cfg(feature = "container")]
    let skip_builtin_bash = sandbox_bash;
    #[cfg(not(feature = "container"))]
    let skip_builtin_bash = false;

    // Build the shared memory store (if a DB pool is available). The registry is
    // assembled by `app::build_registry`; here we only resolve the store +
    // embedder so the same handle can also drive proactive injection (M6d).
    #[cfg(feature = "memory")]
    let memory_store_for_injection: Option<std::sync::Arc<dyn cherub::storage::MemoryStore>> =
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
            Some(store)
        } else {
            None
        };

    // Build the shared credential broker (if master key + DB are present).
    #[cfg(feature = "credentials")]
    let credential_broker: Option<
        std::sync::Arc<cherub::tools::credential_broker::CredentialBroker>,
    > = {
        use cherub::storage::pg_credential_store::PgCredentialStore;
        use cherub::tools::credential_broker::CredentialBroker;
        use std::sync::Arc;

        match (std::env::var("CHERUB_MASTER_KEY"), &db_pool) {
            (Ok(key_raw), Some(pool)) if !key_raw.is_empty() => {
                match PgCredentialStore::new(pool.clone(), SecretString::from(key_raw)) {
                    Ok(store) => {
                        let cred_store: Arc<dyn cherub::storage::CredentialStore> = Arc::new(store);
                        Some(Arc::new(CredentialBroker::new(cred_store)))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "credential store init failed, HTTP tool disabled");
                        None
                    }
                }
            }
            _ => {
                info!("CHERUB_MASTER_KEY not set or DB unavailable, HTTP tool disabled");
                None
            }
        }
    };

    // Resolve the system prompt (CLI override file, else the shared default).
    let system_prompt = std::env::var("CHERUB_SYSTEM_PROMPT_FILE")
        .ok()
        .and_then(|path| {
            std::fs::read_to_string(&path)
                .map_err(|e| {
                    tracing::warn!(path = %path, error = %e, "failed to read system prompt file, using default");
                    e
                })
                .ok()
        })
        .unwrap_or_else(|| build_system_prompt(&cwd));

    // Assemble the shared services (provider + tool registry + stores) once,
    // identical to the path the Telegram bot uses — no per-transport drift.
    let agent_config = cherub::app::AgentConfig {
        provider: provider_spec,
        policy: policy.clone(),
        system_prompt,
        cwd,
        skip_builtin_bash,
        user_id: user_id.clone(),
        providers_config: loaded_providers_config,
        #[cfg(feature = "postgres")]
        db_pool,
        #[cfg(feature = "memory")]
        memory_store: memory_store_for_injection,
        #[cfg(feature = "credentials")]
        credential_broker,
        #[cfg(feature = "wasm")]
        wasm_dir: wasm_tools_dir,
        #[cfg(feature = "container")]
        container_tools_dir,
        #[cfg(feature = "container")]
        enable_sandbox_bash: sandbox_bash,
        #[cfg(feature = "browser")]
        enable_browser: browser,
        #[cfg(feature = "mcp")]
        mcp_config,
    };
    // Remember whether persistence will be attempted (the pool moves into shared).
    #[cfg(feature = "sessions")]
    let has_db = agent_config.db_pool.is_some();
    let shared = cherub::app::SharedAgentServices::build(agent_config).await?;

    let mut agent = cherub::app::AgentBuilder::new(&shared, &user_id)
        .persistence(cherub::app::PersistenceId::Cli)
        .show_thinking(show_thinking)
        .build(CliApprovalGate::new(), StdoutSink)
        .await;

    // Session banner (only when persistence was actually attempted).
    #[cfg(feature = "sessions")]
    if has_db {
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

    // ── Schedule runner setup (feature = "schedule") ─────────────────────────
    #[cfg(feature = "schedule")]
    let schedule_rx: Option<
        tokio::sync::mpsc::Receiver<cherub::runtime::schedule::ScheduledMessage>,
    > = {
        use cherub::runtime::schedule::{ScheduleConfig, parse_entries, schedule_runner};
        if let Some(ref path) = schedule_config {
            let path_str = path
                .to_str()
                .context("schedule config path is not valid UTF-8")?;
            let config = ScheduleConfig::load(path_str)?;
            let parsed = parse_entries(&config.schedules)?;
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(schedule_runner(parsed, tx));
            info!(config = %path.display(), "schedule runner started");
            Some(rx)
        } else {
            None
        }
    };

    info!(model = %model, user_id = %user_id, "cherub started");
    println!("cherub: secure agent runtime (model: {model})");
    println!("Type a message, Ctrl-D to exit, Ctrl-C to cancel input.\n");

    let mut rl = DefaultEditor::new().context("failed to init readline")?;

    // ── Schedule-enabled path ─────────────────────────────────────────────────
    // When a schedule channel is present, use a dedicated readline task that
    // permanently owns `rl` and sends results through an mpsc channel, so we
    // can `select!` on two receivers without any ownership juggling.
    #[cfg(feature = "schedule")]
    if let Some(mut sched_rx) = schedule_rx {
        let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<rustyline::Result<String>>(1);

        // Readline task: owns `rl` for the session lifetime.
        tokio::task::spawn_blocking(move || {
            loop {
                let result = rl.readline("you> ");
                match &result {
                    Ok(line) if !line.trim().is_empty() => {
                        let _ = rl.add_history_entry(line.trim());
                    }
                    _ => {}
                }
                let is_done = result.is_err();
                if line_tx.blocking_send(result).is_err() || is_done {
                    break;
                }
            }
        });

        'sched: loop {
            tokio::select! {
                line_result = line_rx.recv() => {
                    match line_result {
                        Some(Ok(line)) => {
                            let line = line.trim();
                            if line.is_empty() { continue; }
                            if let Err(e) = agent.run_turn_text(line).await {
                                eprintln!("[error] {e}");
                            }
                            println!();
                        }
                        Some(Err(ReadlineError::Interrupted)) => {
                            println!("(Ctrl-C — type a message or Ctrl-D to exit)");
                        }
                        Some(Err(ReadlineError::Eof)) | None => {
                            println!("Goodbye.");
                            break 'sched;
                        }
                        Some(Err(e)) => {
                            bail!("readline error: {e}");
                        }
                    }
                }
                msg = sched_rx.recv() => {
                    if let Some(msg) = msg {
                        println!("\n[schedule: {}] {}", msg.name, msg.message);
                        if let Err(e) = agent.run_turn_text(&msg.message).await {
                            eprintln!("[error] {e}");
                        }
                        println!();
                    }
                    // msg == None means schedule runner stopped; keep going with readline only.
                }
            }
        }
        return Ok(());
    }

    // ── Default path (no schedule) ────────────────────────────────────────────
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
            provider,
            base_url,
            providers_config,
            #[cfg(feature = "wasm")]
            wasm_tools_dir,
            #[cfg(feature = "container")]
            container_tools_dir,
            #[cfg(feature = "container")]
            sandbox_bash,
            #[cfg(feature = "browser")]
            browser,
            #[cfg(feature = "mcp")]
            mcp_config,
            #[cfg(feature = "schedule")]
            schedule_config,
            thinking_budget,
            show_thinking,
        } => {
            run_agent(
                policy_path,
                model,
                provider,
                base_url,
                providers_config,
                #[cfg(feature = "wasm")]
                wasm_tools_dir,
                #[cfg(feature = "container")]
                container_tools_dir,
                #[cfg(feature = "container")]
                sandbox_bash,
                #[cfg(feature = "browser")]
                browser,
                #[cfg(feature = "mcp")]
                mcp_config,
                #[cfg(feature = "schedule")]
                schedule_config,
                thinking_budget,
                show_thinking,
            )
            .await
        }
        #[cfg(feature = "credentials")]
        Command::Credential(sub) => run_credential_command(sub).await,
        #[cfg(feature = "postgres")]
        Command::Audit(sub) => run_audit_command(sub).await,
        #[cfg(feature = "postgres")]
        Command::Cost(sub) => run_cost_command(sub).await,
        #[cfg(feature = "postgres")]
        Command::Pricing(sub) => run_pricing_command(sub).await,
    }
}
