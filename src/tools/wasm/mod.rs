//! WASM sandbox for untrusted tool execution (M8).
//!
//! Provides a tiered defense-in-depth model:
//!
//! ```text
//! Layer 1: Policy enforcement     — CapabilityToken required (same path as built-in tools)
//! Layer 2: Capability declaration — TOML sidecar limits what host functions can do
//! Layer 3: WASM memory isolation  — Wasmtime linear memory, no host memory access
//! Layer 4: Resource limits        — fuel (CPU) + memory cap + epoch timeout
//! Layer 5: Host function guards   — path traversal, HTTP allowlist, DNS rebinding
//! Layer 6: Credential isolation   — broker injection + leak detection at host boundary
//! Layer 7: Fresh instance         — no shared state between executions
//! ```

pub mod capabilities;
pub(crate) mod host;
pub mod limits;
pub mod loader;
pub mod runtime;
pub(crate) mod wrapper;

pub use capabilities::Capabilities;
pub use limits::ResourceLimits;
pub use loader::{LoadResult, load_from_dir, load_one};
pub use runtime::{PreparedModule, WasmToolRuntime};
pub use wrapper::WasmTool;
