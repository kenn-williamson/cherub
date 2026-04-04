//! Factory for building a container-sandboxed Playwright browser tool.
//!
//! Constructs a `ContainerTool` configured for headless Chromium browsing.
//! The browser runs inside an isolated Docker container with Playwright,
//! communicating via the standard IPC protocol. No workspace mount —
//! this is a pure web browsing tool.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use super::container::capabilities::ContainerCapabilities;
use super::container::runtime::ContainerRuntime;
use super::container::wrapper::{ContainerTool, ContainerToolMetadata};

/// Docker image name for the browser tool.
pub const IMAGE: &str = "cherub-browser:latest";

/// Build a container-sandboxed Playwright browser tool.
///
/// Returns `(Arc<ContainerTool>, PathBuf)`. The `PathBuf` is the IPC directory;
/// the caller must keep it alive for the lifetime of the tool.
pub fn build(runtime: Arc<dyn ContainerRuntime>) -> (Arc<ContainerTool>, PathBuf) {
    let ipc_dir = std::env::temp_dir().join(format!("cherub-browser-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&ipc_dir).expect("failed to create IPC dir for browser tool");

    let metadata = ContainerToolMetadata {
        name: "browser".to_owned(),
        description: "Browse websites using a real browser (Playwright + Chromium). \
            Navigates JavaScript-heavy pages, fills forms, clicks buttons, \
            takes screenshots. Use for sites that block automated HTTP requests."
            .to_owned(),
        schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["browse", "click", "fill", "select", "screenshot",
                             "evaluate", "wait_for", "get_text", "get_url", "scroll"],
                    "description": "Browser action to perform"
                },
                "url": {
                    "type": "string",
                    "description": "URL to navigate to (required for browse)"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector for the target element"
                },
                "value": {
                    "type": "string",
                    "description": "Value to fill or option to select"
                },
                "script": {
                    "type": "string",
                    "description": "JavaScript to evaluate in page context (for evaluate)"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Action timeout in milliseconds (default: 30000)"
                },
                "direction": {
                    "type": "string",
                    "enum": ["up", "down"],
                    "description": "Scroll direction (default: down)"
                },
                "amount": {
                    "type": "integer",
                    "description": "Scroll amount in pixels (default: 500)"
                }
            },
            "required": ["action"]
        }),
        image: IMAGE.to_owned(),
    };

    // No host functions needed — the browser makes its own HTTP requests via Chromium.
    let capabilities = ContainerCapabilities::default();

    let tool = ContainerTool::new(metadata, runtime, capabilities, ipc_dir.clone())
        .with_network("bridge") // Must access the internet
        .with_writable_rootfs() // Chromium needs writable dirs for cache/profile
        .without_tmpfs() // Chromium manages its own temp files
        .with_memory(2 * 1024 * 1024 * 1024); // 2 GiB — Chromium headroom

    (Arc::new(tool), ipc_dir)
}
