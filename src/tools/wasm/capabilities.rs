//! Capability declarations for WASM sandboxed tools.
//!
//! Parsed from a TOML sidecar file that sits alongside each `.wasm` binary.
//! All capabilities are opt-in — a tool has no access by default.
//!
//! # Sidecar format
//!
//! ```toml
//! [workspace]
//! allowed_prefixes = ["data/", "context/"]
//!
//! [http]
//! allowed_hosts = ["api.example.com"]
//! credentials = ["example_api_key"]   # credential names to inject
//! max_requests = 50
//!
//! [secrets]
//! allowed_names = ["example_api_key", "openai_*"]
//! ```

use serde::Deserialize;

/// All capabilities that can be granted to a WASM tool.
///
/// Parsed from `<name>.capabilities.toml`. `None` means the capability
/// is disabled entirely. By default all fields are `None`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// Read files from the agent's workspace.
    pub workspace: Option<WorkspaceCapability>,
    /// Make outbound HTTP requests.
    pub http: Option<HttpCapability>,
    /// Check secret existence (never read values).
    pub secrets: Option<SecretsCapability>,
}

impl Capabilities {
    /// Parse capabilities from TOML content.
    pub fn from_toml(content: &str) -> Result<Self, String> {
        // Enforce a size limit before parsing.
        const MAX_BYTES: usize = 64 * 1024;
        if content.len() > MAX_BYTES {
            return Err(format!(
                "capabilities file too large: {} bytes (max {MAX_BYTES})",
                content.len()
            ));
        }
        toml::from_str(content).map_err(|e| format!("invalid capabilities TOML: {e}"))
    }
}

/// Workspace read capability.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCapability {
    /// Allowed path prefixes (e.g., `["data/", "context/"]`).
    ///
    /// Empty list means all relative paths are allowed (within safety
    /// constraints: no `..`, no leading `/`, no null bytes).
    pub allowed_prefixes: Vec<String>,
}

impl WorkspaceCapability {
    /// Check if `path` is covered by the declared prefixes.
    ///
    /// An empty prefix list allows all (safe) paths.
    pub fn path_allowed(&self, path: &str) -> bool {
        if self.allowed_prefixes.is_empty() {
            return true;
        }
        self.allowed_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
    }
}

/// HTTP request capability.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpCapability {
    /// Hostnames the tool is allowed to contact (exact or `*.example.com`).
    pub allowed_hosts: Vec<String>,
    /// Credential names to look up and inject for matching requests.
    ///
    /// The host runtime looks up each credential from the store and injects
    /// the value; the WASM tool never sees the raw secret.
    #[serde(default)]
    pub credentials: Vec<String>,
    /// Maximum outbound HTTP requests per execution (default: 50).
    #[serde(default = "default_max_requests")]
    pub max_requests: u32,
}

fn default_max_requests() -> u32 {
    50
}

impl HttpCapability {
    /// Check if `host` is covered by the declared allowlist.
    pub fn host_allowed(&self, host: &str) -> bool {
        self.allowed_hosts.iter().any(|pattern| {
            if let Some(suffix) = pattern.strip_prefix("*.") {
                // *.example.com matches api.example.com but not example.com
                host.ends_with(suffix)
                    && host.len() > suffix.len()
                    && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
            } else {
                host == pattern
            }
        })
    }
}

/// Secrets existence-check capability.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsCapability {
    /// Secret names this tool can check existence of.
    ///
    /// Supports trailing glob: `"openai_*"` matches `"openai_key"` and
    /// `"openai_org"` but not `"anthropic_key"`.
    pub allowed_names: Vec<String>,
}

impl SecretsCapability {
    /// Check if `name` is covered by the declared allowlist.
    pub fn is_allowed(&self, name: &str) -> bool {
        self.allowed_names.iter().any(|pattern| {
            if let Some(prefix) = pattern.strip_suffix('*') {
                name.starts_with(prefix)
            } else {
                pattern == name
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_none() {
        let caps = Capabilities::default();
        assert!(caps.workspace.is_none());
        assert!(caps.http.is_none());
        assert!(caps.secrets.is_none());
    }

    #[test]
    fn parse_full_capabilities() {
        let toml = r#"
[workspace]
allowed_prefixes = ["data/", "context/"]

[http]
allowed_hosts = ["api.example.com", "*.cdn.example.com"]
credentials = ["example_key"]
max_requests = 20

[secrets]
allowed_names = ["example_key", "openai_*"]
"#;
        let caps = Capabilities::from_toml(toml).unwrap();
        let ws = caps.workspace.unwrap();
        assert!(ws.path_allowed("data/file.json"));
        assert!(!ws.path_allowed("secret/hidden.txt"));

        let http = caps.http.unwrap();
        assert!(http.host_allowed("api.example.com"));
        assert!(http.host_allowed("static.cdn.example.com"));
        assert!(!http.host_allowed("evil.com"));
        assert_eq!(http.max_requests, 20);

        let secrets = caps.secrets.unwrap();
        assert!(secrets.is_allowed("example_key"));
        assert!(secrets.is_allowed("openai_key"));
        assert!(secrets.is_allowed("openai_org"));
        assert!(!secrets.is_allowed("anthropic_key"));
    }

    #[test]
    fn empty_prefix_list_allows_any_path() {
        let ws = WorkspaceCapability {
            allowed_prefixes: vec![],
        };
        assert!(ws.path_allowed("any/path/at/all"));
    }

    #[test]
    fn http_wildcard_host() {
        let http = HttpCapability {
            allowed_hosts: vec!["*.example.com".to_owned()],
            credentials: vec![],
            max_requests: 50,
        };
        assert!(http.host_allowed("api.example.com"));
        assert!(http.host_allowed("cdn.example.com"));
        // Root domain itself is NOT matched by *.example.com
        assert!(!http.host_allowed("example.com"));
        assert!(!http.host_allowed("evil.com"));
    }

    #[test]
    fn rejects_oversized_capabilities_file() {
        let big = "x".repeat(65_536);
        assert!(Capabilities::from_toml(&big).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let toml = r#"
[http]
allowed_hosts = ["example.com"]
unknown_field = true
"#;
        assert!(Capabilities::from_toml(toml).is_err());
    }
}
