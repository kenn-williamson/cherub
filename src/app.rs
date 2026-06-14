//! App-assembly layer: builds the transport-agnostic core of an agent.
//!
//! This sits *above* `runtime` — it spawns processes, opens the Docker socket,
//! scans tool directories, and reads the DB. `AgentLoop` stays focused on the
//! turn loop; this module orchestrates the one-time, expensive construction of
//! the `ToolRegistry` and its process/socket/DB-backed backends.
//!
//! Both binaries (`main.rs` and the Telegram bot) produce an [`AgentConfig`]
//! from their own config source and call [`build_registry`], so the full tool
//! surface is wired in exactly one place — no per-transport drift.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

// `PathBuf` appears only in feature-gated tool-source fields; gate the import to
// match so a no-feature build stays warning-clean.
#[cfg(any(feature = "wasm", feature = "container", feature = "mcp"))]
use std::path::PathBuf;

use crate::enforcement::policy::Policy;
use crate::error::CherubError;
use crate::providers::Provider;
use crate::providers::config::ProvidersConfig;
use crate::runtime::AgentLoop;
use crate::runtime::approval::ApprovalGate;
use crate::runtime::output::OutputSink;
use crate::tools::ToolRegistry;

#[cfg(feature = "memory")]
use crate::storage::MemoryStore;
#[cfg(feature = "credentials")]
use crate::tools::credential_broker::CredentialBroker;

/// How to construct the LLM provider — the two mutually-exclusive paths each
/// transport offers. The flag path is **not** routed through
/// `instantiate_provider`: it carries an already-resolved API key, whereas the
/// named path resolves a key from an env-var *name* in the providers config.
pub enum ProviderSpec {
    /// Instantiate the `"default"` provider from a providers config (supports failover).
    Named(ProvidersConfig),
    /// Construct directly from already-resolved flags/env.
    Flags {
        /// `"anthropic"` or `"openai"`.
        provider_type: String,
        model: String,
        /// Required for Anthropic; optional for OpenAI-compatible (local) endpoints.
        api_key: Option<secrecy::SecretString>,
        base_url: Option<String>,
        thinking_budget: Option<u32>,
        max_tokens: u32,
    },
}

/// Construct a provider from its spec. Mirrors the original inline construction
/// in each transport; the flag path stays off `instantiate_provider` by design.
fn build_provider(spec: &ProviderSpec) -> Result<Box<dyn Provider>, CherubError> {
    match spec {
        ProviderSpec::Named(config) => {
            crate::providers::config::instantiate_named_provider(config, "default", &mut Vec::new())
        }
        ProviderSpec::Flags {
            provider_type,
            model,
            api_key,
            base_url,
            thinking_budget,
            max_tokens,
        } => match provider_type.as_str() {
            "openai" => {
                let mut p = crate::providers::openai::OpenAiProvider::new(
                    api_key.clone(),
                    model,
                    *max_tokens,
                )?;
                if let Some(url) = base_url {
                    p = p.with_base_url(url.clone());
                }
                Ok(Box::new(p))
            }
            "anthropic" => {
                let key = api_key.clone().ok_or_else(|| {
                    CherubError::Provider("anthropic provider requires an API key".to_owned())
                })?;
                let mut p =
                    crate::providers::anthropic::AnthropicProvider::new(key, model, *max_tokens)?;
                if let Some(budget) = thinking_budget {
                    p = p.with_thinking_budget(*budget);
                }
                Ok(Box::new(p))
            }
            other => Err(CherubError::Provider(format!(
                "unknown provider '{other}'. Available: anthropic, openai"
            ))),
        },
    }
}

