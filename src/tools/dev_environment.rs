//! Dev environment tool: build custom sandbox images with language toolchains.
//!
//! The agent calls `dev_environment` with `action: "setup"` and a list of
//! languages. The tool generates a Dockerfile, builds it with `docker build`,
//! and reconfigures the sandbox bash container to use the new image.
//!
//! Images are tagged deterministically (sorted language list) so repeated
//! calls with the same languages reuse the cached image.

use std::sync::Arc;

use serde_json::json;

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;
use crate::providers::ToolDefinition;

use super::ToolResult;
use super::container::ContainerTool;

/// Languages that can be installed in the sandbox image.
pub const ALLOWED_LANGUAGES: &[&str] = &["rust", "node", "go"];

/// Base image tag prefix.
const IMAGE_PREFIX: &str = "cherub-sandbox-bash";

/// Timeout for `docker build` (10 minutes — language installs can be slow).
const BUILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// The IPC client script embedded from the sandbox-bash tool directory.
const IPC_CLIENT_PY: &str = include_str!("../../../tools/container/sandbox-bash/ipc_client.py");

/// Tool that builds custom sandbox Docker images with requested language toolchains.
pub struct DevEnvironmentTool {
    sandbox_bash: Arc<ContainerTool>,
}

impl DevEnvironmentTool {
    pub fn new(sandbox_bash: Arc<ContainerTool>) -> Self {
        Self { sandbox_bash }
    }

    pub async fn execute(
        &self,
        params: &serde_json::Value,
        _token: CapabilityToken,
    ) -> Result<ToolResult, CherubError> {
        let action = params.get("action").and_then(|v| v.as_str()).unwrap_or("");

        if action != "setup" {
            return Err(CherubError::ToolExecution(format!(
                "dev_environment: unknown action '{action}'"
            )));
        }

        let languages = parse_languages(params)?;
        validate_languages(&languages)?;

        let tag = image_tag(&languages);

        // Check if image already exists.
        if !image_exists(&tag).await {
            build_image(&tag, &languages).await?;
        }

        // Reconfigure sandbox bash to use the new image.
        self.sandbox_bash.reconfigure_image(&tag).await;

        let lang_list = if languages.is_empty() {
            "base (Python 3 only)".to_owned()
        } else {
            languages.join(", ")
        };

        Ok(ToolResult {
            output: format!(
                "Dev environment ready. Image: {tag}\nInstalled: {lang_list}\n\
                 Python 3 is always included. The sandbox bash tool will use this image."
            ),
        })
    }
}

/// Parse the `languages` array from params.
pub fn parse_languages(params: &serde_json::Value) -> Result<Vec<String>, CherubError> {
    match params.get("languages") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .map(|v| {
                v.as_str().map(|s| s.to_lowercase()).ok_or_else(|| {
                    CherubError::ToolExecution(
                        "dev_environment: language must be a string".to_owned(),
                    )
                })
            })
            .collect(),
        Some(serde_json::Value::Null) | None => Ok(Vec::new()),
        _ => Err(CherubError::ToolExecution(
            "dev_environment: 'languages' must be an array".to_owned(),
        )),
    }
}

/// Validate that all requested languages are in the allowed list.
pub fn validate_languages(languages: &[String]) -> Result<(), CherubError> {
    for lang in languages {
        if !ALLOWED_LANGUAGES.contains(&lang.as_str()) {
            return Err(CherubError::ToolExecution(format!(
                "dev_environment: unknown language '{lang}'. Allowed: {}",
                ALLOWED_LANGUAGES.join(", ")
            )));
        }
    }
    Ok(())
}

/// Generate a deterministic image tag from the sorted language list.
///
/// Empty list → `cherub-sandbox-bash:base`
/// `["node", "rust"]` and `["rust", "node"]` both → `cherub-sandbox-bash:node-rust`
pub fn image_tag(languages: &[String]) -> String {
    if languages.is_empty() {
        return format!("{IMAGE_PREFIX}:base");
    }
    let mut sorted: Vec<&str> = languages.iter().map(|s| s.as_str()).collect();
    sorted.sort_unstable();
    sorted.dedup();
    format!("{IMAGE_PREFIX}:{}", sorted.join("-"))
}

