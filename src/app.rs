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

// `PathBuf`/`Arc` are used only inside feature-gated tool wiring; gate the
// imports to match so a no-feature build stays warning-clean.
#[cfg(any(feature = "wasm", feature = "container", feature = "mcp"))]
use std::path::PathBuf;
#[cfg(any(
    feature = "memory",
    feature = "credentials",
    feature = "wasm",
    feature = "container"
))]
use std::sync::Arc;

use crate::enforcement::policy::Policy;
use crate::error::CherubError;
use crate::providers::config::ProvidersConfig;
use crate::tools::ToolRegistry;

#[cfg(feature = "memory")]
use crate::storage::MemoryStore;
#[cfg(feature = "credentials")]
use crate::tools::credential_broker::CredentialBroker;

/// Transport-agnostic description of *what* to assemble. Each binary produces
/// one from its own config source (CLI: clap flags + env; Telegram: env +
/// `SessionConfig`). It carries already-resolved shared handles (memory store,
/// credential broker) plus the source specs (dirs, configs, flags) that
/// [`build_registry`] turns into live backends. It performs no I/O itself.
pub struct AgentConfig {
    /// Policy — cloned into each sub-agent's bounded inner loop.
    pub policy: Policy,
    /// When true, the in-process bash tool is omitted (sandbox bash replaces it).
    pub skip_builtin_bash: bool,
    /// Process-level identity used for MCP `credential_env` wiring.
    pub user_id: String,
    /// Providers config — drives sub-agent tool instantiation when present.
    pub providers_config: Option<ProvidersConfig>,

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
            None, // TODO: wire credential store for MCP credential_env
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal config with no optional tools — exercises the base assembly path.
    fn base_config() -> AgentConfig {
        let policy = Policy::load(std::path::Path::new("config/default_policy.toml"))
            .expect("default policy loads");
        AgentConfig {
            policy,
            skip_builtin_bash: false,
            user_id: "test".to_owned(),
            providers_config: None,
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
}
