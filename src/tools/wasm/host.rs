//! Host state and host-function implementations for the WASM sandbox.
//!
//! `HostState` is the VMLogic equivalent: it tracks all side effects from
//! a single WASM execution and enforces resource limits at the host boundary.
//!
//! # Security architecture
//!
//! ```text
//! WASM Tool ──► host function ──► allowlist check ──► resource check ──► execute
//! (untrusted)   (host boundary)   (capabilities)       (rate limit)
//!                                                                │
//!                                    ◄── leak scan ◄── response ◄─
//! ```

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use url::Url;

use crate::tools::wasm::capabilities::Capabilities;

/// Maximum log entries per execution. After this, logging is silently dropped.
const MAX_LOG_ENTRIES: usize = 1_000;
/// Maximum bytes per log message. Longer messages are truncated.
const MAX_LOG_MESSAGE_BYTES: usize = 4_096;

// ─── Log types ───────────────────────────────────────────────────────────────

/// Log level matching the WIT `log-level` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Trace => write!(f, "TRACE"),
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Warn => write!(f, "WARN"),
            LogLevel::Error => write!(f, "ERROR"),
        }
    }
}

/// A single log entry collected during WASM execution.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
    // Preserved for future audit logging; not yet consumed by callers.
    #[allow(dead_code)]
    pub timestamp_millis: u64,
}

// ─── HostState ───────────────────────────────────────────────────────────────

/// Per-execution host state.
///
/// Tracks all side effects and enforces resource limits on host function calls.
/// Created fresh for each WASM execution, dropped on completion.
pub struct HostState {
    /// Collected log entries (capped at `MAX_LOG_ENTRIES`).
    logs: Vec<LogEntry>,
    /// `false` after the log cap is hit.
    logging_enabled: bool,
    /// Number of log entries dropped due to the cap.
    logs_dropped: usize,
    /// Granted capabilities for this execution.
    capabilities: Capabilities,
    /// Number of HTTP requests made so far.
    http_request_count: u32,
    /// Root of the workspace directory (for workspace-read).
    workspace_root: Option<PathBuf>,
    /// User ID for credential lookups (passed from the enforcement context).
    pub(crate) user_id: Option<String>,
}

impl std::fmt::Debug for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostState")
            .field("logs_count", &self.logs.len())
            .field("logging_enabled", &self.logging_enabled)
            .field("logs_dropped", &self.logs_dropped)
            .field("http_request_count", &self.http_request_count)
            .finish()
    }
}

