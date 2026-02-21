//! Container tool loader: scan config directories and build `ContainerTool` instances.
//!
//! # Directory layout
//!
//! Each container tool lives in its own subdirectory:
//! ```text
//! tools/container/
//! ├── text-analysis/
//! │   ├── tool.toml           (name, description, image, JSON schema)
//! │   └── capabilities.toml   (capability declarations)
//! └── another-tool/
//!     ├── tool.toml
//!     └── capabilities.toml
//! ```
//!
//! # Security
//!
//! - `tool.toml` parsed with `#[serde(deny_unknown_fields)]`.
//! - `capabilities.toml` parsed with the same guard + 64 KiB size limit.
//! - IPC socket directory created under `/tmp/cherub-ipc/`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::error::CherubError;
use crate::tools::container::capabilities::ContainerCapabilities;
use crate::tools::container::runtime::ContainerRuntime;
use crate::tools::container::wrapper::{ContainerTool, ContainerToolMetadata};

// ─── tool.toml format ─────────────────────────────────────────────────────────

/// Deserialized `tool.toml` for a container tool.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolToml {
    name: String,
    description: String,
    image: String,
    /// JSON schema for the tool's input parameters.
    /// Written as a TOML table in tool.toml; converted to JSON on load.
    schema: toml::Value,
}

impl ToolToml {
    fn from_toml(content: &str) -> Result<Self, String> {
        const MAX_BYTES: usize = 64 * 1024;
        if content.len() > MAX_BYTES {
            return Err(format!(
                "tool.toml too large: {} bytes (max {MAX_BYTES})",
                content.len()
            ));
        }
        toml::from_str(content).map_err(|e| format!("invalid tool.toml: {e}"))
    }
}

// ─── TOML → JSON conversion ───────────────────────────────────────────────────

/// Convert a `toml::Value` to `serde_json::Value`.
///
/// TOML and JSON have compatible type systems for the schema use case:
/// booleans, integers, floats, strings, arrays, and tables all map directly.
/// TOML datetime values are converted to their string representation.
fn toml_to_json(val: toml::Value) -> serde_json::Value {
    match val {
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Integer(i) => serde_json::Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(t) => {
            let map: serde_json::Map<String, serde_json::Value> =
                t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect();
            serde_json::Value::Object(map)
        }
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
    }
}

// ─── Loader ───────────────────────────────────────────────────────────────────

/// Result of loading a directory of container tools.
pub struct ContainerLoadResult {
    /// Successfully loaded tools.
    pub tools: Vec<ContainerTool>,
    /// Per-tool error messages for tools that failed to load.
    pub errors: Vec<String>,
}

/// Load all container tools from a config directory.
///
/// Scans `dir` for subdirectories. Each subdirectory must contain `tool.toml`
/// and `capabilities.toml`. Tools that fail to load are logged as warnings
/// and included in `errors`.
pub async fn load_from_dir(
    dir: &Path,
    runtime: Arc<dyn ContainerRuntime>,
    #[cfg(feature = "credentials")] broker: Option<
        Arc<crate::tools::credential_broker::CredentialBroker>,
    >,
) -> ContainerLoadResult {
    let mut tools = Vec::new();
    let mut errors = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            return ContainerLoadResult {
                tools: vec![],
                errors: vec![format!(
                    "failed to read container tools directory '{}': {e}",
                    dir.display()
                )],
            };
        }
    };

    for entry in entries.flatten() {
        let tool_dir = entry.path();
        if !tool_dir.is_dir() {
            continue;
        }

        let name = match tool_dir.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => {
                errors.push(format!(
                    "skipping '{}': directory name is not valid UTF-8",
                    tool_dir.display()
                ));
                continue;
            }
        };

        match load_one(
            &tool_dir,
            &name,
            Arc::clone(&runtime),
            #[cfg(feature = "credentials")]
            broker.clone(),
        )
        .await
        {
            Ok(tool) => {
                tracing::info!(
                    tool = %name,
                    image = %tool.metadata.image,
                    "loaded container tool"
                );
                tools.push(tool);
            }
            Err(e) => {
                tracing::warn!(tool = %name, error = %e, "failed to load container tool");
                errors.push(format!("'{name}': {e}"));
            }
        }
    }

    ContainerLoadResult { tools, errors }
}

