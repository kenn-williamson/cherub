//! Credential types for M7: vault storage, agent-safe references, and decrypted handles.
//!
//! Three-tier type hierarchy keeps the security invariants explicit in the type system:
//!
//! - [`Credential`]: encrypted DB row. Has no plaintext — only ciphertext + salt.
//! - [`CredentialRef`]: agent-visible metadata. Name + capabilities + host patterns, no value.
//! - [`DecryptedCredential`]: ephemeral plaintext handle. `pub(crate)` expose() only.
//!   No Clone, no Display — cannot accidentally leak through serialization or logging.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Encrypted credential row as stored in PostgreSQL.
///
/// Contains no plaintext — the value is AES-256-GCM encrypted with a per-secret key.
/// The `key_salt` is required for HKDF key derivation at decrypt time.
#[derive(Debug)]
pub struct Credential {
    pub id: Uuid,
    pub user_id: String,
    pub name: String,
    /// `nonce || ciphertext || tag` (AES-256-GCM output).
    pub encrypted_value: Vec<u8>,
    /// Per-secret HKDF salt (32 bytes, random). Required for decryption.
    pub key_salt: Vec<u8>,
    /// Provider hint (e.g. "stripe", "openai"). Informational only.
    pub provider: Option<String>,
    /// Capability scope: `["read"]`, `["write"]`, `["read", "write"]`, etc.
    pub capabilities: Vec<String>,
    /// Host patterns the credential may be used with (e.g. `["api.stripe.com"]`).
    /// Empty = no restriction (not recommended).
    pub host_patterns: Vec<String>,
    /// Where to inject the credential into HTTP requests.
    pub location: CredentialLocation,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub usage_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Agent-visible credential metadata. No value, no encrypted bytes.
///
/// This is what the agent sees when it lists credentials — enough to reference
/// a credential by name but nothing that could be exploited.
#[derive(Debug, Clone)]
pub struct CredentialRef {
    pub name: String,
    pub provider: Option<String>,
    pub capabilities: Vec<String>,
    pub host_patterns: Vec<String>,
}

/// Ephemeral plaintext credential handle.
///
/// # Security properties
///
/// - No `Clone`: cannot be duplicated.
/// - No `Display`: cannot appear in format strings.
/// - `Debug` redacts the value to `[REDACTED]`.
/// - `expose()` is `pub(crate)`: only code within this crate can access the value.
/// - The inner `SecretString` zeros memory on drop.
///
/// Only `credential_broker.rs` should call `expose()`.
pub struct DecryptedCredential {
    name: String,
    value: secrecy::SecretString,
}

impl DecryptedCredential {
    pub(crate) fn new(name: String, value: secrecy::SecretString) -> Self {
        Self { name, value }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Expose the plaintext value for HTTP injection.
    ///
    /// # Security
    ///
    /// Only call this from `credential_broker.rs` at the single injection point.
    /// The returned `&str` borrows `self` — it cannot outlive the `DecryptedCredential`.
    pub(crate) fn expose(&self) -> &str {
        // CREDENTIAL: expose_secret() at the broker injection boundary.
        // This is the 4th and final expose_secret() call site in the codebase:
        //   1. DB URL in storage/mod.rs
        //   2. API key in providers/anthropic.rs
        //   3. Embedding key in storage/embedding.rs
        //   4. HERE — credential broker uses this to inject into HTTP requests.
        use secrecy::ExposeSecret;
        self.value.expose_secret()
    }
}

impl std::fmt::Debug for DecryptedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecryptedCredential")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

// No Clone — intentional. Cannot duplicate a decrypted credential.
// No Display — intentional. Cannot accidentally print the value in format strings.

/// Where to inject a credential into an HTTP request.
///
/// Serializes with serde external tagging:
/// - `AuthorizationBearer` → JSON string `"AuthorizationBearer"`
/// - `Header { name, prefix }` → JSON object `{"Header": {"name": "...", "prefix": null}}`
/// - `QueryParam { name }` → JSON object `{"QueryParam": {"name": "..."}}`
///
/// The external tagging format matches the PostgreSQL JSONB default:
/// `'"AuthorizationBearer"'`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialLocation {
    /// Inject as `Authorization: Bearer <value>`.
    AuthorizationBearer,
    /// Inject as a custom header, with an optional prefix before the value.
    Header {
        name: String,
        /// If `Some("Token")`, the header value becomes `"Token <value>"`.
        prefix: Option<String>,
    },
    /// Inject as a URL query parameter.
    QueryParam { name: String },
}

/// Input for `CredentialStore::store()`. ID and timestamps are DB-generated.
#[derive(Debug)]
pub struct NewCredential {
    pub user_id: String,
    pub name: String,
    /// Plaintext value. Will be encrypted before storage; not retained after `store()`.
    pub value: String,
    pub provider: Option<String>,
    pub capabilities: Vec<String>,
    pub host_patterns: Vec<String>,
    pub location: CredentialLocation,
    pub expires_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_ref_clone() {
        let r = CredentialRef {
            name: "stripe".to_owned(),
            provider: Some("stripe".to_owned()),
            capabilities: vec!["read".to_owned()],
            host_patterns: vec!["api.stripe.com".to_owned()],
        };
        let r2 = r.clone();
        assert_eq!(r.name, r2.name);
    }

    #[test]
    fn decrypted_credential_debug_redacts_value() {
        let cred = DecryptedCredential::new(
            "stripe_api".to_owned(),
            secrecy::SecretString::from("sk-super-secret"),
        );
        let debug = format!("{cred:?}");
        assert!(debug.contains("stripe_api"));
        assert!(!debug.contains("sk-super-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn credential_location_serde_bearer() {
        let loc = CredentialLocation::AuthorizationBearer;
        let json = serde_json::to_string(&loc).unwrap();
        assert_eq!(json, r#""AuthorizationBearer""#);
        let back: CredentialLocation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, CredentialLocation::AuthorizationBearer);
    }

    #[test]
    fn credential_location_serde_header() {
        let loc = CredentialLocation::Header {
            name: "X-Api-Key".to_owned(),
            prefix: None,
        };
        let json = serde_json::to_string(&loc).unwrap();
        let back: CredentialLocation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, loc);
    }

    #[test]
    fn credential_location_serde_query_param() {
        let loc = CredentialLocation::QueryParam {
            name: "api_key".to_owned(),
        };
        let json = serde_json::to_string(&loc).unwrap();
        let back: CredentialLocation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, loc);
    }
}
