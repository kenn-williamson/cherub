//! WASM tool runtime: engine configuration and epoch ticker.
//!
//! Follows the compile-once, instantiate-fresh-per-execution pattern for
//! deterministic, isolated WASM execution.

use std::time::Duration;

use wasmtime::{Config, Engine, OptLevel};

use crate::error::CherubError;
use crate::tools::wasm::capabilities::Capabilities;
use crate::tools::wasm::limits::ResourceLimits;

/// How often the epoch ticker increments the engine's epoch counter.
///
/// Each WASM store with an epoch deadline set will trap when the epoch
/// counter exceeds its deadline. At 500 ms ticks and a 60 s timeout,
/// the deadline is set to `60_000 / 500 = 120` epochs.
pub const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(500);

/// A compiled WASM component ready for repeated instantiation.
///
/// Created by [`prepare_module`](crate::tools::wasm::loader::prepare_module).
/// Shared (via `Arc`) between [`WasmTool`](crate::tools::wasm::WasmTool)
/// instances loaded from the same binary.
#[derive(Debug, Clone)]
pub struct PreparedModule {
    /// Tool name (derived from the filename stem).
    pub name: String,
    /// Human-readable description (extracted from the component).
    pub description: String,
    /// JSON Schema for the tool's parameters (extracted from the component).
    pub schema: serde_json::Value,
    /// Declared capabilities from the TOML sidecar.
    pub capabilities: Capabilities,
    /// Per-execution resource limits.
    pub limits: ResourceLimits,
    /// Hex-encoded BLAKE3 hash of the original `.wasm` bytes.
    pub blake3_hash: String,
    /// Pre-compiled component bytes (via `wasmtime::component::Component::serialize()`).
    /// Unsafe to deserialize unless the engine's `Config` matches exactly.
    pub(crate) component_bytes: Vec<u8>,
}

impl PreparedModule {
    /// Raw pre-compiled component bytes.
    pub fn component_bytes(&self) -> &[u8] {
        &self.component_bytes
    }
}

/// Shared WASM engine with epoch interruption and fuel metering.
///
/// Owns the background epoch-ticker thread. The engine is `Clone`-able
/// (cheap Arc clone under the hood) — clones share the same engine state.
pub struct WasmToolRuntime {
    pub(crate) engine: Engine,
}

impl WasmToolRuntime {
    /// Create a new runtime, spawning the epoch-ticker background thread.
    pub fn new() -> Result<Self, CherubError> {
        let mut config = Config::new();
        // Fuel metering: each Wasm instruction consumes one unit of fuel.
        config.consume_fuel(true);
        // Epoch interruption: backup timeout via background ticker thread.
        config.epoch_interruption(true);
        // Component model: needed for WIT typed interfaces.
        config.wasm_component_model(true);
        // Disable threads: simplifies the security model.
        config.wasm_threads(false);
        // Speed optimisation: acceptable for tools compiled ahead of time.
        config.cranelift_opt_level(OptLevel::Speed);
        // No debug info in production builds.
        config.debug_info(false);

        let engine = Engine::new(&config)
            .map_err(|e| CherubError::Wasm(format!("engine creation failed: {e}")))?;

        // Spawn a background thread that periodically increments the epoch counter.
        // Without this, `epoch_deadline_trap()` never fires and WASM modules can
        // loop indefinitely even with a deadline set on their store.
        let ticker_engine = engine.clone();
        std::thread::Builder::new()
            .name("wasm-epoch-ticker".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(EPOCH_TICK_INTERVAL);
                    ticker_engine.increment_epoch();
                }
            })
            .map_err(|e| CherubError::Wasm(format!("epoch ticker spawn failed: {e}")))?;

        Ok(Self { engine })
    }

    /// The underlying Wasmtime engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compute the epoch deadline for a given timeout.
    ///
    /// Returns the number of epoch ticks that cover `timeout`. The caller
    /// sets this as the store's epoch deadline via
    /// `store.set_epoch_deadline(deadline)`.
    pub fn epoch_deadline(timeout: Duration) -> u64 {
        // Ceiling division: ensure at least one tick even for tiny timeouts.
        let ticks = timeout
            .as_millis()
            .div_ceil(EPOCH_TICK_INTERVAL.as_millis());
        ticks.max(1) as u64
    }
}

impl std::fmt::Debug for WasmToolRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmToolRuntime")
            .field("engine", &"<Engine>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_creates_successfully() {
        WasmToolRuntime::new().expect("runtime creation should succeed");
    }

    #[test]
    fn epoch_deadline_rounds_up() {
        // 60 s / 500 ms = 120 ticks exactly
        assert_eq!(
            WasmToolRuntime::epoch_deadline(Duration::from_secs(60)),
            120
        );
        // 1 ms → should be at least 1
        assert_eq!(WasmToolRuntime::epoch_deadline(Duration::from_millis(1)), 1);
        // 501 ms → 2 ticks (ceiling)
        assert_eq!(
            WasmToolRuntime::epoch_deadline(Duration::from_millis(501)),
            2
        );
    }
}
