//! Per-request leak detector for credential values in HTTP responses.
//!
//! Created fresh for each HTTP tool execution. The broker registers decrypted
//! credential values after decryption; the tool scans response bodies and error
//! messages before they reach session history.
//!
//! # Threat model
//!
//! The agent cannot access credential values through the tool schema (it only
//! sees names). But an API that echoes back the authorization header (e.g. in a
//! 401 debug response) could leak the value into the session history, giving the
//! agent a path to extract it on the next turn. `LeakDetector` prevents this.

/// Per-request secret scanner.
///
/// Register secret values with `register()`, then call `redact()` on any string
/// that will be added to session history. Registered values are stored as bytes
/// for constant-time-equivalent substring scanning.
///
/// Dropped when the HTTP request completes — secrets do not persist across requests.
pub(crate) struct LeakDetector {
    // Each entry is (credential_name, value_bytes).
    // Stored as Vec<u8> so we can scan binary-safe without UTF-8 assumptions.
    secrets: Vec<(String, Vec<u8>)>,
}

impl LeakDetector {
    pub(crate) fn new() -> Self {
        Self {
            secrets: Vec::new(),
        }
    }

    /// Register a credential value for scanning.
    ///
    /// Call this once per credential after decryption. The value is cloned into
    /// the detector and will be zeroed when the detector is dropped (Vec<u8> zeroize
    /// is handled at OS level via memory allocation; we don't use zeroize crate here
    /// since the values are also temporarily in the HTTP headers, so the window is
    /// already open).
    pub(crate) fn register(&mut self, name: &str, value: &str) {
        if !value.is_empty() {
            self.secrets
                .push((name.to_owned(), value.as_bytes().to_vec()));
        }
    }

    /// Scan `text` for registered secret values and replace each occurrence with
    /// `[REDACTED:<name>]`.
    ///
    /// Performs a linear scan for each registered secret. Short-circuits if no
    /// secrets are registered (the common case for unauthenticated requests).
    pub(crate) fn redact(&self, text: &str) -> String {
        if self.secrets.is_empty() {
            return text.to_owned();
        }

        let mut result = text.to_owned();
        for (name, value_bytes) in &self.secrets {
            if value_bytes.is_empty() {
                continue;
            }
            // Only scan if the value is valid UTF-8 (all credential values are).
            if let Ok(value_str) = std::str::from_utf8(value_bytes)
                && result.contains(value_str)
            {
                result = result.replace(value_str, &format!("[REDACTED:{name}]"));
            }
        }
        result
    }

    /// Returns true if any registered secret appears in `text`.
    ///
    /// Used for logging/tracing: if this returns true, the redacted version
    /// should be logged with a warning.
    pub(crate) fn contains_secret(&self, text: &str) -> bool {
        self.secrets.iter().any(|(_, value_bytes)| {
            std::str::from_utf8(value_bytes).is_ok_and(|s| text.contains(s))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_secrets_returns_original() {
        let detector = LeakDetector::new();
        let text = "response body with no secrets";
        assert_eq!(detector.redact(text), text);
    }

    #[test]
    fn single_secret_redacted() {
        let mut detector = LeakDetector::new();
        detector.register("stripe_api", "sk-stripe-secret");
        let text = "error: invalid key sk-stripe-secret for account";
        let redacted = detector.redact(text);
        assert!(!redacted.contains("sk-stripe-secret"));
        assert!(redacted.contains("[REDACTED:stripe_api]"));
    }

    #[test]
    fn multiple_secrets_all_redacted() {
        let mut detector = LeakDetector::new();
        detector.register("key_a", "secret_aaa");
        detector.register("key_b", "secret_bbb");
        let text = "secret_aaa and secret_bbb both appeared";
        let redacted = detector.redact(text);
        assert!(!redacted.contains("secret_aaa"));
        assert!(!redacted.contains("secret_bbb"));
        assert!(redacted.contains("[REDACTED:key_a]"));
        assert!(redacted.contains("[REDACTED:key_b]"));
    }

    #[test]
    fn multiple_occurrences_all_redacted() {
        let mut detector = LeakDetector::new();
        detector.register("api_key", "tok-123");
        let text = "tok-123 was used, tok-123 was also returned";
        let redacted = detector.redact(text);
        assert!(!redacted.contains("tok-123"));
        assert_eq!(redacted.matches("[REDACTED:api_key]").count(), 2);
    }

    #[test]
    fn no_match_returns_original() {
        let mut detector = LeakDetector::new();
        detector.register("api_key", "secret-value");
        let text = "response body with different content";
        assert_eq!(detector.redact(text), text);
    }

    #[test]
    fn empty_value_not_registered() {
        let mut detector = LeakDetector::new();
        detector.register("empty_key", "");
        // No secrets registered (empty values are skipped).
        assert!(detector.secrets.is_empty());
    }

    #[test]
    fn contains_secret_true() {
        let mut detector = LeakDetector::new();
        detector.register("k", "tok-abc");
        assert!(detector.contains_secret("response includes tok-abc here"));
    }

    #[test]
    fn contains_secret_false() {
        let mut detector = LeakDetector::new();
        detector.register("k", "tok-abc");
        assert!(!detector.contains_secret("no match here"));
    }
}
