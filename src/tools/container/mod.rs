//! Container sandbox for heavy/polyglot tool execution (M9).
//!
//! Provides the coarsest isolation tier: Docker/Podman containers for tools
//! that can't be compiled to WASM. Enables Python, Go, Node, and any other
//! language-agnostic plugins while maintaining the same enforcement guarantees.
//!
//! # Defense-in-depth model
//!
//! ```text
//! Layer 1: Policy enforcement     — CapabilityToken required (same path as built-in tools)
//! Layer 2: Capability declaration — TOML sidecar limits what host functions can do
//! Layer 3: Kernel isolation       — Docker namespaces, no host memory access
//! Layer 4: Resource limits        — cgroup memory/CPU, wall-clock timeout
//! Layer 5: Host function guards   — path traversal, HTTP allowlist, DNS rebinding
//! Layer 6: Credential isolation   — broker injection at IPC boundary + leak detection
//! Layer 7: tmpfs wipe             — workspace cleared between calls
//! ```
//!
//! # IPC protocol
//!
//! Bidirectional length-prefixed JSON over Unix domain sockets:
//! ```text
//! [4 bytes big-endian length] [JSON payload]
//! Max frame: 16 MiB
//! ```
//!
//! Runtime → Tool: `execute`, `host_response`, `shutdown`
//! Tool → Runtime: `registration`, `result`, `host_call`, `log`

pub mod capabilities;
pub(crate) mod host;
pub mod ipc;
pub mod loader;
pub mod runtime;
pub(crate) mod wrapper;

pub use capabilities::ContainerCapabilities;
pub use loader::{ContainerLoadResult, load_from_dir};
pub use runtime::{BollardRuntime, ContainerConfig, ContainerRuntime};
pub use wrapper::{ContainerTool, ContainerToolMetadata};
