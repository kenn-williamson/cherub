//! Container tool wrapper: lifecycle management and IPC execution.
//!
//! `ContainerTool` represents a single sandboxed tool backed by a long-lived
//! Docker container. The container is started on first use and kept alive
//! across calls; crashes are detected and the container is respawned
//! transparently.
//!
//! # Concurrency
//!
//! `Mutex<ContainerState>` serializes IPC access: each container handles one
//! request at a time. This is a deliberate architectural constraint — the IPC
//! stream is inherently sequential. Documented per CLAUDE.md anti-pattern rules
//! as "truly shared concurrent state that cannot be restructured."

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;
use crate::tools::ToolContext;
use crate::tools::ToolResult;
use crate::tools::container::capabilities::ContainerCapabilities;
use crate::tools::container::host::ContainerHostState;
use crate::tools::container::ipc::{IpcTransport, RuntimeMessage, ToolMessage};
use crate::tools::container::runtime::{ContainerConfig, ContainerRuntime};

/// Timeout for container startup and IPC registration (connect + Registration message).
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for a single tool execution (send Execute + receive Result).
const EXECUTE_TIMEOUT: Duration = Duration::from_secs(120);

// ─── Metadata ─────────────────────────────────────────────────────────────────

/// Static metadata for a container tool (name, description, JSON schema).
///
/// Populated from `tool.toml` at load time.
#[derive(Debug, Clone)]
pub struct ContainerToolMetadata {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
    pub image: String,
}

// ─── ContainerState ───────────────────────────────────────────────────────────

/// Mutable runtime state guarded by a single `Mutex`.
///
/// Both `container_id` and `transport` live here so there is only one
/// mutex to acquire — no lock-ordering hazards.
struct ContainerState {
    container_id: Option<String>,
    transport: Option<IpcTransport>,
    /// Overrides the image from `ContainerToolMetadata`. Set by `DevEnvironmentTool`
    /// when the agent requests a custom sandbox image with specific language toolchains.
    image_override: Option<String>,
}

// ─── ContainerTool ────────────────────────────────────────────────────────────

/// A tool backed by a long-lived Docker/Podman container.
pub struct ContainerTool {
    pub(crate) metadata: ContainerToolMetadata,
    runtime: Arc<dyn ContainerRuntime>,
    capabilities: ContainerCapabilities,
    /// Host filesystem directory bind-mounted at `/ipc/` inside the container.
    ipc_dir: PathBuf,
    /// Optional host directory bind-mounted at `/workspace` inside the container.
    workspace_dir: Option<PathBuf>,
    /// Docker network mode override (default: `"none"`).
    network_mode: Option<String>,
    /// Whether the root filesystem should be writable (default: read-only).
    writable_rootfs: bool,
    /// Whether tmpfs mounts should be disabled (default: enabled).
    no_tmpfs: bool,
    /// Memory limit override in bytes (default: `ContainerConfig::DEFAULT_MEMORY_BYTES`).
    memory_bytes: Option<u64>,
    /// Serializes all IPC access and container lifecycle mutations.
    state: Mutex<ContainerState>,
    /// Monotonic call-ID counter for correlating Execute/Result pairs.
    next_id: AtomicU64,
    /// Optional credential broker for host-side injection (requires `credentials` feature).
    #[cfg(feature = "credentials")]
    broker: Option<Arc<crate::tools::credential_broker::CredentialBroker>>,
}

impl ContainerTool {
    pub fn new(
        metadata: ContainerToolMetadata,
        runtime: Arc<dyn ContainerRuntime>,
        capabilities: ContainerCapabilities,
        ipc_dir: PathBuf,
    ) -> Self {
        Self {
            metadata,
            runtime,
            capabilities,
            ipc_dir,
            workspace_dir: None,
            network_mode: None,
            writable_rootfs: false,
            no_tmpfs: false,
            memory_bytes: None,
            state: Mutex::new(ContainerState {
                container_id: None,
                transport: None,
                image_override: None,
            }),
            next_id: AtomicU64::new(1),
            #[cfg(feature = "credentials")]
            broker: None,
        }
    }

    /// Set a host directory to bind-mount at `/workspace` inside the container.
    pub fn with_workspace(mut self, dir: PathBuf) -> Self {
        self.workspace_dir = Some(dir);
        self
    }

    /// Set the Docker network mode (e.g., `"bridge"` for outbound access).
    pub fn with_network(mut self, mode: &str) -> Self {
        self.network_mode = Some(mode.to_owned());
        self
    }

    /// Allow writes to the root filesystem (disables `--read-only`).
    pub fn with_writable_rootfs(mut self) -> Self {
        self.writable_rootfs = true;
        self
    }

