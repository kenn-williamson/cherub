//! Container runtime abstraction and bollard-backed implementation.
//!
//! `ContainerRuntime` trait allows mocking in tests and future Podman-specific
//! implementations. `BollardRuntime` is the production implementation backed
//! by the bollard Docker client (compatible with Podman via `DOCKER_HOST`).

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountTypeEnum};
use bollard::query_parameters::{
    CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};

use crate::error::CherubError;

// ─── ContainerConfig ──────────────────────────────────────────────────────────

/// Configuration for spawning a container tool.
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    /// Docker image name (e.g., `"cherub-tool-text-analysis:latest"`).
    pub image: String,
    /// Container name prefix (made unique at spawn time).
    pub name: String,
    /// Host directory bind-mounted into the container at `/ipc/`.
    /// The runtime places the UDS socket file here; the container connects to it.
    pub ipc_dir: PathBuf,
    /// Memory limit in bytes (default: 512 MiB).
    pub memory_bytes: u64,
    /// CPU shares (relative weight; default: 512 — half of 1024 baseline).
    pub cpu_shares: u64,
    /// Optional host directory bind-mounted at `/workspace` inside the container.
    /// Used by the sandbox bash tool to give the agent read/write access to project files.
    pub workspace_dir: Option<PathBuf>,
}

impl ContainerConfig {
    /// Default memory limit: 512 MiB.
    pub const DEFAULT_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
    /// Default CPU shares.
    pub const DEFAULT_CPU_SHARES: u64 = 512;

    pub fn new(image: impl Into<String>, name: impl Into<String>, ipc_dir: PathBuf) -> Self {
        Self {
            image: image.into(),
            name: name.into(),
            ipc_dir,
            memory_bytes: Self::DEFAULT_MEMORY_BYTES,
            cpu_shares: Self::DEFAULT_CPU_SHARES,
            workspace_dir: None,
        }
    }

    /// Set a host directory to bind-mount at `/workspace` inside the container.
    pub fn with_workspace(mut self, dir: PathBuf) -> Self {
        self.workspace_dir = Some(dir);
        self
    }
}

// ─── ContainerRuntime trait ───────────────────────────────────────────────────

/// Manages Docker/Podman container lifecycle.
///
/// Genuine extension boundary: `BollardRuntime` in production, mock in tests,
/// potential Podman-specific impl in future. Hence `async_trait` + `dyn`.
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Returns `true` if the Docker/Podman daemon is reachable.
    async fn is_available(&self) -> bool;

    /// Spawn a new container from `config`.
    ///
    /// Returns the container ID on success.
    async fn spawn(&self, config: &ContainerConfig) -> Result<String, CherubError>;

    /// Stop a running container gracefully (SIGTERM + timeout).
    async fn stop(&self, container_id: &str) -> Result<(), CherubError>;

    /// Remove a stopped container.
    async fn remove(&self, container_id: &str) -> Result<(), CherubError>;

    /// Returns `true` if the container with `container_id` is currently running.
    async fn is_running(&self, container_id: &str) -> Result<bool, CherubError>;
}

// ─── BollardRuntime ───────────────────────────────────────────────────────────

/// Production container runtime backed by the bollard Docker client.
///
/// Works with Podman by setting `DOCKER_HOST` to the Podman socket.
pub struct BollardRuntime {
    docker: Docker,
}

impl BollardRuntime {
    /// Connect to the local Docker/Podman daemon.
    pub fn new() -> Result<Self, CherubError> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| CherubError::Container(format!("failed to connect to Docker: {e}")))?;
        Ok(Self { docker })
    }
}

#[async_trait]
impl ContainerRuntime for BollardRuntime {
    async fn is_available(&self) -> bool {
        self.docker.ping().await.is_ok()
    }

