//! HTTP tool: makes outbound API calls with runtime credential injection.
//!
//! The agent specifies a credential by name (`"credential": "stripe_api"`).
//! The broker resolves the name, validates host/capability scope, decrypts
//! the value, and injects it into the `reqwest::RequestBuilder` before sending.
//! The plaintext credential value never appears in the tool parameters, return
//! value, or session history.
//!
//! # Security hardening (M10)
//!
//! - **No redirects**: `reqwest` is configured with `redirect::Policy::none()`.
//!   An injected redirect could exfiltrate credentials to an attacker-controlled host.
//! - **DNS rebinding defense**: the hostname is resolved before sending.
//!   Any resolved IP in a private/loopback/link-local range is rejected.
//!   This prevents SSRF attacks where the agent is tricked into hitting internal services
//!   (e.g., `http://metadata.internal/`, `http://10.0.0.1/`).
//! - **Leak detection covers all response bodies**: the `LeakDetector` scan runs on the
//!   full body regardless of HTTP status code (2xx or error). Non-2xx bodies are still
//!   returned to the agent so it can handle API errors, but credentials are redacted first.
//!
//! # Enforcement flow
//!
//! 1. Agent proposes `http` tool call with `action: "get"`, `url: "https://api.stripe.com/v1/..."`.
//! 2. `HttpStructured` extractor produces `"get:api.stripe.com"`.
//! 3. Enforcement evaluates against `[tools.http.actions.*]` patterns → Allow/Escalate/Reject.
//! 4. On Allow: `HttpTool::execute()` is called with a `CapabilityToken`.
//! 5. DNS is resolved; any private IP rejects the call.
//! 6. Broker injects credential (if specified), request is sent without redirect following.
//! 7. Response body (any status) is scanned by `LeakDetector` before returning.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tracing::{info, warn};
use url::Url;

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;
use crate::tools::{ToolContext, ToolResult};

use super::credential_broker::CredentialBroker;
use super::leak_detector::LeakDetector;

/// Connect timeout for outbound HTTP requests.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Read timeout: time from response headers received to body complete.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Total request timeout (connect + transfer).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Maximum response body size to return (truncate beyond this).
const MAX_BODY_BYTES: usize = 256 * 1024; // 256 KiB

/// HTTP tool implementation. One instance shared across the AgentLoop lifecycle.
pub struct HttpTool {
    client: reqwest::Client,
    broker: Arc<CredentialBroker>,
}

impl HttpTool {
    /// Create an `HttpTool` with pre-configured timeouts and security policy.
    ///
    /// Security configuration applied here:
    /// - Redirects disabled (`Policy::none()`) — prevents credential exfiltration via redirect.
    /// - Timeouts enforced at connect, read, and total-request levels.
    /// - DNS rebinding check enforced per-request (see `check_dns_rebinding()`).
    pub fn new(broker: Arc<CredentialBroker>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // No redirect following: a redirect to an attacker-controlled host after
            // credential injection would exfiltrate the Authorization header.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client construction is infallible with valid config");
        Self { client, broker }
    }

