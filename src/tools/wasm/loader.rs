//! WASM tool loader: directory scan, BLAKE3 verification, and module preparation.
//!
//! # Directory layout
//!
//! ```text
//! tools/
//! ├── example-api.wasm
//! └── example-api.capabilities.toml
//! ```
//!
//! Each `.wasm` file must have a matching `.capabilities.toml` sidecar.
//! Tools without a sidecar are rejected.
//!
//! # Security
//!
//! - BLAKE3 hash computed on load and stored in `PreparedModule.blake3_hash`
//!   for integrity tracking.
//! - Capabilities parsed with `#[serde(deny_unknown_fields)]`.
//! - Module compilation in `spawn_blocking` (CPU-bound).
//! - Description/schema extracted by briefly instantiating the component
//!   against a no-op host environment — the component cannot cause I/O at
//!   this stage because no real capabilities are granted.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wasmtime::component::Component;

use crate::error::CherubError;
use crate::tools::wasm::capabilities::Capabilities;
use crate::tools::wasm::limits::ResourceLimits;
use crate::tools::wasm::runtime::{PreparedModule, WasmToolRuntime};
use crate::tools::wasm::wrapper::{WasmTool, extract_metadata};

/// Load all WASM tools from a directory.
///
/// Returns only successfully loaded tools; errors for individual files are
/// logged as warnings and returned in the `errors` field of [`LoadResult`].
pub async fn load_from_dir(
    dir: &Path,
    runtime: Arc<WasmToolRuntime>,
    limits: Option<ResourceLimits>,
    #[cfg(feature = "credentials")] broker: Option<
        Arc<crate::tools::credential_broker::CredentialBroker>,
    >,
) -> LoadResult {
    let mut tools = Vec::new();
    let mut errors = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            return LoadResult {
                tools: vec![],
                errors: vec![format!(
                    "failed to read tools directory '{}': {e}",
                    dir.display()
                )],
            };
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
            continue;
        }

        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_owned(),
            None => {
                errors.push(format!(
                    "skipping '{}': cannot derive tool name",
                    path.display()
                ));
                continue;
            }
        };

        match load_one(
            &path,
            &name,
            Arc::clone(&runtime),
            limits.clone(),
            #[cfg(feature = "credentials")]
            broker.clone(),
        )
        .await
        {
            Ok(tool) => {
                tracing::info!(
                    tool = %name,
                    hash = %tool.module.blake3_hash,
                    "loaded WASM tool"
                );
                tools.push(tool);
            }
            Err(e) => {
                tracing::warn!(tool = %name, error = %e, "failed to load WASM tool");
                errors.push(format!("'{name}': {e}"));
            }
        }
    }

    LoadResult { tools, errors }
}

/// Result of loading a directory of WASM tools.
pub struct LoadResult {
    /// Successfully loaded tools.
    pub tools: Vec<WasmTool>,
    /// Per-file error messages for files that failed to load.
    pub errors: Vec<String>,
}

/// Load and prepare a single WASM tool from a `.wasm` file.
///
/// # Steps
///
/// 1. Read the `.wasm` file.
/// 2. Compute and store the BLAKE3 hash.
/// 3. Read and parse the matching `.capabilities.toml` sidecar.
/// 4. Compile the component in `spawn_blocking`.
/// 5. Extract `description` and `schema` by briefly instantiating.
/// 6. Return a `WasmTool` ready for registration.
pub async fn load_one(
    wasm_path: &Path,
    name: &str,
    runtime: Arc<WasmToolRuntime>,
    limits: Option<ResourceLimits>,
    #[cfg(feature = "credentials")] broker: Option<
        Arc<crate::tools::credential_broker::CredentialBroker>,
    >,
) -> Result<WasmTool, CherubError> {
    // Read the .wasm bytes.
    let wasm_bytes = tokio::fs::read(wasm_path)
        .await
        .map_err(|e| CherubError::Wasm(format!("failed to read '{}': {e}", wasm_path.display())))?;

    // Read the capabilities sidecar.
    let cap_path = sidecar_path(wasm_path);
    let cap_content = tokio::fs::read_to_string(&cap_path).await.map_err(|e| {
        CherubError::Wasm(format!(
            "capabilities sidecar '{}' missing or unreadable: {e}",
            cap_path.display()
        ))
    })?;
    let capabilities = Capabilities::from_toml(&cap_content)
        .map_err(|e| CherubError::Wasm(format!("invalid capabilities: {e}")))?;

    // Compute BLAKE3 hash of the raw .wasm bytes.
    let blake3_hash = {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&wasm_bytes);
        hasher.finalize().to_hex().to_string()
    };

    let limits = limits.unwrap_or_default();
    let name_owned = name.to_owned();
    let engine = runtime.engine.clone();
    let caps_clone = capabilities.clone();

    // Compilation and metadata extraction in spawn_blocking (CPU-bound).
    let (component_bytes, description, schema) =
        tokio::task::spawn_blocking(move || -> Result<_, CherubError> {
            // Compile the component.
            let component = Component::new(&engine, &wasm_bytes).map_err(|e| {
                CherubError::Wasm(format!("compilation of '{name_owned}' failed: {e}"))
            })?;

            // Serialize to pre-compiled bytes for fast deserialization per execution.
            let compiled = component.serialize().map_err(|e| {
                CherubError::Wasm(format!("serialization of '{name_owned}' failed: {e}"))
            })?;

            // Extract description and schema by briefly instantiating.
            let (desc, schema) = extract_metadata(&engine, &compiled, &caps_clone)?;

            Ok((compiled, desc, schema))
        })
        .await
        .map_err(|e| CherubError::Wasm(format!("compilation task panicked: {e}")))??;

    let module = Arc::new(PreparedModule {
        name: name.to_owned(),
        description,
        schema,
        capabilities,
        limits,
        blake3_hash,
        component_bytes,
    });

    let tool = WasmTool::new(module, runtime);

    // Attach broker if available.
    #[cfg(feature = "credentials")]
    let tool = if let Some(b) = broker {
        tool.with_broker(b)
    } else {
        tool
    };

    Ok(tool)
}

/// Derive the `.capabilities.toml` path from a `.wasm` path.
///
/// `tools/example-api.wasm` → `tools/example-api.capabilities.toml`
fn sidecar_path(wasm_path: &Path) -> PathBuf {
    let stem = wasm_path.file_stem().unwrap_or_default();
    let mut sidecar = wasm_path.with_file_name(stem);
    sidecar.set_extension("capabilities.toml");
    sidecar
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_path_correct() {
        let wasm = Path::new("/tools/example-api.wasm");
        let sidecar = sidecar_path(wasm);
        assert_eq!(sidecar, Path::new("/tools/example-api.capabilities.toml"));
    }

    #[test]
    fn sidecar_path_no_dir() {
        let wasm = Path::new("my-tool.wasm");
        let sidecar = sidecar_path(wasm);
        assert_eq!(sidecar, Path::new("my-tool.capabilities.toml"));
    }
}