    /// Disable tmpfs mounts (use the container's default `/tmp`).
    pub fn without_tmpfs(mut self) -> Self {
        self.no_tmpfs = true;
        self
    }

    /// Override the container memory limit.
    pub fn with_memory(mut self, bytes: u64) -> Self {
        self.memory_bytes = Some(bytes);
        self
    }

    /// Attach a credential broker for credential injection in host HTTP calls.
    #[cfg(feature = "credentials")]
    pub fn with_broker(
        mut self,
        broker: Arc<crate::tools::credential_broker::CredentialBroker>,
    ) -> Self {
        self.broker = Some(broker);
        self
    }

    // ─── Execute ──────────────────────────────────────────────────────────────

    /// Execute the container tool.
    ///
    /// Requires a `CapabilityToken` (consumed — proves enforcement ran). Starts
    /// or restarts the container if needed, then performs a synchronous IPC
    /// call within `EXECUTE_TIMEOUT`.
    pub async fn execute(
        &self,
        params: &serde_json::Value,
        _token: CapabilityToken, // consumed — proves enforcement ran
        ctx: &ToolContext,
    ) -> Result<ToolResult, CherubError> {
        let span = tracing::info_span!(
            "container_execute",
            tool = %self.metadata.name,
            user_id = %ctx.user_id
        );
        let _guard = span.enter();

        let mut state = self.state.lock().await;
        self.ensure_running(&mut state).await?;

        let call_id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let context = serde_json::json!({
            "user_id": ctx.user_id,
            "session_id": ctx.session_id.to_string(),
            "turn_number": ctx.turn_number,
        });

        let name = self.metadata.name.clone();
        let caps = self.capabilities.clone();
        let user_id = ctx.user_id.clone();
        #[cfg(feature = "credentials")]
        let broker = self.broker.clone();

        let transport = state
            .transport
            .as_mut()
            .expect("transport set by ensure_running");

        let result = timeout(EXECUTE_TIMEOUT, async {
            // Send the Execute message.
            transport
                .send(&RuntimeMessage::Execute {
                    id: call_id,
                    params: params.clone(),
                    context: Some(context),
                })
                .await
                .map_err(|e| CherubError::Container(format!("IPC send failed: {e}")))?;

            let mut host_state = ContainerHostState::new(caps, user_id);

            // Receive messages until we get the Result for this call.
            loop {
                let msg = transport
                    .recv()
                    .await
                    .map_err(|e| CherubError::Container(format!("IPC recv failed: {e}")))?;

                match msg {
                    ToolMessage::Log { level, message } => {
                        host_state.handle_log(&name, &level, &message);
                    }
                    ToolMessage::Result { id, output, error } => {
                        if id != call_id {
                            tracing::warn!(
                                expected = call_id,
                                got = id,
                                "container tool sent Result for wrong call ID — skipping"
                            );
                            continue;
                        }
                        host_state.emit_log_summary(&name);
                        if let Some(err) = error {
                            return Err(CherubError::ToolExecution(format!(
                                "container tool '{}' returned error: {err}",
                                name
                            )));
                        }
                        return Ok(ToolResult {
                            output: output.unwrap_or_default(),
                        });
                    }
                    ToolMessage::HostCall { id, function, args } => {
                        let result = host_state
                            .dispatch(
                                &name,
                                &function,
                                &args,
                                #[cfg(feature = "credentials")]
                                broker.as_ref(),
                            )
                            .await;
                        transport
                            .send(&RuntimeMessage::HostResponse { id, result })
                            .await
                            .map_err(|e| {
                                CherubError::Container(format!(
                                    "IPC host_response send failed: {e}"
                                ))
                            })?;
                    }
                    ToolMessage::Registration { .. } => {
                        tracing::warn!(
                            tool = %name,
                            "unexpected Registration message during execution — ignoring"
                        );
                    }
                }
            }
        })
        .await;

        match result {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e)) => {
                // IPC error — mark container for respawn on next call.
                state.transport = None;
                state.container_id = None;
                Err(e)
            }
            Err(_elapsed) => {
                // Timeout — mark container for respawn.
                state.transport = None;
                state.container_id = None;
                Err(CherubError::Container(format!(
                    "container tool '{}' timed out after {}s",
                    self.metadata.name,
                    EXECUTE_TIMEOUT.as_secs()
                )))
            }
        }
    }

    // ─── Lifecycle ────────────────────────────────────────────────────────────

    /// Ensure the container is running and the IPC transport is connected.
    ///
    /// Respawns if the container has crashed or was never started.
    async fn ensure_running(&self, state: &mut ContainerState) -> Result<(), CherubError> {
        // Fast path: already running with a live transport.
        if state.transport.is_some() {
            if let Some(ref cid) = state.container_id {
                match self.runtime.is_running(cid).await {
                    Ok(true) => return Ok(()),
                    Ok(false) => {
                        tracing::warn!(
                            tool = %self.metadata.name,
                            container_id = %cid,
                            "container crashed — respawning"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            tool = %self.metadata.name,
                            error = %e,
                            "failed to inspect container — respawning"
                        );
                    }
                }
            }
        }

        // Clean up any stale container.
        if let Some(cid) = state.container_id.take() {
            let _ = self.runtime.stop(&cid).await;
            let _ = self.runtime.remove(&cid).await;
        }
        state.transport = None;

        // Create IPC directory if needed.
        std::fs::create_dir_all(&self.ipc_dir).map_err(|e| {
            CherubError::Container(format!(
                "failed to create IPC dir '{}': {e}",
                self.ipc_dir.display()
            ))
        })?;

        let socket_path = self.ipc_dir.join("tool.sock");

        // Remove any stale socket file.
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).map_err(|e| {
                CherubError::Container(format!(
                    "failed to remove stale socket '{}': {e}",
                    socket_path.display()
                ))
            })?;
        }

        // Create the Unix domain socket listener.
        let listener = UnixListener::bind(&socket_path).map_err(|e| {
            CherubError::Container(format!(
                "failed to bind UDS '{}': {e}",
                socket_path.display()
            ))
        })?;

        // Spawn the container (bind-mounts ipc_dir → /ipc/ in the container).
        let image = state
            .image_override
            .as_deref()
            .unwrap_or(&self.metadata.image);
        let mut config = ContainerConfig::new(image, &self.metadata.name, self.ipc_dir.clone());
        if let Some(ref ws) = self.workspace_dir {
            config = config.with_workspace(ws.clone());
        }
        if let Some(ref mode) = self.network_mode {
            config = config.with_network(mode);
        }
        if self.writable_rootfs {
            config = config.with_writable_rootfs();
        }
        if self.no_tmpfs {
            config = config.without_tmpfs();
        }
        if let Some(bytes) = self.memory_bytes {
            config.memory_bytes = bytes;
        }
        let container_id = self.runtime.spawn(&config).await?;
        state.container_id = Some(container_id.clone());

        // Wait for the container to connect and send Registration.
        let transport = timeout(STARTUP_TIMEOUT, async {
            let (stream, _) = listener.accept().await.map_err(|e| {
                CherubError::Container(format!("failed to accept IPC connection: {e}"))
            })?;
            let mut transport = IpcTransport::new(stream);
            let msg = transport.recv().await.map_err(|e| {
                CherubError::Container(format!("failed to receive Registration: {e}"))
            })?;
            match msg {
                ToolMessage::Registration {
                    name,
                    description,
                    schema,
                } => {
                    tracing::info!(
                        container_tool = %name,
                        container_id = %container_id,
                        "container tool registered"
                    );
                    // Validate name matches expected.
                    if name != self.metadata.name {
                        tracing::warn!(
                            expected = %self.metadata.name,
                            got = %name,
                            "container tool registered with unexpected name"
                        );
                    }
                    let _ = (description, schema); // already stored from tool.toml
                }
                other => {
                    tracing::warn!(
                        tool = %self.metadata.name,
                        "expected Registration, got unexpected message: {other:?}"
                    );
                }
            }
            Ok::<_, CherubError>(transport)
        })
        .await
        .map_err(|_elapsed| {
            CherubError::Container(format!(
                "container tool '{}' failed to connect within {}s",
                self.metadata.name,
                STARTUP_TIMEOUT.as_secs()
            ))
        })??;

        state.transport = Some(transport);
        tracing::info!(
            tool = %self.metadata.name,
            "container tool ready"
        );
        Ok(())
    }

    /// Reconfigure the container to use a different image.
    ///
    /// Gracefully shuts down any running container and sets the image override.
    /// The next `execute()` call will start a fresh container from the new image.
    pub(crate) async fn reconfigure_image(&self, image: &str) {
        let mut state = self.state.lock().await;
        // Gracefully shut down running container.
        if let Some(ref mut transport) = state.transport {
            let _ = transport.send(&RuntimeMessage::Shutdown).await;
        }
        state.transport = None;
        if let Some(cid) = state.container_id.take() {
            let _ = self.runtime.stop(&cid).await;
            let _ = self.runtime.remove(&cid).await;
        }
        state.image_override = Some(image.to_owned());
    }

    /// Send `Shutdown` and remove the container. Called explicitly when unloading.
    pub async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        if let Some(ref mut transport) = state.transport {
            let _ = transport.send(&RuntimeMessage::Shutdown).await;
        }
        state.transport = None;
        if let Some(cid) = state.container_id.take() {
            let _ = self.runtime.stop(&cid).await;
            let _ = self.runtime.remove(&cid).await;
        }
    }
}