    pub async fn execute(
        &self,
        params: &serde_json::Value,
        _token: CapabilityToken, // Consumed — proves enforcement cleared this call.
        ctx: &ToolContext,
    ) -> Result<ToolResult, CherubError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CherubError::InvalidInvocation("http: missing 'action'".to_owned()))?;

        let url_str = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CherubError::InvalidInvocation("http: missing 'url'".to_owned()))?;

        let url = Url::parse(url_str).map_err(|e| {
            CherubError::InvalidInvocation(format!("http: invalid url '{url_str}': {e}"))
        })?;

        // Only allow HTTP(S) — block file://, data://, etc.
        if !matches!(url.scheme(), "http" | "https") {
            return Err(CherubError::InvalidInvocation(
                "http: only http and https schemes are permitted".to_owned(),
            ));
        }

        // DNS rebinding defense (M10): resolve the hostname and reject if any resolved
        // address is in a private/loopback/link-local range. This prevents SSRF attacks
        // where the agent is tricked into targeting internal services.
        check_dns_rebinding(&url).await?;

        let method =
            reqwest::Method::from_bytes(action.to_uppercase().as_bytes()).map_err(|_| {
                CherubError::InvalidInvocation(format!("http: invalid method '{action}'"))
            })?;

        // Inject credential before building the final request — the broker may modify
        // the URL (QueryParam location) so we need the final URL before constructing
        // the RequestBuilder.
        let (final_url, credential_headers, leak_detector) =
            if let Some(cred_name) = params.get("credential").and_then(|v| v.as_str()) {
                let injection = self
                    .broker
                    .inject(&ctx.user_id, cred_name, action, url)
                    .await?;
                (injection.url, injection.headers, injection.leak_detector)
            } else {
                (url, vec![], LeakDetector::new())
            };

        let mut builder = self.client.request(method.clone(), final_url.clone());

        // Apply any extra headers from params.
        if let Some(headers) = params.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in headers {
                if let Some(val_str) = v.as_str() {
                    builder = builder.header(k.as_str(), val_str);
                }
            }
        }

        // Apply credential headers returned by the broker (Authorization, API-Key, etc.).
        for (k, v) in &credential_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        // Apply request body (for POST/PUT/PATCH).
        if let Some(body) = params.get("body").and_then(|v| v.as_str()) {
            builder = builder.body(body.to_owned());
        }

        // Send the request.
        let response = builder.send().await.map_err(|e| {
            let msg = format!("http request failed: {e}");
            // Scan error message for credential leakage before returning.
            let safe_msg = leak_detector.redact(&msg);
            CherubError::Http(safe_msg)
        })?;

        let status = response.status();
        let status_code = status.as_u16();

        // Read body with size limit.
        let body_bytes = response.bytes().await.map_err(|e| {
            let msg = format!("failed to read response body: {e}");
            let safe_msg = leak_detector.redact(&msg);
            CherubError::Http(safe_msg)
        })?;

        let body_str = if body_bytes.len() > MAX_BODY_BYTES {
            let truncated = &body_bytes[..MAX_BODY_BYTES];
            let lossy = String::from_utf8_lossy(truncated);
            format!("{lossy}\n[... truncated at {MAX_BODY_BYTES} bytes]")
        } else {
            String::from_utf8_lossy(&body_bytes).into_owned()
        };

        // Scan for leaked credential values before returning to session history.
        let safe_body = leak_detector.redact(&body_str);

        if leak_detector.contains_secret(&body_str) {
            warn!(
                url = %final_url,
                status = status_code,
                "credential value detected in HTTP response body — redacted before returning to agent"
            );
        }

        info!(
            method = %action.to_uppercase(),
            url = %final_url,
            status = status_code,
            "http request complete"
        );

        let output = format!("HTTP {status_code}\n\n{safe_body}");
        Ok(ToolResult { output })
    }
}

/// Resolve the URL hostname and reject if any IP is in a private/reserved range.
///
/// Blocks:
/// - IPv4 loopback: 127.0.0.0/8
/// - IPv4 private: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
/// - IPv4 link-local (APIPA): 169.254.0.0/16
/// - IPv6 loopback: ::1
/// - IPv6 unique-local: fc00::/7
/// - IPv6 link-local: fe80::/10
///
/// An empty resolution (no DNS records) is also rejected.
async fn check_dns_rebinding(url: &Url) -> Result<(), CherubError> {
    let host = url
        .host_str()
        .ok_or_else(|| CherubError::InvalidInvocation("http: URL has no host".to_owned()))?;

    // Bare IP addresses must be validated directly without a DNS lookup.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(ip) {
            warn!(host = %host, "http: blocked — direct private IP address");
            return Err(CherubError::InvalidInvocation(
                "http: target address is not permitted".to_owned(),
            ));
        }
        return Ok(());
    }

    let port = url.port_or_known_default().unwrap_or(443);
    let lookup_target = format!("{host}:{port}");

    let addrs = tokio::net::lookup_host(&lookup_target)
        .await
        .map_err(|e| CherubError::Http(format!("http: DNS resolution failed: {e}")))?;

    let mut resolved_any = false;
    for addr in addrs {
        resolved_any = true;
        let ip = addr.ip();
        if is_private_ip(ip) {
            warn!(host = %host, ip = %ip, "http: blocked — hostname resolves to private IP");
            return Err(CherubError::InvalidInvocation(
                "http: target address is not permitted".to_owned(),
            ));
        }
    }

    if !resolved_any {
        return Err(CherubError::Http(format!(
            "http: DNS resolution returned no addresses for '{host}'"
        )));
    }

    Ok(())
}

/// Returns true if the IP address is in a private, loopback, or link-local range.
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 127.0.0.0/8 — loopback
            octets[0] == 127
            // 10.0.0.0/8 — RFC 1918 private
            || octets[0] == 10
            // 172.16.0.0/12 — RFC 1918 private
            || (octets[0] == 172 && (octets[1] & 0xf0) == 16)
            // 192.168.0.0/16 — RFC 1918 private
            || (octets[0] == 192 && octets[1] == 168)
            // 169.254.0.0/16 — link-local (APIPA / AWS metadata)
            || (octets[0] == 169 && octets[1] == 254)
            // 0.0.0.0/8 — "this" network
            || octets[0] == 0
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            // ::1 — loopback
            v6.is_loopback()
            // fc00::/7 — unique-local (ULA)
            || (segments[0] & 0xfe00) == 0xfc00
            // fe80::/10 — link-local
            || (segments[0] & 0xffc0) == 0xfe80
            // ::ffff:0:0/96 — IPv4-mapped; check the embedded IPv4 address
            || v6.to_ipv4_mapped().is_some_and(|v4| is_private_ip(IpAddr::V4(v4)))
        }
    }
}