/// Transport-agnostic description of *what* to assemble. Each binary produces
/// one from its own config source (CLI: clap flags + env; Telegram: env +
/// `SessionConfig`). It carries already-resolved shared handles plus the source
/// specs that [`SharedAgentServices::build`] turns into live backends/services.
pub struct AgentConfig {
    /// How to construct the LLM provider.
    pub provider: ProviderSpec,
    /// Policy — cloned into each sub-agent's inner loop and into each agent loop.
    pub policy: Policy,
    /// Resolved system prompt (the transport applies its own override-or-default).
    pub system_prompt: String,
    /// Workspace directory — drives the output-stashing hook.
    pub cwd: String,
    /// When true, the in-process bash tool is omitted (sandbox bash replaces it).
    pub skip_builtin_bash: bool,
    /// Process-level identity used for MCP `credential_env` wiring.
    pub user_id: String,
    /// Providers config — drives sub-agent tool instantiation when present.
    pub providers_config: Option<ProvidersConfig>,
    /// DB pool — backs the audit/cost/pricing stores and session persistence,
    /// all built once in `SharedAgentServices::build`.
    #[cfg(feature = "postgres")]
    pub db_pool: Option<crate::storage::Pool>,

    /// Shared memory store (already built by the transport from pool + embedder).
    #[cfg(feature = "memory")]
    pub memory_store: Option<Arc<dyn MemoryStore>>,
    /// Shared credential broker (already built by the transport from master key
    /// + pool). Enables the HTTP tool when present.
    #[cfg(feature = "credentials")]
    pub credential_broker: Option<Arc<CredentialBroker>>,
    /// Directory of WASM tools to compile and load.
    #[cfg(feature = "wasm")]
    pub wasm_dir: Option<PathBuf>,
    /// Directory of container (Docker) plugin tools to load.
    #[cfg(feature = "container")]
    pub container_tools_dir: Option<PathBuf>,
    /// Replace in-process bash with container-sandboxed bash + dev-environment.
    #[cfg(feature = "container")]
    pub enable_sandbox_bash: bool,
    /// Register the Playwright browser tool (runs in an isolated container).
    #[cfg(feature = "browser")]
    pub enable_browser: bool,
    /// MCP server config file — spawns servers and discovers their tools.
    #[cfg(feature = "mcp")]
    pub mcp_config: Option<PathBuf>,
}

/// Process-lifetime resources that must outlive the registry. Today this is the
/// IPC temp dirs (which have no `Drop`); kept on `SharedAgentServices` for the
/// whole process and a hook for future RAII cleanup.
#[derive(Default)]
pub struct ResourceGuards {
    #[cfg(feature = "container")]
    pub ipc_dirs: Vec<PathBuf>,
}

/// The credential store MCP `credential_env` resolves through: the broker's
/// store, which is the process-wide credentials handle. Returns `None` when no
/// broker is configured (no master key / DB) — the loader then errors only if a
/// server actually declares `credential_env`. Reaching the store through the
/// broker keeps MCP credential injection and the HTTP tool on one vault.
#[cfg(all(feature = "mcp", feature = "credentials"))]
fn mcp_credential_store(cfg: &AgentConfig) -> Option<&dyn crate::storage::CredentialStore> {
    cfg.credential_broker
        .as_ref()
        .map(|broker| broker.store.as_ref())
}

