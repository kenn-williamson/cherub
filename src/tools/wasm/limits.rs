//! Resource limits for WASM sandbox execution.
//!
//! Provides memory and fuel (CPU) limits following NEAR/blockchain patterns
//! for deterministic, bounded execution of untrusted code.

use std::time::Duration;

use wasmtime::ResourceLimiter;

/// Default memory limit per WASM execution: 10 MiB.
pub const DEFAULT_MEMORY_LIMIT: u64 = 10 * 1024 * 1024;

/// Default fuel limit: 10 million wasmtime instructions.
pub const DEFAULT_FUEL_LIMIT: u64 = 10_000_000;

/// Default wall-clock execution timeout: 60 seconds.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Resource limits for a single WASM execution.
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    /// Maximum memory in bytes.
    pub memory_bytes: u64,
    /// Maximum fuel (instruction count).
    pub fuel: u64,
    /// Maximum wall-clock execution time.
    pub timeout: Duration,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            memory_bytes: DEFAULT_MEMORY_LIMIT,
            fuel: DEFAULT_FUEL_LIMIT,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl ResourceLimits {
    /// Override memory limit.
    pub fn with_memory(mut self, bytes: u64) -> Self {
        self.memory_bytes = bytes;
        self
    }

    /// Override fuel limit.
    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    /// Override execution timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Wasmtime `ResourceLimiter` that enforces memory caps.
///
/// Attached to a `Store<StoreData>` for each fresh execution.
/// Tracks cumulative memory usage and denies growth beyond `memory_limit`.
pub struct WasmResourceLimiter {
    memory_limit: u64,
    memory_used: u64,
}

impl WasmResourceLimiter {
    /// Create a limiter with the given memory limit.
    ///
    /// `max_instances = 10` accommodates the WASM Component Model, which
    /// creates multiple internal instances (main component + WASI adapters).
    pub fn new(memory_limit: u64) -> Self {
        Self {
            memory_limit,
            memory_used: 0,
        }
    }

    /// Current memory usage in bytes.
    pub fn memory_used(&self) -> u64 {
        self.memory_used
    }
}

impl ResourceLimiter for WasmResourceLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        let desired_u64 = desired as u64;
        if desired_u64 > self.memory_limit {
            tracing::warn!(
                current,
                desired,
                limit = self.memory_limit,
                "WASM memory growth denied: would exceed limit"
            );
            return Ok(false);
        }
        self.memory_used = desired_u64;
        tracing::trace!(current, desired, "WASM memory growth allowed");
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        if desired > 10_000 {
            tracing::warn!(desired, "WASM table growth denied: too large");
            return Ok(false);
        }
        Ok(true)
    }

    fn instances(&self) -> usize {
        // Component model needs multiple instances for WASI adapters.
        10
    }

    fn tables(&self) -> usize {
        10
    }

    fn memories(&self) -> usize {
        // Multiple memories for component model internals.
        10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits() {
        let lim = ResourceLimits::default();
        assert_eq!(lim.memory_bytes, DEFAULT_MEMORY_LIMIT);
        assert_eq!(lim.fuel, DEFAULT_FUEL_LIMIT);
        assert_eq!(lim.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn builder_overrides() {
        let lim = ResourceLimits::default()
            .with_memory(5 * 1024 * 1024)
            .with_fuel(1_000_000)
            .with_timeout(Duration::from_secs(30));
        assert_eq!(lim.memory_bytes, 5 * 1024 * 1024);
        assert_eq!(lim.fuel, 1_000_000);
        assert_eq!(lim.timeout, Duration::from_secs(30));
    }

    #[test]
    fn limiter_allows_growth_within_limit() {
        let mut limiter = WasmResourceLimiter::new(10 * 1024 * 1024);
        assert!(limiter.memory_growing(0, 1024 * 1024, None).unwrap());
        assert_eq!(limiter.memory_used(), 1024 * 1024);
    }

    #[test]
    fn limiter_denies_growth_beyond_limit() {
        let mut limiter = WasmResourceLimiter::new(10 * 1024 * 1024);
        assert!(!limiter.memory_growing(0, 20 * 1024 * 1024, None).unwrap());
        // memory_used should not be updated on denied growth
        assert_eq!(limiter.memory_used(), 0);
    }

    #[test]
    fn limiter_denies_oversized_table() {
        let mut limiter = WasmResourceLimiter::new(10 * 1024 * 1024);
        assert!(!limiter.table_growing(0, 100_001, None).unwrap());
        assert!(limiter.table_growing(0, 1_000, None).unwrap());
    }
}
