//! Credential broker: resolves credential names to injection-ready request parameters.
//!
//! The broker sits between the HTTP tool (which knows what the agent asked for)
//! and the credential store (which holds the encrypted values). It validates
//! host patterns and capability scope before decrypting, then returns the
//! headers/URL modifications needed — the credential value never passes through
//! the agent's context or the tool's return value.
//!
//! # Security model
//!
//! 1. **Policy gates the action**: enforcement validated `"get:api.stripe.com"` against
//!    the policy before `HttpTool::execute()` was ever called.
//! 2. **Broker gates the credential**: host patterns + capability scope checked here,
//!    in application code, as defense-in-depth.
//! 3. **Decrypt at injection boundary**: `DecryptedCredential::expose()` is called
//!    exactly once, in this file, at the point where the value is used for injection.
//! 4. **LeakDetector**: the value is registered with the detector so response
//!    bodies can be scanned before being returned to session history.
//!
//! # API design
//!
//! `inject()` takes the URL by value and returns `InjectionResult` with a possibly
//! modified URL (for `QueryParam` injection) and a list of headers to add. It does NOT
//! take or return a `reqwest::RequestBuilder` — this avoids version-specific API issues
//! and keeps the broker independent of the HTTP client implementation.

use std::sync::Arc;

use tracing::warn;
use url::Url;

use crate::error::CherubError;
use crate::storage::CredentialStore;

use super::leak_detector::LeakDetector;

/// Result of a successful credential injection.
pub(crate) struct InjectionResult {
    /// The target URL, possibly modified (e.g. query parameter appended for `QueryParam` location).
    pub(crate) url: Url,
    /// HTTP headers to add to the request, in `(name, value)` pairs.
    pub(crate) headers: Vec<(String, String)>,
    /// Leak detector pre-loaded with the credential value.
    /// Caller must scan all response bodies and error messages through this.
    pub(crate) leak_detector: LeakDetector,
}

/// Resolves credential names to request-level injection metadata.
///
/// One broker is shared across all HTTP tool instances via `Arc`.
pub struct CredentialBroker {
    store: Arc<dyn CredentialStore>,
}

