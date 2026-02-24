//! Factory for building a container-sandboxed bash tool.
//!
//! Constructs a `ContainerTool` configured as a drop-in replacement for the
//! in-process `BashTool`. Same tool name ("bash"), same JSON schema, but the
//! command runs inside an isolated Docker container with no access to host
//! env vars, secrets, or the policy file.
//!
//! The workspace directory is bind-mounted at `/workspace` so the agent can
//! still read/write project files.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use super::container::capabilities::ContainerCapabilities;
use super::container::runtime::ContainerRuntime;
use super::container::wrapper::{ContainerTool, ContainerToolMetadata};

/// Docker image name for the sandbox bash tool.
pub const IMAGE: &str = "cherub-sandbox-bash:latest";

/// Build a container-sandboxed bash tool.
///
/// Returns `(ContainerTool, PathBuf)`. The `PathBuf` is the IPC directory;
/// the caller must keep it alive for the lifetime of the tool (it is auto-cleaned
/// on process exit since it lives under the OS temp dir).
pub fn build(runtime: Arc<dyn ContainerRuntime>, workspace: PathBuf) -> (ContainerTool, PathBuf) {
    let ipc_dir =
        std::env::temp_dir().join(format!("cherub-sandbox-bash-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&ipc_dir).expect("failed to create IPC dir for sandbox bash");

    let metadata = ContainerToolMetadata {
        name: "bash".to_owned(),
        description: "Execute a bash command. The command is passed to `bash -c`.".to_owned(),
        schema: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                }
            },
            "required": ["command"]
        }),
        image: IMAGE.to_owned(),
    };

    // No host functions needed — the tool has direct workspace access via bind mount.
    let capabilities = ContainerCapabilities::default();

    let tool = ContainerTool::new(metadata, runtime, capabilities, ipc_dir.clone())
        .with_workspace(workspace);

    (tool, ipc_dir)
}