/// Build the JSON schema for the HTTP tool, used by the provider API.
pub fn http_tool_definition() -> crate::providers::ToolDefinition {
    crate::providers::ToolDefinition {
        name: "http".to_owned(),
        description: "Make an HTTP request to an external API. \
            Credentials are referenced by name and injected at the execution boundary — \
            their values are never visible to you. \
            Use the 'credential' field to specify which stored credential to use."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["get", "post", "put", "patch", "delete"],
                    "description": "HTTP method"
                },
                "url": {
                    "type": "string",
                    "description": "Target URL (must use https:// for external APIs)"
                },
                "headers": {
                    "type": "object",
                    "description": "Additional request headers (key/value strings)"
                },
                "body": {
                    "type": "string",
                    "description": "Request body (for POST/PUT/PATCH)"
                },
                "credential": {
                    "type": "string",
                    "description": "Name of a stored credential to inject (e.g. 'stripe_api'). \
                        The value is injected at the execution boundary — you never see it."
                }
            },
            "required": ["action", "url"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_has_required_fields() {
        let def = http_tool_definition();
        assert_eq!(def.name, "http");
        let props = def.input_schema.get("properties").unwrap();
        assert!(props.get("action").is_some());
        assert!(props.get("url").is_some());
        assert!(props.get("credential").is_some());
    }

    // ─── is_private_ip tests ─────────────────────────────────────────────────

    #[test]
    fn loopback_v4_is_private() {
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("127.255.255.255".parse().unwrap()));
    }

    #[test]
    fn rfc1918_10_is_private() {
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("10.255.255.255".parse().unwrap()));
    }

    #[test]
    fn rfc1918_172_is_private() {
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip("172.31.255.255".parse().unwrap()));
        // 172.15.x is NOT in 172.16.0.0/12
        assert!(!is_private_ip("172.15.0.1".parse().unwrap()));
        // 172.32.x is NOT in 172.16.0.0/12
        assert!(!is_private_ip("172.32.0.1".parse().unwrap()));
    }

    #[test]
    fn rfc1918_192_168_is_private() {
        assert!(is_private_ip("192.168.0.1".parse().unwrap()));
        assert!(is_private_ip("192.168.255.255".parse().unwrap()));
        assert!(!is_private_ip("192.169.0.1".parse().unwrap()));
    }

    #[test]
    fn link_local_v4_is_private() {
        // AWS EC2 instance metadata and APIPA
        assert!(is_private_ip("169.254.169.254".parse().unwrap()));
        assert!(is_private_ip("169.254.0.1".parse().unwrap()));
    }

    #[test]
    fn public_ipv4_is_not_private() {
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip("93.184.216.34".parse().unwrap()));
    }

    #[test]
    fn loopback_v6_is_private() {
        assert!(is_private_ip("::1".parse().unwrap()));
    }

    #[test]
    fn ula_v6_is_private() {
        assert!(is_private_ip("fc00::1".parse().unwrap()));
        assert!(is_private_ip("fd12:3456::1".parse().unwrap()));
    }

    #[test]
    fn link_local_v6_is_private() {
        assert!(is_private_ip("fe80::1".parse().unwrap()));
    }

    #[test]
    fn public_v6_is_not_private() {
        assert!(!is_private_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn ipv4_mapped_private_is_blocked() {
        // ::ffff:127.0.0.1 maps to 127.0.0.1
        assert!(is_private_ip("::ffff:127.0.0.1".parse().unwrap()));
        // ::ffff:10.0.0.1 maps to 10.0.0.1
        assert!(is_private_ip("::ffff:10.0.0.1".parse().unwrap()));
        // ::ffff:8.8.8.8 is public
        assert!(!is_private_ip("::ffff:8.8.8.8".parse().unwrap()));
    }

    // ─── check_dns_rebinding tests ────────────────────────────────────────────

    #[tokio::test]
    async fn direct_private_ip_blocked() {
        let url = Url::parse("https://10.0.0.1/api").unwrap();
        assert!(check_dns_rebinding(&url).await.is_err());
    }

    #[tokio::test]
    async fn direct_loopback_blocked() {
        let url = Url::parse("http://127.0.0.1:8080/").unwrap();
        assert!(check_dns_rebinding(&url).await.is_err());
    }

    #[tokio::test]
    async fn direct_link_local_blocked() {
        let url = Url::parse("http://169.254.169.254/latest/meta-data/").unwrap();
        assert!(check_dns_rebinding(&url).await.is_err());
    }

    #[tokio::test]
    async fn direct_ipv6_loopback_blocked() {
        let url = Url::parse("http://[::1]/").unwrap();
        assert!(check_dns_rebinding(&url).await.is_err());
    }
}