/// Load and prepare a single container tool from its config directory.
///
/// # Steps
///
/// 1. Read and parse `tool.toml`.
/// 2. Read and parse `capabilities.toml`.
/// 3. Create the IPC socket directory under `/tmp/cherub-ipc/`.
/// 4. Return a `ContainerTool` ready for registration.
///    (Container is not started until first `execute()` call.)
pub async fn load_one(
    tool_dir: &Path,
    name: &str,
    runtime: Arc<dyn ContainerRuntime>,
    #[cfg(feature = "credentials")] broker: Option<
        Arc<crate::tools::credential_broker::CredentialBroker>,
    >,
) -> Result<ContainerTool, CherubError> {
    // Read tool.toml.
    let tool_toml_path = tool_dir.join("tool.toml");
    let tool_toml_content = tokio::fs::read_to_string(&tool_toml_path)
        .await
        .map_err(|e| {
            CherubError::Container(format!(
                "tool.toml '{}' missing or unreadable: {e}",
                tool_toml_path.display()
            ))
        })?;
    let tool_toml = ToolToml::from_toml(&tool_toml_content)
        .map_err(|e| CherubError::Container(format!("invalid tool.toml for '{name}': {e}")))?;

    // Validate: name in tool.toml must match directory name.
    if tool_toml.name != name {
        return Err(CherubError::Container(format!(
            "tool.toml name '{}' does not match directory name '{name}'",
            tool_toml.name
        )));
    }

    // Read capabilities.toml.
    let caps_path = tool_dir.join("capabilities.toml");
    let caps_content = tokio::fs::read_to_string(&caps_path).await.map_err(|e| {
        CherubError::Container(format!(
            "capabilities.toml '{}' missing or unreadable: {e}",
            caps_path.display()
        ))
    })?;
    let capabilities = ContainerCapabilities::from_toml(&caps_content).map_err(|e| {
        CherubError::Container(format!("invalid capabilities.toml for '{name}': {e}"))
    })?;

    // Convert TOML schema to JSON.
    let schema = toml_to_json(tool_toml.schema);

    // Create IPC socket directory.
    let ipc_dir = ipc_socket_dir(name);
    tokio::fs::create_dir_all(&ipc_dir).await.map_err(|e| {
        CherubError::Container(format!(
            "failed to create IPC dir '{}': {e}",
            ipc_dir.display()
        ))
    })?;

    let metadata = ContainerToolMetadata {
        name: tool_toml.name,
        description: tool_toml.description,
        image: tool_toml.image,
        schema,
    };

    let tool = ContainerTool::new(metadata, runtime, capabilities, ipc_dir);

    // Attach broker if available.
    #[cfg(feature = "credentials")]
    let tool = if let Some(b) = broker {
        tool.with_broker(b)
    } else {
        tool
    };

    Ok(tool)
}

/// Returns the IPC socket directory for a tool.
///
/// Each tool gets its own directory: `/tmp/cherub-ipc/{name}/`
/// The socket file will be `/tmp/cherub-ipc/{name}/tool.sock`.
fn ipc_socket_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join("cherub-ipc").join(name)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_to_json_primitives() {
        assert_eq!(
            toml_to_json(toml::Value::Boolean(true)),
            serde_json::json!(true)
        );
        assert_eq!(
            toml_to_json(toml::Value::Integer(42)),
            serde_json::json!(42)
        );
        assert_eq!(
            toml_to_json(toml::Value::String("hello".to_owned())),
            serde_json::json!("hello")
        );
    }

    #[test]
    fn toml_to_json_nested_table() {
        // `required` must appear before section headers in TOML; section headers
        // capture all subsequent key=value pairs until the next header.
        let toml_str = r#"
type = "object"
required = ["text"]

[properties]
[properties.text]
type = "string"
"#;
        let val: toml::Value = toml::from_str(toml_str).unwrap();
        let json = toml_to_json(val);
        assert_eq!(json["type"], serde_json::json!("object"));
        assert_eq!(
            json["properties"]["text"]["type"],
            serde_json::json!("string")
        );
        assert_eq!(json["required"], serde_json::json!(["text"]));
    }

    #[test]
    fn tool_toml_parsing() {
        let content = r#"
name = "my-tool"
description = "Does something useful"
image = "cherub-tool-my-tool:latest"

[schema]
type = "object"

[schema.properties]
[schema.properties.input]
type = "string"

required = ["input"]
"#;
        let tool = ToolToml::from_toml(content).unwrap();
        assert_eq!(tool.name, "my-tool");
        assert_eq!(tool.image, "cherub-tool-my-tool:latest");
    }

    #[test]
    fn tool_toml_rejects_unknown_fields() {
        let content = r#"
name = "my-tool"
description = "test"
image = "img:latest"
unknown_field = true

[schema]
type = "object"
"#;
        assert!(ToolToml::from_toml(content).is_err());
    }

    #[test]
    fn tool_toml_rejects_oversized() {
        let big = "x".repeat(65_536);
        assert!(ToolToml::from_toml(&big).is_err());
    }

    #[test]
    fn ipc_socket_dir_structure() {
        let dir = ipc_socket_dir("my-tool");
        assert!(dir.ends_with("cherub-ipc/my-tool"));
    }
}