impl HostState {
    /// Create a new host state with the given capabilities.
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            logs: Vec::new(),
            logging_enabled: true,
            logs_dropped: 0,
            capabilities,
            http_request_count: 0,
            workspace_root: None,
            user_id: None,
        }
    }

    /// Set the workspace root directory.
    // Used by loader when workspace capability is configured.
    #[allow(dead_code)]
    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace_root = Some(root);
        self
    }

    /// Set the user ID for credential lookups.
    pub fn with_user_id(mut self, user_id: String) -> Self {
        self.user_id = Some(user_id);
        self
    }

    /// Granted capabilities.
    // Used by wrapper.rs under `credentials` feature.
    #[cfg_attr(not(feature = "credentials"), allow(dead_code))]
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Collected log entries from this execution.
    // Used in integration tests.
    #[allow(dead_code)]
    pub fn logs(&self) -> &[LogEntry] {
        &self.logs
    }

    /// Emit logs via `tracing` after execution completes.
    pub fn emit_logs(&self, tool_name: &str) {
        for entry in &self.logs {
            match entry.level {
                LogLevel::Trace => tracing::trace!(tool = tool_name, "[wasm] {}", entry.message),
                LogLevel::Debug => tracing::debug!(tool = tool_name, "[wasm] {}", entry.message),
                LogLevel::Info => tracing::info!(tool = tool_name, "[wasm] {}", entry.message),
                LogLevel::Warn => tracing::warn!(tool = tool_name, "[wasm] {}", entry.message),
                LogLevel::Error => tracing::error!(tool = tool_name, "[wasm] {}", entry.message),
            }
        }
        if self.logs_dropped > 0 {
            tracing::warn!(
                tool = tool_name,
                dropped = self.logs_dropped,
                "WASM tool log cap hit — entries were dropped"
            );
        }
    }

    // ─── Host function implementations ───────────────────────────────────────

    /// `log(level, message)` host function.
    pub fn log(&mut self, level: LogLevel, message: String) {
        if !self.logging_enabled {
            self.logs_dropped += 1;
            return;
        }
        // Truncate oversized messages rather than rejecting them.
        let message = if message.len() > MAX_LOG_MESSAGE_BYTES {
            let mut m = message;
            m.truncate(MAX_LOG_MESSAGE_BYTES);
            m.push_str(" [truncated]");
            m
        } else {
            message
        };
        let timestamp_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.logs.push(LogEntry {
            level,
            message,
            timestamp_millis,
        });
        if self.logs.len() >= MAX_LOG_ENTRIES {
            self.logging_enabled = false;
        }
    }

    /// `now-millis() -> u64` host function.
    pub fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// `workspace-read(path) -> option<string>` host function.
    ///
    /// Returns `None` on:
    /// - Capability not granted
    /// - Path validation failure (traversal, absolute, null bytes)
    /// - Prefix not in allowlist
    /// - File not found
    ///
    /// Never returns an error that reveals filesystem structure.
    pub fn workspace_read(&self, path: &str) -> Option<String> {
        // Capability check.
        let ws_cap = self.capabilities.workspace.as_ref()?;
        let root = self.workspace_root.as_ref()?;

        // Path safety validation.
        if !is_safe_relative_path(path) {
            tracing::warn!(path, "workspace_read rejected unsafe path");
            return None;
        }

        // Prefix allowlist check.
        if !ws_cap.path_allowed(path) {
            tracing::debug!(path, "workspace_read rejected: prefix not in allowlist");
            return None;
        }

        // Resolve to absolute path and read.
        let abs = root.join(path);
        // Safety: ensure the resolved path is still inside the root.
        if !abs.starts_with(root) {
            tracing::warn!(path, "workspace_read rejected: resolved path escapes root");
            return None;
        }
        std::fs::read_to_string(&abs).ok()
    }

    /// Check and record an outbound HTTP request.
    ///
    /// Returns `Err` if the capability is not granted, the host is not
    /// allowlisted, or the rate limit is reached.
    pub fn check_http_request(&mut self, host: &str) -> Result<(), String> {
        let http_cap = self
            .capabilities
            .http
            .as_ref()
            .ok_or_else(|| "http capability not granted".to_owned())?;

        if !http_cap.host_allowed(host) {
            return Err(format!("host '{host}' is not in the HTTP allowlist"));
        }

        if self.http_request_count >= http_cap.max_requests {
            return Err(format!(
                "HTTP rate limit exceeded ({} requests)",
                http_cap.max_requests
            ));
        }
        self.http_request_count += 1;
        Ok(())
    }

    /// `secret-exists(name) -> bool` host function.
    pub fn secret_exists(&self, name: &str) -> bool {
        // Capability check first — secrets capability must be granted.
        let secrets_cap = match &self.capabilities.secrets {
            Some(c) => c,
            None => return false,
        };
        if !secrets_cap.is_allowed(name) {
            return false;
        }
        // Without a live credential store reference, we can only confirm
        // that the name is in scope. The actual existence check happens in
        // wrapper.rs where the CredentialBroker (if available) is accessible.
        // This method returns `true` when the name is allowlisted — the
        // wrapper overrides this with a real store lookup when possible.
        true
    }
}

// ─── Path safety helpers ─────────────────────────────────────────────────────

// Re-export from shared module for use within wasm host.
pub(crate) use crate::tools::path::is_safe_relative_path;

// ─── DNS rebinding protection ─────────────────────────────────────────────────

/// Reject URLs that resolve to private/loopback IP ranges.
///
/// Call this *before* making the HTTP request. DNS resolution happens inside
/// this function to give us the resolved IP for checking.
pub(crate) fn reject_private_ip(url: &str) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    let parsed = Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_owned())?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addr_str = format!("{host}:{port}");

    // Resolve and check each address.
    let addrs = addr_str
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for '{host}': {e}"))?;

    for addr in addrs {
        if is_private_ip(addr.ip()) {
            return Err(format!(
                "host '{host}' resolves to a private/loopback address (DNS rebinding protection)"
            ));
        }
    }
    Ok(())
}

fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::wasm::capabilities::*;

    fn make_state(caps: Capabilities) -> HostState {
        HostState::new(caps)
    }

    #[test]
    fn log_basic() {
        let caps = Capabilities::default();
        let mut state = make_state(caps);
        state.log(LogLevel::Info, "hello".to_owned());
        assert_eq!(state.logs().len(), 1);
        assert_eq!(state.logs()[0].message, "hello");
    }

    #[test]
    fn log_truncates_long_message() {
        let caps = Capabilities::default();
        let mut state = make_state(caps);
        let long_msg = "x".repeat(MAX_LOG_MESSAGE_BYTES + 100);
        state.log(LogLevel::Info, long_msg);
        // Truncated + " [truncated]" suffix
        assert!(state.logs()[0].message.len() <= MAX_LOG_MESSAGE_BYTES + 20);
        assert!(state.logs()[0].message.ends_with("[truncated]"));
    }

    #[test]
    fn log_cap_enforced() {
        let caps = Capabilities::default();
        let mut state = make_state(caps);
        for i in 0..MAX_LOG_ENTRIES + 10 {
            state.log(LogLevel::Info, format!("msg {i}"));
        }
        assert_eq!(state.logs().len(), MAX_LOG_ENTRIES);
        assert_eq!(state.logs_dropped, 10);
    }

    #[test]
    fn workspace_read_no_capability() {
        let caps = Capabilities::default();
        let state = make_state(caps);
        assert!(state.workspace_read("data/file.txt").is_none());
    }

    #[test]
    fn path_validation_rejects_traversal() {
        assert!(!is_safe_relative_path("../etc/passwd"));
        assert!(!is_safe_relative_path("/etc/passwd"));
        assert!(!is_safe_relative_path(""));
        assert!(!is_safe_relative_path("foo/../../etc/passwd"));
    }

    #[test]
    fn path_validation_allows_normal_paths() {
        assert!(is_safe_relative_path("data/file.json"));
        assert!(is_safe_relative_path("context/notes.txt"));
        assert!(is_safe_relative_path("file.txt"));
    }

    #[test]
    fn http_check_no_capability() {
        let caps = Capabilities::default();
        let mut state = make_state(caps);
        let err = state.check_http_request("api.example.com").unwrap_err();
        assert!(err.contains("http capability not granted"));
    }

    #[test]
    fn http_check_allowlist_enforced() {
        let caps = Capabilities {
            http: Some(HttpCapability {
                allowed_hosts: vec!["api.example.com".to_owned()],
                credentials: vec![],
                max_requests: 50,
            }),
            ..Default::default()
        };
        let mut state = make_state(caps);
        assert!(state.check_http_request("api.example.com").is_ok());
        assert!(state.check_http_request("evil.com").is_err());
    }

    #[test]
    fn http_rate_limit_enforced() {
        let caps = Capabilities {
            http: Some(HttpCapability {
                allowed_hosts: vec!["api.example.com".to_owned()],
                credentials: vec![],
                max_requests: 2,
            }),
            ..Default::default()
        };
        let mut state = make_state(caps);
        assert!(state.check_http_request("api.example.com").is_ok());
        assert!(state.check_http_request("api.example.com").is_ok());
        let err = state.check_http_request("api.example.com").unwrap_err();
        assert!(err.contains("rate limit exceeded"), "got: {err}");
    }

    #[test]
    fn secret_exists_no_capability() {
        let caps = Capabilities::default();
        let state = make_state(caps);
        assert!(!state.secret_exists("some_key"));
    }

    #[test]
    fn secret_exists_not_in_allowlist() {
        let caps = Capabilities {
            secrets: Some(SecretsCapability {
                allowed_names: vec!["allowed_key".to_owned()],
            }),
            ..Default::default()
        };
        let state = make_state(caps);
        assert!(!state.secret_exists("other_key"));
        assert!(state.secret_exists("allowed_key"));
    }
}