/// Check if a Docker image exists locally.
async fn image_exists(tag: &str) -> bool {
    tokio::process::Command::new("docker")
        .args(["image", "inspect", tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Generate the Dockerfile content for the given languages.
fn generate_dockerfile(languages: &[String]) -> String {
    let languages_csv = languages.join(",");

    let mut dockerfile = String::from(
        r#"# Auto-generated by cherub dev_environment tool.
FROM python:3.13-slim

# Base tools (always installed).
RUN apt-get update && apt-get install -y --no-install-recommends \
    bash coreutils grep findutils sed gawk curl jq git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
"#,
    );

    // Conditional language layers — same structure as the manual Dockerfile.
    if languages.contains(&"rust".to_owned()) {
        dockerfile.push_str(
            r#"
# Rust (system-wide install).
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --default-toolchain stable \
    && chmod -R a+r /usr/local/rustup /usr/local/cargo
ENV PATH="/usr/local/cargo/bin:${PATH}"
"#,
        );
    }

    if languages.contains(&"node".to_owned()) {
        dockerfile.push_str(
            r#"
# Node.js 22 LTS.
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*
"#,
        );
    }

    if languages.contains(&"go".to_owned()) {
        dockerfile.push_str(
            r#"
# Go.
RUN curl -fsSL https://go.dev/dl/go1.24.0.linux-amd64.tar.gz \
    | tar -C /usr/local -xz
ENV PATH="/usr/local/go/bin:${PATH}"
"#,
        );
    }

    // Common tail: non-root user, IPC client, directories.
    dockerfile.push_str(&format!(
        r#"
# Non-root user with writable home (for cargo/npm/pip caches).
RUN groupadd -g 1000 tool && useradd -u 1000 -g tool -m tool

COPY ipc_client.py /app/ipc_client.py
RUN mkdir -p /ipc /workspace && chown tool:tool /ipc /workspace

USER tool
WORKDIR /workspace
CMD ["python3", "/app/ipc_client.py"]

LABEL cherub.languages="{languages_csv}"
"#
    ));

    dockerfile
}

/// Build a Docker image from the generated Dockerfile + embedded ipc_client.py.
async fn build_image(tag: &str, languages: &[String]) -> Result<(), CherubError> {
    let tmp_dir = tempfile::tempdir().map_err(|e| {
        CherubError::ToolExecution(format!("dev_environment: failed to create temp dir: {e}"))
    })?;

    // Write Dockerfile.
    let dockerfile_path = tmp_dir.path().join("Dockerfile");
    std::fs::write(&dockerfile_path, generate_dockerfile(languages)).map_err(|e| {
        CherubError::ToolExecution(format!("dev_environment: failed to write Dockerfile: {e}"))
    })?;

    // Write embedded ipc_client.py.
    let ipc_path = tmp_dir.path().join("ipc_client.py");
    std::fs::write(&ipc_path, IPC_CLIENT_PY).map_err(|e| {
        CherubError::ToolExecution(format!(
            "dev_environment: failed to write ipc_client.py: {e}"
        ))
    })?;

    tracing::info!(tag = %tag, languages = ?languages, "building sandbox image");

    let output = tokio::time::timeout(
        BUILD_TIMEOUT,
        tokio::process::Command::new("docker")
            .args(["build", "-t", tag, "."])
            .current_dir(tmp_dir.path())
            .output(),
    )
    .await
    .map_err(|_| {
        CherubError::ToolExecution(format!(
            "dev_environment: docker build timed out after {}s",
            BUILD_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| {
        CherubError::ToolExecution(format!(
            "dev_environment: docker build failed to start: {e}"
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CherubError::ToolExecution(format!(
            "dev_environment: docker build failed:\n{stderr}"
        )));
    }

    tracing::info!(tag = %tag, "sandbox image built successfully");
    Ok(())
}

/// Tool definition for the dev_environment tool (used by ToolImpl::definition()).
pub fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "dev_environment".to_owned(),
        description: "Set up the development environment with specific language toolchains. \
            Builds a sandbox Docker image with the requested languages installed. \
            Python 3 is always included."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["setup"],
                    "description": "Action to perform"
                },
                "languages": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["rust", "node", "go"]
                    },
                    "description": "Language toolchains to install. Python 3 is always included."
                }
            },
            "required": ["action", "languages"]
        }),
    }
}
