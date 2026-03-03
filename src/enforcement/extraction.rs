//! Action extraction strategies for the enforcement layer.
//!
//! Different tools express their intended action differently:
//! - `bash` puts it in `params["command"]`, parsed via the shell module
//! - `memory` puts it in `params["action"]`, optionally qualified by `params["path"]`
//! - `http` puts it in `params["action"]` (method) + `params["url"]` (host)
//!
//! `MatchSource` selects the extraction strategy at policy-compile time.
//! No changes to `evaluate()` are needed when adding new structured tools.

use super::shell;

/// How to extract matchable action strings from a tool invocation's params.
///
/// `Copy` so it can be stored on `CompiledTool` without affecting cloneability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MatchSource {
    /// Extract `params["command"]`, parse via the shell module.
    /// Each sub-command (split on `;`, `&&`, `|`, etc.) becomes a separate action string.
    Command,
    /// Extract `params["action"]`, optionally qualified by `params["path"]`.
    /// Produces a single action string: `"{action}:{path}"` or `"{action}"`.
    Structured,
    /// Extract `params["action"]` (HTTP method) + `params["url"]` (host).
    /// Produces a single action string: `"{method}:{host}"`, e.g. `"get:api.stripe.com"`.
    /// Malformed URL → `None` → Reject.
    HttpStructured,
    /// Extract `params["__mcp_server"]` + `params["__mcp_tool"]`.
    /// Produces a single action string: `"{server}:{tool}"`, e.g. `"google-workspace:list_events"`.
    /// Missing/empty fields → `None` → Reject.
    McpStructured,
}

impl MatchSource {
    /// Extract matchable action strings from tool invocation params.
    ///
    /// Returns `None` if the params are malformed or unparseable (→ Reject).
    /// Returns `Some([])` is never produced — an empty list is treated as `None`.
    pub(super) fn extract(self, params: &serde_json::Value) -> Option<Vec<String>> {
        match self {
            MatchSource::Command => {
                let command = params
                    .get("command")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())?;

                let sub_commands = shell::parse_commands(command)?;

                if sub_commands.is_empty() {
                    return None;
                }

                Some(sub_commands.iter().map(|s| s.to_string()).collect())
            }
            MatchSource::Structured => {
                let action = params
                    .get("action")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())?;

                let action_str = match params.get("path").and_then(|v| v.as_str()) {
                    Some(path) if !path.is_empty() => format!("{action}:{path}"),
                    _ => action.to_owned(),
                };

                Some(vec![action_str])
            }
            MatchSource::HttpStructured => {
                let action = params
                    .get("action")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())?;

                let url_str = params
                    .get("url")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())?;

                let host = extract_url_host(url_str)?;
                Some(vec![format!("{action}:{host}")])
            }
            MatchSource::McpStructured => {
                let server = params
                    .get("__mcp_server")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())?;

                let tool = params
                    .get("__mcp_tool")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())?;

                Some(vec![format!("{server}:{tool}")])
            }
        }
    }
}

