//! HTTP tool: makes outbound API calls with runtime credential injection.
//!
//! The agent specifies a credential by name (`"credential": "stripe_api"`).
//! The broker resolves the name, validates host/capability scope, decrypts
//! the value, and injects it into the `reqwest::RequestBuilder` before sending.
//! The plaintext credential value never appears in the tool parameters, return
//! value, or session history.
//!
//! # Enforcement flow
//!
//! 1. Agent proposes `http` tool call with `action: "get"`, `url: "https://api.stripe.com/v1/..."`.
//! 2. `HttpStructured` extractor produces `"get:api.stripe.com"`.
//! 3. Enforcement evaluates against `[tools.http.actions.*]` patterns → Allow/Escalate/Reject.
//! 4. On Allow: `HttpTool::execute()` is called with a `CapabilityToken`.
//! 5. Broker injects credential (if specified), request is sent.
//! 6. Response body is scanned by `LeakDetector` before being returned.

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
    /// Create an `HttpTool` with pre-configured timeouts.
    pub fn new(broker: Arc<CredentialBroker>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
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
}
