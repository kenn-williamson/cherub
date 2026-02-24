//! Host function proxy for container sandboxed tools (M9b).
//!
//! `ContainerHostState` is the async parallel to WASM's `HostState`: it tracks
//! all side effects from a single container tool execution and enforces resource
//! limits at the IPC boundary.
//!
//! # Security architecture
//!
//! ```text
//! Container Tool ──► host_call IPC msg ──► allowlist check ──► resource check ──► execute
//! (untrusted)        (IPC boundary)        (capabilities)       (rate limit)
//!                                                                      │
//!                          ◄── host_response IPC msg ◄── leak scan ◄──┘
//! ```
//!
//! Host functions mirror the WASM sandbox:
//! - `workspace_read(path)` — path traversal protection + prefix allowlist
//! - `http_request(...)` — allowlist + DNS rebinding protection + credential injection
//! - `secret_exists(name)` — allowlist check only (never returns values)
//! - `log(level, message)` — capped at 1000 entries, 4 KiB per message
//! - `now_millis()` — wall-clock time

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use url::Url;

use crate::tools::container::capabilities::ContainerCapabilities;

/// Maximum log entries per execution. After this, logging is silently dropped.
pub const MAX_LOG_ENTRIES: usize = 1_000;
/// Maximum bytes per log message. Longer messages are truncated.
pub const MAX_LOG_MESSAGE_BYTES: usize = 4_096;

// ─── ContainerHostState ───────────────────────────────────────────────────────

/// Per-execution host state for a container tool.
///
/// Created fresh for each `execute()` call. Tracks side effects and enforces
/// resource limits on host function calls received over IPC.
pub struct ContainerHostState {
    capabilities: ContainerCapabilities,
    http_request_count: u32,
    workspace_root: Option<PathBuf>,
    /// User ID forwarded from the enforcement context for credential lookups.
    #[allow(dead_code)] // used in M9b credential injection
    pub user_id: String,
    // Log state (aggregated from ToolMessage::Log entries).
    logs_received: usize,
    logs_dropped: usize,
}

impl ContainerHostState {
    pub fn new(capabilities: ContainerCapabilities, user_id: String) -> Self {
        Self {
            capabilities,
            http_request_count: 0,
            workspace_root: None,
            user_id,
            logs_received: 0,
            logs_dropped: 0,
        }
    }