/// Assemble the full `ToolRegistry` from `cfg`. This is the single place that
/// spawns MCP servers, connects to Docker, compiles WASM modules, and wires
/// every tool. It runs once (inside `SharedAgentServices::build`) and the
/// resulting registry is shared read-only across all agent loops.
///
/// Wiring order mirrors the original inline assembly: base registry →
/// credentials/HTTP → WASM → container plugins → sandbox bash + dev-env →
/// browser → MCP → sub-agents.
pub async fn build_registry(
    cfg: &AgentConfig,
) -> Result<(ToolRegistry, ResourceGuards), CherubError> {
    // Base registry — attach the memory tool if a store is available.
    #[cfg(feature = "memory")]
    let registry = match &cfg.memory_store {
        Some(store) if cfg.skip_builtin_bash => {
            ToolRegistry::with_memory_no_bash(Arc::clone(store))
        }
        Some(store) => ToolRegistry::with_memory(Arc::clone(store)),
        None if cfg.skip_builtin_bash => ToolRegistry::new_without_bash(),
        None => ToolRegistry::new(),
    };
    #[cfg(not(feature = "memory"))]
    let registry = if cfg.skip_builtin_bash {
        ToolRegistry::new_without_bash()
    } else {
        ToolRegistry::new()
    };

    // Credential broker + HTTP tool.
    #[cfg(feature = "credentials")]
    let registry = match &cfg.credential_broker {
        Some(broker) => {
            tracing::info!("credential broker configured (HTTP tool enabled)");
            registry.with_credentials(Arc::clone(broker))
        }
        None => registry,
    };

    // IPC temp dirs that must live for the process lifetime.
    #[cfg(feature = "container")]
    let mut guards = ResourceGuards::default();
    #[cfg(not(feature = "container"))]
    let guards = ResourceGuards::default();

    // WASM tools (M8).
    #[cfg(feature = "wasm")]
    let registry = {
        use crate::tools::wasm::{WasmToolRuntime, load_from_dir};

        if let Some(dir) = &cfg.wasm_dir {
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
                    if result.tools.is_empty() {
                        registry
                    } else {
                        tracing::info!(
                            count = result.tools.len(),
                            dir = %dir.display(),
                            "WASM tools loaded"
                        );
                        registry.with_wasm(result.tools)
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

    // Container plugin tools (M9).
    #[cfg(feature = "container")]
    let registry = {
        use crate::tools::container::{BollardRuntime, load_from_dir};

        if let Some(dir) = &cfg.container_tools_dir {
            match BollardRuntime::new() {
                Ok(runtime) => {
                    let rt: Arc<dyn crate::tools::container::ContainerRuntime> = Arc::new(runtime);
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
                    if result.tools.is_empty() {
                        registry
                    } else {
                        tracing::info!(
                            count = result.tools.len(),
                            dir = %dir.display(),
                            "container tools loaded"
                        );
                        registry.with_container(result.tools)
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

    // Replace in-process bash with container-sandboxed bash + dev-environment.
    #[cfg(feature = "container")]
    let registry = if cfg.enable_sandbox_bash {
        use crate::tools::container::BollardRuntime;
        use crate::tools::dev_environment::DevEnvironmentTool;

        let runtime = BollardRuntime::new().map_err(|e| {
            CherubError::Container(format!(
                "sandbox bash requires Docker — failed to connect: {e}"
            ))
        })?;
        let rt: Arc<dyn crate::tools::container::ContainerRuntime> = Arc::new(runtime);
        if !rt.is_available().await {
            return Err(CherubError::Container(
                "sandbox bash requires Docker but the daemon is not reachable".to_owned(),
            ));
        }

        let workspace = std::env::current_dir().map_err(|e| {
            CherubError::Container(format!(
                "sandbox bash: failed to determine current directory: {e}"
            ))
        })?;
        let (bash_tool, ipc_dir) = crate::tools::container_bash::build(Arc::clone(&rt), workspace);
        guards.ipc_dirs.push(ipc_dir);

        let dev_env = DevEnvironmentTool::new(Arc::clone(&bash_tool));
        tracing::info!("sandbox bash enabled — bash commands run in isolated container");
        registry
            .with_container(vec![bash_tool])
            .with_dev_environment(dev_env)
    } else {
        registry
    };

    // Playwright browser tool.
    #[cfg(feature = "browser")]
    let registry = if cfg.enable_browser {
        use crate::tools::container::BollardRuntime;

        let runtime = BollardRuntime::new().map_err(|e| {
            CherubError::Container(format!("browser requires Docker — failed to connect: {e}"))
        })?;
        let rt: Arc<dyn crate::tools::container::ContainerRuntime> = Arc::new(runtime);
        if !rt.is_available().await {
            return Err(CherubError::Container(
                "browser requires Docker but the daemon is not reachable".to_owned(),
            ));
        }

        let (browser_tool, ipc_dir) = crate::tools::container_browser::build(Arc::clone(&rt));
        guards.ipc_dirs.push(ipc_dir);
        tracing::info!("browser tool enabled — Playwright runs in isolated container");
        registry.with_container(vec![browser_tool])
    } else {
        registry
    };

    // MCP servers (M11).
    #[cfg(feature = "mcp")]
    let registry = if let Some(config_path) = &cfg.mcp_config {
        let result = crate::tools::mcp::loader::load_from_config(
            config_path,
            #[cfg(feature = "credentials")]
            mcp_credential_store(cfg),
            #[cfg(feature = "credentials")]
            &cfg.user_id,
        )
        .await;
        for err in &result.errors {
            eprintln!("[warn] MCP: {err}");
        }
        if result.tools.is_empty() {
            registry
        } else {
            tracing::info!(
                count = result.tools.len(),
                config = %config_path.display(),
                "MCP tools loaded"
            );
            registry.with_mcp(result.tools)
        }
    } else {
        registry
    };

    // Sub-agent tools from providers config (M13d).
    let registry = if let Some(config) = &cfg.providers_config {
        use crate::providers::config::instantiate_named_provider;
        use crate::tools::sub_agent::SubAgentTool;

        let mut sub_agents: Vec<SubAgentTool> = Vec::new();
        for (agent_name, agent_def) in &config.agents {
            match instantiate_named_provider(config, &agent_def.provider, &mut Vec::new()) {
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
                        policy: cfg.policy.clone(),
                    });
                    tracing::info!(agent = %agent_name, "sub-agent tool registered");
                }
                Err(e) => {
                    tracing::warn!(
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

    Ok((registry, guards))
}

/// The transport-agnostic core, built **once** at startup and shared (by `Arc`)
/// across many agent loops. The Telegram bot builds one and hands
/// `Arc<SharedAgentServices>` to every per-chat session, so the expensive tool
/// backends (MCP processes, Docker runtime, WASM modules) are spawned a single
/// time — never per chat.
pub struct SharedAgentServices {
    /// The shared tool registry. Immutable after construction; every turn-path
    /// access is `&self`, so it is safe to share read-only across loops.
    pub registry: Arc<ToolRegistry>,
    /// The shared provider. Cloned into each loop; `Provider::send` is `&self`,
    /// so sharing also makes `FailoverProvider`'s circuit breaker global.
    pub provider: Arc<dyn Provider>,
    /// Policy, cloned into each loop.
    pub policy: Policy,
    /// Resolved system prompt, cloned into each loop.
    pub system_prompt: String,
    /// Workspace directory for the per-loop output-stashing hook.
    pub cwd: String,
    /// Shared memory store, exposed so each loop can enable proactive injection
    /// with the same handle that backs the memory tool.
    #[cfg(feature = "memory")]
    pub memory_store: Option<Arc<dyn MemoryStore>>,
    /// Audit log store (built once from the pool).
    #[cfg(feature = "postgres")]
    pub audit_store: Option<Arc<dyn crate::storage::AuditStore>>,
    /// Cost tracking store (built once from the pool).
    #[cfg(feature = "postgres")]
    pub cost_store: Option<Arc<dyn crate::storage::CostStore>>,
    /// Model pricing table, loaded once from the DB.
    #[cfg(feature = "postgres")]
    pub pricing_table: crate::providers::pricing::PricingTable,
    /// DB pool — used per session to build the session-persistence store.
    #[cfg(feature = "postgres")]
    pub db_pool: Option<crate::storage::Pool>,
    /// IPC temp dirs held alive for the whole process (no `Drop` today).
    #[allow(dead_code)]
    guards: ResourceGuards,
}

impl SharedAgentServices {
    /// Build the shared core from `cfg`. Runs [`build_registry`] exactly once.
    pub async fn build(cfg: AgentConfig) -> Result<Self, CherubError> {
        let provider: Arc<dyn Provider> = Arc::from(build_provider(&cfg.provider)?);
        #[cfg(feature = "memory")]
        let memory_store = cfg.memory_store.clone();

        // Build the DB-backed stores once (audit/cost are cheap handles; the
        // pricing table is a single query).
        #[cfg(feature = "postgres")]
        let (audit_store, cost_store, pricing_table) = if let Some(pool) = &cfg.db_pool {
            use crate::storage::PricingStore;
            let audit: Arc<dyn crate::storage::AuditStore> = Arc::new(
                crate::storage::pg_audit_store::PgAuditStore::new(pool.clone()),
            );
            let cost: Arc<dyn crate::storage::CostStore> = Arc::new(
                crate::storage::pg_cost_store::PgCostStore::new(pool.clone()),
            );
            let pricing_table: crate::providers::pricing::PricingTable =
                crate::storage::pg_pricing_store::PgPricingStore::new(pool.clone())
                    .list()
                    .await
                    .map(|entries| {
                        entries
                            .into_iter()
                            .map(|e| (e.model_pattern.clone(), e.to_model_pricing()))
                            .collect()
                    })
                    .unwrap_or_default();
            if !pricing_table.is_empty() {
                tracing::info!(entries = pricing_table.len(), "pricing table loaded");
            }
            (Some(audit), Some(cost), pricing_table)
        } else {
            (
                None,
                None,
                crate::providers::pricing::PricingTable::default(),
            )
        };

        let (registry, guards) = build_registry(&cfg).await?;
        Ok(Self {
            registry: Arc::new(registry),
            provider,
            policy: cfg.policy,
            system_prompt: cfg.system_prompt,
            cwd: cfg.cwd,
            #[cfg(feature = "memory")]
            memory_store,
            #[cfg(feature = "postgres")]
            audit_store,
            #[cfg(feature = "postgres")]
            cost_store,
            #[cfg(feature = "postgres")]
            pricing_table,
            #[cfg(feature = "postgres")]
            db_pool: cfg.db_pool,
            guards,
        })
    }
}

/// Which persistence slot a session uses — maps to the `(connector, id)` pair
/// the session store keys on.
pub enum PersistenceId {
    Cli,
    Telegram(i64),
}

impl PersistenceId {
    #[allow(dead_code)] // only read under `feature = "sessions"`
    fn parts(&self) -> (&'static str, String) {
        match self {
            PersistenceId::Cli => ("cli", "default".to_owned()),
            PersistenceId::Telegram(chat_id) => ("telegram", chat_id.to_string()),
        }
    }
}

/// Per-session assembler. Borrows the build-once [`SharedAgentServices`], layers
/// on the per-session state (persistence slot, cancel flag, task store, ...),
/// then `build(gate, sink)` produces a ready `AgentLoop` — running the full
/// `with_*` service chain in exactly one place, shared by every transport.
pub struct AgentBuilder<'a> {
    shared: &'a SharedAgentServices,
    user_id: String,
    /// Only read under `feature = "sessions"`.
    #[allow(dead_code)]
    persistence: Option<PersistenceId>,
    cancel_flag: Option<Arc<AtomicBool>>,
    #[cfg(feature = "postgres")]
    task_store: Option<Arc<dyn crate::storage::TaskStore>>,
    show_thinking: bool,
}

impl<'a> AgentBuilder<'a> {
    pub fn new(shared: &'a SharedAgentServices, user_id: impl Into<String>) -> Self {
        Self {
            shared,
            user_id: user_id.into(),
            persistence: None,
            cancel_flag: None,
            #[cfg(feature = "postgres")]
            task_store: None,
            show_thinking: false,
        }
    }

    /// Set the session-persistence slot (CLI or per-chat Telegram).
    pub fn persistence(mut self, id: PersistenceId) -> Self {
        self.persistence = Some(id);
        self
    }

    /// Shared cancellation flag (Telegram /stop).
    pub fn cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel_flag = Some(flag);
        self
    }

    /// Async-approval task store (Telegram autonomous turns). Note the gate also
    /// needs its own copy — wire that transport-side.
    #[cfg(feature = "postgres")]
    pub fn task_store(mut self, store: Arc<dyn crate::storage::TaskStore>) -> Self {
        self.task_store = Some(store);
        self
    }

    /// Emit thinking blocks to the output sink.
    pub fn show_thinking(mut self, show: bool) -> Self {
        self.show_thinking = show;
        self
    }

    /// Build the agent: `AgentLoop::new` + the full per-session service chain.
    /// Session persistence is best-effort — a failure logs and runs ephemeral.
    pub async fn build<A: ApprovalGate, O: OutputSink>(
        self,
        gate: A,
        output: O,
    ) -> AgentLoop<A, O> {
        let s = self.shared;
        let mut agent = AgentLoop::new(
            s.policy.clone(),
            Arc::clone(&s.provider),
            Arc::clone(&s.registry),
            s.system_prompt.clone(),
            gate,
            output,
            &self.user_id,
        );
        if self.show_thinking {
            agent.with_show_thinking(true);
        }
        if let Some(flag) = self.cancel_flag {
            agent.with_cancel_flag(flag);
        }
        agent.with_hook(Box::new(crate::runtime::hooks::OutputStashingHook::new(
            std::path::Path::new(&s.cwd),
        )));
        #[cfg(feature = "memory")]
        if let Some(store) = &s.memory_store {
            agent.with_memory_injection(Arc::clone(store));
        }
        #[cfg(feature = "postgres")]
        {
            if let Some(store) = &s.audit_store {
                agent.with_audit_log(Arc::clone(store));
            }
            if let Some(store) = &s.cost_store {
                agent.with_cost_tracking(Arc::clone(store));
            }
            agent.with_pricing_table(s.pricing_table.clone());
        }
        #[cfg(feature = "postgres")]
        if let Some(store) = self.task_store {
            agent.with_task_store(store);
        }
        #[cfg(feature = "sessions")]
        if let Some(pid) = &self.persistence
            && let Some(pool) = &s.db_pool
        {
            let store = Box::new(crate::storage::pg_session_store::PgSessionStore::new(
                pool.clone(),
            ));
            let (connector, id) = pid.parts();
            if let Err(e) = agent.with_persistence(store, connector, &id).await {
                tracing::warn!(error = %e, "session persistence unavailable, running ephemeral");
            }
        }
        agent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal config with no optional tools — exercises the base assembly path.
    fn base_config() -> AgentConfig {
        let policy = Policy::load(std::path::Path::new("config/default_policy.toml"))
            .expect("default policy loads");
        AgentConfig {
            // Keyless OpenAI spec — constructs without any network or API key.
            provider: ProviderSpec::Flags {
                provider_type: "openai".to_owned(),
                model: "test-model".to_owned(),
                api_key: None,
                base_url: None,
                thinking_budget: None,
                max_tokens: 1024,
            },
            policy,
            system_prompt: "test prompt".to_owned(),
            cwd: ".".to_owned(),
            skip_builtin_bash: false,
            user_id: "test".to_owned(),
            providers_config: None,
            #[cfg(feature = "postgres")]
            db_pool: None,
            #[cfg(feature = "memory")]
            memory_store: None,
            #[cfg(feature = "credentials")]
            credential_broker: None,
            #[cfg(feature = "wasm")]
            wasm_dir: None,
            #[cfg(feature = "container")]
            container_tools_dir: None,
            #[cfg(feature = "container")]
            enable_sandbox_bash: false,
            #[cfg(feature = "browser")]
            enable_browser: false,
            #[cfg(feature = "mcp")]
            mcp_config: None,
        }
    }

    #[tokio::test]
    async fn base_registry_has_builtin_tools() {
        let (registry, _guards) = build_registry(&base_config())
            .await
            .expect("build_registry");
        assert!(
            !registry.definitions().is_empty(),
            "base registry should expose the built-in tools"
        );
    }

    #[tokio::test]
    async fn skip_builtin_bash_drops_one_tool() {
        let with_bash = build_registry(&base_config())
            .await
            .expect("build_registry")
            .0
            .definitions()
            .len();
        let mut cfg = base_config();
        cfg.skip_builtin_bash = true;
        let without_bash = build_registry(&cfg)
            .await
            .expect("build_registry")
            .0
            .definitions()
            .len();
        assert_eq!(
            without_bash + 1,
            with_bash,
            "skip_builtin_bash should drop exactly the in-process bash tool"
        );
    }

    fn sorted_tool_names(registry: &ToolRegistry) -> Vec<String> {
        let mut names: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        names.sort();
        names
    }

    /// The tool set must depend only on the tool *config* — never on which
    /// transport assembled it. Two configs that differ solely in transport
    /// identity (user_id, system prompt, cwd) must yield an identical tool set.
    /// Guards against reintroducing transport-specific tool wiring (the bug that
    /// left the Telegram bot missing five tool categories).
    #[tokio::test]
    async fn tool_set_is_transport_independent() {
        let mut cli = base_config();
        cli.user_id = "cli".to_owned();
        cli.system_prompt = "CLI prompt".to_owned();
        cli.cwd = "/home/cli".to_owned();

        let mut telegram = base_config();
        telegram.user_id = "telegram-123".to_owned();
        telegram.system_prompt = "Telegram prompt".to_owned();
        telegram.cwd = "/tmp/telegram".to_owned();

        let cli_tools = sorted_tool_names(&build_registry(&cli).await.expect("build cli").0);
        let telegram_tools =
            sorted_tool_names(&build_registry(&telegram).await.expect("build telegram").0);

        assert!(!cli_tools.is_empty(), "expected built-in tools");
        assert_eq!(
            cli_tools, telegram_tools,
            "tool set must not depend on transport identity"
        );
    }

    /// MCP `credential_env` must resolve through the configured broker's store.
    /// Before this wiring, `build_registry` passed `None` and any server with a
    /// `credential_env` would fail with "requires credential store" even when a
    /// vault was configured. These tests pin the extraction: `None` with no
    /// broker, and the broker's own store when one is present.
    #[cfg(all(feature = "mcp", feature = "credentials"))]
    mod mcp_credential_wiring {
        use super::*;
        use crate::storage::{
            Credential, CredentialRef, CredentialStore, DecryptedCredential, NewCredential,
        };
        use crate::tools::credential_broker::CredentialBroker;

        /// Minimal store whose `get` records the lookup as a recognizable error,
        /// proving which store the wiring reached. Every other method is unused.
        struct SentinelStore;

        #[async_trait::async_trait]
        impl CredentialStore for SentinelStore {
            async fn get(&self, _user_id: &str, name: &str) -> Result<Credential, CherubError> {
                Err(CherubError::Credential(format!("sentinel:{name}")))
            }
            async fn store(&self, _c: NewCredential) -> Result<uuid::Uuid, CherubError> {
                unimplemented!("unused by the wiring test")
            }
            async fn get_ref(&self, _u: &str, _n: &str) -> Result<CredentialRef, CherubError> {
                unimplemented!("unused by the wiring test")
            }
            async fn list(&self, _u: &str) -> Result<Vec<CredentialRef>, CherubError> {
                unimplemented!("unused by the wiring test")
            }
            async fn delete(&self, _u: &str, _n: &str) -> Result<(), CherubError> {
                unimplemented!("unused by the wiring test")
            }
            async fn exists(&self, _u: &str, _n: &str) -> Result<bool, CherubError> {
                unimplemented!("unused by the wiring test")
            }
            async fn decrypt(&self, _c: &Credential) -> Result<DecryptedCredential, CherubError> {
                unimplemented!("unused by the wiring test")
            }
            async fn record_usage(&self, _u: &str, _n: &str) -> Result<(), CherubError> {
                unimplemented!("unused by the wiring test")
            }
            async fn is_expired(&self, _u: &str, _n: &str) -> Result<bool, CherubError> {
                unimplemented!("unused by the wiring test")
            }
        }

        #[test]
        fn no_broker_yields_no_store() {
            let cfg = base_config();
            assert!(
                mcp_credential_store(&cfg).is_none(),
                "without a broker, MCP credential_env has no store to resolve against"
            );
        }

        #[tokio::test]
        async fn broker_exposes_its_own_store() {
            let mut cfg = base_config();
            cfg.credential_broker = Some(Arc::new(CredentialBroker::new(Arc::new(SentinelStore))));

            let store = mcp_credential_store(&cfg).expect("broker present → store wired");

            // Reaching .get proves the store the loader receives is the broker's
            // own SentinelStore, not some unrelated handle.
            let err = store.get("u", "kairos-oauth").await.unwrap_err();
            assert!(
                err.to_string().contains("sentinel:kairos-oauth"),
                "loader must resolve credential_env through the broker's store, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn shared_services_builds_and_shares_registry() {
        let shared = SharedAgentServices::build(base_config())
            .await
            .expect("build shared services");
        assert!(
            !shared.registry.definitions().is_empty(),
            "shared registry should expose the built-in tools"
        );
        // Two cheap clones point at the same registry — one set of backends,
        // shared across loops (Telegram's per-chat sessions rely on this).
        let a = Arc::clone(&shared.registry);
        let b = Arc::clone(&shared.registry);
        assert!(Arc::ptr_eq(&a, &b), "clones must share one registry");
    }
}