impl CredentialBroker {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self { store }
    }

    /// Resolve and inject a credential into a request.
    ///
    /// # Steps
    ///
    /// 1. Retrieve the encrypted credential row.
    /// 2. Check expiration.
    /// 3. Validate that `url`'s host matches the credential's `host_patterns`.
    /// 4. Validate that `method` maps to a required capability in `capabilities`.
    /// 5. Decrypt the credential.
    /// 6. Register the plaintext value with the `LeakDetector`.
    /// 7. Build headers / modify URL based on `location`.
    /// 8. Record usage (fire-and-forget).
    ///
    /// # Returns
    ///
    /// `InjectionResult` containing:
    /// - The (possibly modified) URL
    /// - Headers to add to the request
    /// - A `LeakDetector` preloaded with the credential value
    pub(crate) async fn inject(
        &self,
        user_id: &str,
        credential_name: &str,
        method: &str,
        url: Url,
    ) -> Result<InjectionResult, CherubError> {
        use crate::storage::CredentialLocation;

        let cred = self.store.get(user_id, credential_name).await?;

        // Check expiration before decrypting.
        if let Some(expires_at) = cred.expires_at
            && expires_at < chrono::Utc::now()
        {
            return Err(CherubError::Credential(format!(
                "credential '{credential_name}' has expired"
            )));
        }

        // Validate host patterns (defense-in-depth — policy already checked the host).
        if !cred.host_patterns.is_empty() {
            let host = url
                .host_str()
                .ok_or_else(|| CherubError::Credential("target URL has no host".to_owned()))?;
            if !cred.host_patterns.iter().any(|p| host_matches(host, p)) {
                return Err(CherubError::Credential(format!(
                    "credential '{credential_name}' is not permitted for host '{host}'"
                )));
            }
        }

        // Validate capability scope: HTTP method → required capability.
        if !cred.capabilities.is_empty() {
            let required = method_to_capability(method);
            if !cred.capabilities.iter().any(|c| c == required) {
                return Err(CherubError::Credential(format!(
                    "credential '{credential_name}' does not have the '{required}' capability \
                     required for HTTP {}",
                    method.to_uppercase()
                )));
            }
        }

        // Decrypt at the injection boundary.
        let decrypted = self.store.decrypt(&cred).await?;

        // CREDENTIAL: expose_secret() at the broker injection boundary (via expose()).
        // This is the 4th expose_secret() call site in the codebase (see credential_types.rs).
        // Clone the value so `decrypted` can be dropped after this block.
        let value = decrypted.expose().to_owned();
        let cred_name_for_detector = decrypted.name().to_owned();
        drop(decrypted); // Drop SecretString immediately after use.

        // Register with leak detector before any headers are built.
        let mut leak_detector = LeakDetector::new();
        leak_detector.register(&cred_name_for_detector, &value);

        // Build injection based on location.
        let (url, headers) = match &cred.location {
            CredentialLocation::AuthorizationBearer => {
                let headers = vec![("Authorization".to_owned(), format!("Bearer {value}"))];
                (url, headers)
            }
            CredentialLocation::Header { name, prefix } => {
                let header_val = match prefix {
                    Some(p) => format!("{p} {value}"),
                    None => value.clone(),
                };
                (url, vec![(name.clone(), header_val)])
            }
            CredentialLocation::QueryParam { name } => {
                // Use url::Url directly to append the query parameter.
                // This avoids reqwest's query() API, which requires serde and may
                // have varying signatures across versions.
                let mut url_with_param = url;
                url_with_param.query_pairs_mut().append_pair(name, &value);
                (url_with_param, vec![])
            }
        };

        // Record usage — fire-and-forget, non-fatal.
        let store_clone = Arc::clone(&self.store);
        let user_id_owned = user_id.to_owned();
        let cred_name_owned = credential_name.to_owned();
        tokio::spawn(async move {
            if let Err(e) = store_clone
                .record_usage(&user_id_owned, &cred_name_owned)
                .await
            {
                warn!(
                    error = %e,
                    credential = %cred_name_owned,
                    "failed to record credential usage"
                );
            }
        });

        Ok(InjectionResult {
            url,
            headers,
            leak_detector,
        })
    }
}

/// Map an HTTP method to the required capability string.
///
/// `GET`/`HEAD`/`OPTIONS` → `"read"` (idempotent, side-effect-free)
/// `POST`/`PUT`/`PATCH`   → `"write"` (creates/modifies)
/// `DELETE`                → `"delete"` (destroys)
fn method_to_capability(method: &str) -> &'static str {
    match method.to_lowercase().as_str() {
        "get" | "head" | "options" => "read",
        "post" | "put" | "patch" => "write",
        "delete" => "delete",
        _ => "write", // Conservative default for unknown methods.
    }
}

/// Check whether `host` matches a pattern.
///
/// Pattern rules:
/// - `"api.stripe.com"` — exact match.
/// - `"*.stripe.com"` — any subdomain of `stripe.com`.
/// - `"*"` — any host (not recommended for production).
fn host_matches(host: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // *.stripe.com matches api.stripe.com and files.stripe.com, but not stripe.com itself.
        host.ends_with(suffix)
            && host.len() > suffix.len()
            && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
    } else {
        host == pattern
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_exact_match() {
        assert!(host_matches("api.stripe.com", "api.stripe.com"));
        assert!(!host_matches("files.stripe.com", "api.stripe.com"));
    }

    #[test]
    fn host_wildcard_match() {
        assert!(host_matches("api.stripe.com", "*.stripe.com"));
        assert!(host_matches("files.stripe.com", "*.stripe.com"));
        // Does not match the root domain itself.
        assert!(!host_matches("stripe.com", "*.stripe.com"));
    }

    #[test]
    fn host_wildcard_all() {
        assert!(host_matches("anything.example.com", "*"));
        assert!(host_matches("localhost", "*"));
    }

    #[test]
    fn method_capabilities() {
        assert_eq!(method_to_capability("get"), "read");
        assert_eq!(method_to_capability("GET"), "read");
        assert_eq!(method_to_capability("post"), "write");
        assert_eq!(method_to_capability("put"), "write");
        assert_eq!(method_to_capability("patch"), "write");
        assert_eq!(method_to_capability("delete"), "delete");
    }
}