    #[allow(dead_code)] // used by loader when workspace capability is configured
    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace_root = Some(root);
        self
    }

    // ─── Log handling ─────────────────────────────────────────────────────────

    /// Process a `Log` message from the container. Emits via `tracing`.
    pub fn handle_log(&mut self, tool_name: &str, level: &str, message: &str) {
        if self.logs_received >= MAX_LOG_ENTRIES {
            self.logs_dropped += 1;
            return;
        }
        self.logs_received += 1;

        // Truncate oversized messages.
        let truncated;
        let message = if message.len() > MAX_LOG_MESSAGE_BYTES {
            truncated = format!("{} [truncated]", &message[..MAX_LOG_MESSAGE_BYTES]);
            &truncated
        } else {
            message
        };

        match level {
            "trace" => tracing::trace!(tool = tool_name, "[container] {message}"),
            "debug" => tracing::debug!(tool = tool_name, "[container] {message}"),
            "warn" => tracing::warn!(tool = tool_name, "[container] {message}"),
            "error" => tracing::error!(tool = tool_name, "[container] {message}"),
            _ => tracing::info!(tool = tool_name, "[container] {message}"),
        }
    }

    /// Emit a warning if any logs were dropped due to the cap.
    pub fn emit_log_summary(&self, tool_name: &str) {
        if self.logs_dropped > 0 {
            tracing::warn!(
                tool = tool_name,
                dropped = self.logs_dropped,
                "container tool log cap hit — entries were dropped"
            );
        }
    }

    // ─── Host function dispatch ───────────────────────────────────────────────

    /// Dispatch a `HostCall` message to the appropriate host function.
    ///
    /// Returns a JSON value to send back as `HostResponse.result`, or an error
    /// string. Errors are surfaced to the container as `{"error": "..."}`.
    pub async fn dispatch(
        &mut self,
        tool_name: &str,
        function: &str,
        args: &serde_json::Value,
        #[cfg(feature = "credentials")] broker: Option<
            &std::sync::Arc<crate::tools::credential_broker::CredentialBroker>,
        >,
    ) -> serde_json::Value {
        let _span = tracing::debug_span!("container_host_call", tool = tool_name, function);

        let result = match function {
            "now_millis" => Ok(serde_json::json!(now_millis())),
            "workspace_read" => self.host_workspace_read(args),
            "http_request" => {
                self.host_http_request(
                    args,
                    #[cfg(feature = "credentials")]
                    broker,
                )
                .await
            }
            "secret_exists" => Ok(serde_json::json!(self.host_secret_exists(args))),
            "log" => {
                self.host_log(tool_name, args);
                Ok(serde_json::json!(null))
            }
            unknown => Err(format!("unknown host function: '{unknown}'")),
        };

        match result {
            Ok(v) => v,
            Err(e) => serde_json::json!({"error": e}),
        }
    }

    fn host_workspace_read(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
        // Return null (not an error) when the capability is not granted — consistent
        // with the WASM sandbox which returns `None` for unconfigured capabilities.
        let ws_cap = match self.capabilities.workspace.as_ref() {
            Some(c) => c,
            None => return Ok(serde_json::json!(null)),
        };
        let root = match self.workspace_root.as_ref() {
            Some(r) => r,
            None => return Ok(serde_json::json!(null)),
        };

        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "workspace_read: 'path' argument is required".to_owned())?;

        if !is_safe_relative_path(path) {
            tracing::warn!(
                path,
                tool = "container",
                "workspace_read rejected unsafe path"
            );
            return Ok(serde_json::json!(null));
        }
        if !ws_cap.path_allowed(path) {
            tracing::debug!(path, "workspace_read rejected: prefix not in allowlist");
            return Ok(serde_json::json!(null));
        }

        let abs = root.join(path);
        if !abs.starts_with(root) {
            tracing::warn!(path, "workspace_read rejected: resolved path escapes root");
            return Ok(serde_json::json!(null));
        }

        let content = std::fs::read_to_string(&abs).ok();
        Ok(match content {
            Some(s) => serde_json::json!(s),
            None => serde_json::json!(null),
        })
    }

    async fn host_http_request(
        &mut self,
        args: &serde_json::Value,
        #[cfg(feature = "credentials")] broker: Option<
            &std::sync::Arc<crate::tools::credential_broker::CredentialBroker>,
        >,
    ) -> Result<serde_json::Value, String> {
        use std::collections::HashMap;

        let http_cap = self
            .capabilities
            .http
            .as_ref()
            .ok_or_else(|| "http capability not granted".to_owned())?;

        let method = args
            .get("method")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "http_request: 'method' argument is required".to_owned())?
            .to_uppercase();
        let url_str = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "http_request: 'url' argument is required".to_owned())?;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000)
            .min(300_000);
        let body: Option<Vec<u8>> = args
            .get("body")
            .and_then(|v| v.as_str())
            .map(|s| s.as_bytes().to_vec());

        // Parse and validate URL.
        let parsed_url = Url::parse(url_str)
            .map_err(|e| format!("http_request: invalid URL '{url_str}': {e}"))?;
        let host = parsed_url
            .host_str()
            .ok_or_else(|| "http_request: URL has no host".to_owned())?
            .to_owned();

        // Allowlist check.
        if !http_cap.host_allowed(&host) {
            return Err(format!(
                "http_request: host '{host}' is not in the HTTP allowlist"
            ));
        }

        // Rate-limit check.
        if self.http_request_count >= http_cap.max_requests {
            return Err(format!(
                "http_request: rate limit exceeded ({} requests per execution)",
                http_cap.max_requests
            ));
        }
        self.http_request_count += 1;

        // DNS rebinding protection.
        reject_private_ip(url_str)?;

        // Parse caller-supplied headers.
        let extra_headers: HashMap<String, String> = args
            .get("headers")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        for key in extra_headers.keys() {
            if key.contains('\n') || key.contains('\r') || key.contains(':') {
                return Err(format!("http_request: invalid header name: '{key}'"));
            }
        }

        let headers: Vec<(String, String)> = extra_headers.into_iter().collect();

        // Credential injection (requires `credentials` feature).
        #[cfg(feature = "credentials")]
        {
            let leak_detector = crate::tools::leak_detector::LeakDetector::new();
            if let Some(b) = broker {
                for cred_name in &http_cap.credentials {
                    match b
                        .inject(&self.user_id, cred_name, &method, parsed_url.clone())
                        .await
                    {
                        Ok(injection) => {
                            headers.extend(injection.headers);
                            let _ = injection.leak_detector;
                        }
                        Err(e) => {
                            tracing::warn!(
                                credential = %cred_name,
                                error = %e,
                                "credential injection skipped"
                            );
                        }
                    }
                }
            }
            let _ = leak_detector; // moved below after response body is available
        }

        let timeout = Duration::from_millis(timeout_ms);
        let url_clone = url_str.to_owned();
        let method_clone = method.clone();

        let result: Result<(u16, String, Vec<u8>), String> = async {
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(30))
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| format!("http_request: failed to build client: {e}"))?;

            let mut req = match method_clone.as_str() {
                "GET" => client.get(&url_clone),
                "POST" => client.post(&url_clone),
                "PUT" => client.put(&url_clone),
                "DELETE" => client.delete(&url_clone),
                "PATCH" => client.patch(&url_clone),
                "HEAD" => client.head(&url_clone),
                m => return Err(format!("http_request: unsupported method: {m}")),
            };
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            if let Some(b) = body {
                req = req.body(b);
            }

            let response = req
                .send()
                .await
                .map_err(|e| format!("http_request: request failed: {e}"))?;

            let status = response.status().as_u16();
            let resp_headers: HashMap<String, String> = response
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|v| (k.as_str().to_owned(), v.to_owned()))
                })
                .collect();
            let headers_json =
                serde_json::to_string(&resp_headers).unwrap_or_else(|_| "{}".to_owned());

            const MAX_RESP: usize = 10 * 1024 * 1024;
            if let Some(cl) = response.content_length()
                && cl as usize > MAX_RESP
            {
                return Err(format!("http_request: response too large: {cl} bytes"));
            }
            let body_bytes = response
                .bytes()
                .await
                .map_err(|e| format!("http_request: failed to read body: {e}"))?;
            if body_bytes.len() > MAX_RESP {
                return Err(format!(
                    "http_request: response too large: {} bytes",
                    body_bytes.len()
                ));
            }
            Ok((status, headers_json, body_bytes.to_vec()))
        }
        .await;

        match result {
            Ok((status, resp_headers_json, body_bytes)) => {
                let body_str = String::from_utf8_lossy(&body_bytes);
                #[cfg(feature = "credentials")]
                let body_str = {
                    let detector = crate::tools::leak_detector::LeakDetector::new();
                    std::borrow::Cow::Owned(detector.redact(&body_str))
                };
                Ok(serde_json::json!({
                    "status": status,
                    "headers": resp_headers_json,
                    "body": body_str.as_ref(),
                }))
            }
            Err(e) => Err(e),
        }
    }

    fn host_secret_exists(&self, args: &serde_json::Value) -> bool {
        let secrets_cap = match &self.capabilities.secrets {
            Some(c) => c,
            None => return false,
        };
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return false,
        };
        secrets_cap.is_allowed(name)
    }

    fn host_log(&mut self, tool_name: &str, args: &serde_json::Value) {
        let level = args.get("level").and_then(|v| v.as_str()).unwrap_or("info");
        let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
        self.handle_log(tool_name, level, message);
    }
}