/// Extract the host component from a URL string.
///
/// Handles standard HTTP/HTTPS URLs. Does not handle IPv6 addresses with brackets.
/// Malformed or unusual URLs return `None` → enforcement rejects the action.
///
/// This is intentionally simple — we do not need full URL parsing for enforcement.
/// The broker uses `url::Url` for the security-critical host validation.
fn extract_url_host(url: &str) -> Option<&str> {
    // Strip scheme: "https://api.stripe.com/v1" → "api.stripe.com/v1"
    let after_scheme = url.split_once("://")?.1;
    // Strip path and query: "api.stripe.com/v1?k=v" → "api.stripe.com"
    let host_and_port = after_scheme.split('/').next()?;
    // Strip port: "api.stripe.com:443" → "api.stripe.com"
    let host = host_and_port.split(':').next()?;

    if host.is_empty() { None } else { Some(host) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- Command extraction ---

    #[test]
    fn command_simple() {
        let params = json!({"command": "ls /tmp"});
        assert_eq!(
            MatchSource::Command.extract(&params),
            Some(vec!["ls /tmp".to_owned()])
        );
    }

    #[test]
    fn command_compound_splits() {
        let params = json!({"command": "ls /tmp; pwd"});
        let actions = MatchSource::Command.extract(&params).unwrap();
        assert_eq!(actions, vec!["ls /tmp", "pwd"]);
    }

    #[test]
    fn command_missing_returns_none() {
        let params = json!({"args": ["ls"]});
        assert!(MatchSource::Command.extract(&params).is_none());
    }

    #[test]
    fn command_empty_string_returns_none() {
        let params = json!({"command": ""});
        assert!(MatchSource::Command.extract(&params).is_none());
    }

    #[test]
    fn command_unparseable_returns_none() {
        // Here-doc is unparseable.
        let params = json!({"command": "cat <<EOF\nhello\nEOF"});
        assert!(MatchSource::Command.extract(&params).is_none());
    }

    #[test]
    fn command_non_string_returns_none() {
        let params = json!({"command": 42});
        assert!(MatchSource::Command.extract(&params).is_none());
    }

    // --- Structured extraction ---

    #[test]
    fn structured_action_only() {
        let params = json!({"action": "recall"});
        assert_eq!(
            MatchSource::Structured.extract(&params),
            Some(vec!["recall".to_owned()])
        );
    }

    #[test]
    fn structured_action_with_path() {
        let params = json!({"action": "store", "path": "preferences/food"});
        assert_eq!(
            MatchSource::Structured.extract(&params),
            Some(vec!["store:preferences/food".to_owned()])
        );
    }

    #[test]
    fn structured_empty_path_omitted() {
        let params = json!({"action": "search", "path": ""});
        assert_eq!(
            MatchSource::Structured.extract(&params),
            Some(vec!["search".to_owned()])
        );
    }

    #[test]
    fn structured_missing_action_returns_none() {
        let params = json!({"path": "preferences/food"});
        assert!(MatchSource::Structured.extract(&params).is_none());
    }

    #[test]
    fn structured_empty_action_returns_none() {
        let params = json!({"action": ""});
        assert!(MatchSource::Structured.extract(&params).is_none());
    }

    #[test]
    fn structured_non_string_action_returns_none() {
        let params = json!({"action": 42});
        assert!(MatchSource::Structured.extract(&params).is_none());
    }

    // --- HttpStructured extraction ---

    #[test]
    fn http_structured_get() {
        let params = json!({"action": "get", "url": "https://api.stripe.com/v1/charges"});
        assert_eq!(
            MatchSource::HttpStructured.extract(&params),
            Some(vec!["get:api.stripe.com".to_owned()])
        );
    }

    #[test]
    fn http_structured_post() {
        let params = json!({"action": "post", "url": "https://hooks.slack.com/services/xyz"});
        assert_eq!(
            MatchSource::HttpStructured.extract(&params),
            Some(vec!["post:hooks.slack.com".to_owned()])
        );
    }

    #[test]
    fn http_structured_strips_port() {
        let params = json!({"action": "get", "url": "https://api.example.com:8443/path"});
        assert_eq!(
            MatchSource::HttpStructured.extract(&params),
            Some(vec!["get:api.example.com".to_owned()])
        );
    }

    #[test]
    fn http_structured_missing_url_returns_none() {
        let params = json!({"action": "get"});
        assert!(MatchSource::HttpStructured.extract(&params).is_none());
    }

    #[test]
    fn http_structured_missing_action_returns_none() {
        let params = json!({"url": "https://api.stripe.com/v1"});
        assert!(MatchSource::HttpStructured.extract(&params).is_none());
    }

    #[test]
    fn http_structured_no_scheme_returns_none() {
        // No "://" → no host to extract → Reject.
        let params = json!({"action": "get", "url": "api.stripe.com/v1"});
        assert!(MatchSource::HttpStructured.extract(&params).is_none());
    }

    #[test]
    fn http_structured_empty_url_returns_none() {
        let params = json!({"action": "get", "url": ""});
        assert!(MatchSource::HttpStructured.extract(&params).is_none());
    }

    #[test]
    fn http_structured_non_string_url_returns_none() {
        let params = json!({"action": "get", "url": 42});
        assert!(MatchSource::HttpStructured.extract(&params).is_none());
    }

    // --- extract_url_host unit tests ---

    #[test]
    fn extract_host_simple() {
        assert_eq!(
            extract_url_host("https://api.stripe.com/v1"),
            Some("api.stripe.com")
        );
    }

    #[test]
    fn extract_host_with_port() {
        assert_eq!(
            extract_url_host("https://api.example.com:8443/path"),
            Some("api.example.com")
        );
    }

    #[test]
    fn extract_host_no_path() {
        assert_eq!(
            extract_url_host("https://api.example.com"),
            Some("api.example.com")
        );
    }

    #[test]
    fn extract_host_no_scheme_returns_none() {
        assert_eq!(extract_url_host("api.example.com/path"), None);
    }

    // --- McpStructured extraction ---

    #[test]
    fn mcp_structured_basic() {
        let params = json!({"__mcp_server": "google-workspace", "__mcp_tool": "list_events"});
        assert_eq!(
            MatchSource::McpStructured.extract(&params),
            Some(vec!["google-workspace:list_events".to_owned()])
        );
    }

    #[test]
    fn mcp_structured_missing_server_returns_none() {
        let params = json!({"__mcp_tool": "list_events"});
        assert!(MatchSource::McpStructured.extract(&params).is_none());
    }

    #[test]
    fn mcp_structured_missing_tool_returns_none() {
        let params = json!({"__mcp_server": "google-workspace"});
        assert!(MatchSource::McpStructured.extract(&params).is_none());
    }

    #[test]
    fn mcp_structured_empty_server_returns_none() {
        let params = json!({"__mcp_server": "", "__mcp_tool": "list_events"});
        assert!(MatchSource::McpStructured.extract(&params).is_none());
    }

    #[test]
    fn mcp_structured_empty_tool_returns_none() {
        let params = json!({"__mcp_server": "google-workspace", "__mcp_tool": ""});
        assert!(MatchSource::McpStructured.extract(&params).is_none());
    }

    #[test]
    fn mcp_structured_non_string_returns_none() {
        let params = json!({"__mcp_server": 42, "__mcp_tool": "list_events"});
        assert!(MatchSource::McpStructured.extract(&params).is_none());
    }

    #[test]
    fn mcp_structured_with_other_params() {
        // Extra params should not interfere with extraction.
        let params = json!({
            "__mcp_server": "fireflies",
            "__mcp_tool": "get_transcript",
            "meeting_id": "abc-123"
        });
        assert_eq!(
            MatchSource::McpStructured.extract(&params),
            Some(vec!["fireflies:get_transcript".to_owned()])
        );
    }
}