    async fn spawn(&self, config: &ContainerConfig) -> Result<String, CherubError> {
        let ipc_dir = config
            .ipc_dir
            .to_str()
            .ok_or_else(|| CherubError::Container("IPC dir path is not valid UTF-8".to_owned()))?
            .to_owned();

        // Unique container name to avoid conflicts on restart.
        let container_name = format!("{}-{}", config.name, &uuid::Uuid::now_v7().to_string()[..8]);

        // Build mount list: always includes IPC, optionally includes workspace.
        let mut mounts = vec![Mount {
            target: Some("/ipc".to_owned()),
            source: Some(ipc_dir),
            typ: Some(MountTypeEnum::BIND),
            read_only: Some(false), // socket connect only needs path traversal
            ..Default::default()
        }];

        let working_dir = if let Some(ref workspace) = config.workspace_dir {
            let ws_str = workspace
                .to_str()
                .ok_or_else(|| {
                    CherubError::Container("workspace dir path is not valid UTF-8".to_owned())
                })?
                .to_owned();
            mounts.push(Mount {
                target: Some("/workspace".to_owned()),
                source: Some(ws_str),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(false),
                ..Default::default()
            });
            Some("/workspace".to_owned())
        } else {
            None
        };

        let host_config = HostConfig {
            // No outbound network — tools call host functions for HTTP.
            network_mode: Some("none".to_owned()),
            // Drop all Linux capabilities.
            cap_drop: Some(vec!["ALL".to_owned()]),
            // Prevent privilege escalation via setuid/setgid.
            security_opt: Some(vec!["no-new-privileges:true".to_owned()]),
            // Root filesystem is read-only; /tmp is writable via tmpfs (see mounts below).
            readonly_rootfs: Some(true),
            // cgroup memory limit.
            memory: Some(config.memory_bytes as i64),
            // cgroup CPU weight.
            cpu_shares: Some(config.cpu_shares as i64),
            // Bind-mount IPC and optionally workspace.
            mounts: Some(mounts),
            // tmpfs at /tmp: tool scratch space, inherently wiped between calls.
            tmpfs: Some(HashMap::from([(
                "/tmp".to_owned(),
                "rw,size=65536k,noexec,nosuid".to_owned(),
            )])),
            ..Default::default()
        };

        let container_config = ContainerCreateBody {
            image: Some(config.image.clone()),
            user: Some("1000:1000".to_owned()),
            env: Some(vec!["CHERUB_IPC_SOCKET=/ipc/tool.sock".to_owned()]),
            working_dir,
            host_config: Some(host_config),
            ..Default::default()
        };

        let response = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name),
                    platform: String::new(),
                }),
                container_config,
            )
            .await
            .map_err(|e| CherubError::Container(format!("create_container failed: {e}")))?;

        let container_id = response.id;

        self.docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await
            .map_err(|e| {
                CherubError::Container(format!("start_container '{container_id}' failed: {e}"))
            })?;

        tracing::info!(
            container_id = %container_id,
            image = %config.image,
            "container tool started"
        );

        Ok(container_id)
    }

    async fn stop(&self, container_id: &str) -> Result<(), CherubError> {
        self.docker
            .stop_container(
                container_id,
                Some(StopContainerOptions {
                    t: Some(5), // 5 second graceful timeout
                    signal: None,
                }),
            )
            .await
            .map_err(|e| {
                CherubError::Container(format!("stop_container '{container_id}' failed: {e}"))
            })?;
        tracing::debug!(container_id, "container tool stopped");
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), CherubError> {
        self.docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: false,
                    link: false,
                }),
            )
            .await
            .map_err(|e| {
                CherubError::Container(format!("remove_container '{container_id}' failed: {e}"))
            })?;
        tracing::debug!(container_id, "container tool removed");
        Ok(())
    }

    async fn is_running(&self, container_id: &str) -> Result<bool, CherubError> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(|e| {
                CherubError::Container(format!("inspect_container '{container_id}' failed: {e}"))
            })?;
        let running = info.state.and_then(|s| s.running).unwrap_or(false);
        Ok(running)
    }
}