// ─── Shared helpers (mirrors wasm/host.rs) ───────────────────────────────────

/// Wall-clock time in milliseconds since UNIX epoch.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Validate that `path` is safe for filesystem access.
///
/// Rejects empty paths, absolute paths, paths with `..`, and paths with null bytes.
pub(crate) fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\0') {
        return false;
    }
    for component in Path::new(path).components() {
        use std::path::Component;
        match component {
            Component::ParentDir | Component::RootDir => return false,
            _ => {}
        }
    }
    true
}

/// Reject URLs that resolve to private/loopback IP ranges (DNS rebinding protection).
pub(crate) fn reject_private_ip(url: &str) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    let parsed = Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_owned())?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addr_str = format!("{host}:{port}");

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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::container::capabilities::*;

    fn make_state(caps: ContainerCapabilities) -> ContainerHostState {
        ContainerHostState::new(caps, "test-user".to_owned())
    }

    #[test]
    fn path_validation_rejects_traversal() {
        assert!(!is_safe_relative_path("../etc/passwd"));
        assert!(!is_safe_relative_path("/etc/passwd"));
        assert!(!is_safe_relative_path(""));
        assert!(!is_safe_relative_path("foo/../../etc/passwd"));
        assert!(!is_safe_relative_path("foo\0bar"));
    }

    #[test]
    fn path_validation_allows_normal_paths() {
        assert!(is_safe_relative_path("data/file.json"));
        assert!(is_safe_relative_path("context/notes.txt"));
        assert!(is_safe_relative_path("file.txt"));
    }

    #[test]
    fn log_cap_enforced() {
        let mut state = make_state(ContainerCapabilities::default());
        for i in 0..MAX_LOG_ENTRIES + 5 {
            state.handle_log("test-tool", "info", &format!("msg {i}"));
        }
        assert_eq!(state.logs_received, MAX_LOG_ENTRIES);
        assert_eq!(state.logs_dropped, 5);
    }

    #[test]
    fn secret_exists_no_capability() {
        let state = make_state(ContainerCapabilities::default());
        let args = serde_json::json!({"name": "some_key"});
        assert!(!state.host_secret_exists(&args));
    }

    #[test]
    fn secret_exists_allowlist_enforced() {
        let caps = ContainerCapabilities {
            secrets: Some(SecretsCapability {
                allowed_names: vec!["allowed_key".to_owned()],
            }),
            ..Default::default()
        };
        let state = make_state(caps);
        assert!(state.host_secret_exists(&serde_json::json!({"name": "allowed_key"})));
        assert!(!state.host_secret_exists(&serde_json::json!({"name": "other_key"})));
    }

    #[test]
    fn workspace_read_no_capability() {
        let state = make_state(ContainerCapabilities::default());
        let result = state.host_workspace_read(&serde_json::json!({"path": "data/file.txt"}));
        assert!(result.unwrap().is_null());
    }

    #[test]
    fn workspace_read_traversal_rejected() {
        let caps = ContainerCapabilities {
            workspace: Some(WorkspaceCapability {
                allowed_prefixes: vec![],
            }),
            ..Default::default()
        };
        let mut state = make_state(caps);
        state.workspace_root = Some(PathBuf::from("/workspace"));
        let result = state
            .host_workspace_read(&serde_json::json!({"path": "../etc/passwd"}))
            .unwrap();
        assert!(result.is_null());
    }

    #[tokio::test]
    async fn dispatch_now_millis() {
        let mut state = make_state(ContainerCapabilities::default());
        let result = state
            .dispatch(
                "test-tool",
                "now_millis",
                &serde_json::json!({}),
                #[cfg(feature = "credentials")]
                None,
            )
            .await;
        assert!(result.as_u64().is_some());
        let millis = result.as_u64().unwrap();
        assert!(millis > 0);
    }

    #[tokio::test]
    async fn dispatch_unknown_function() {
        let mut state = make_state(ContainerCapabilities::default());
        let result = state
            .dispatch(
                "test-tool",
                "nonexistent_function",
                &serde_json::json!({}),
                #[cfg(feature = "credentials")]
                None,
            )
            .await;
        assert!(result.get("error").is_some());
    }
}
