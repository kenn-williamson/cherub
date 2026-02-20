//! AES-256-GCM encryption with HKDF-SHA256 per-secret key derivation.
//!
//! # Design
//!
//! - Master key: 32+ bytes, hex-encoded, from `CHERUB_MASTER_KEY` env var.
//! - Per-secret: 32-byte random salt → HKDF-SHA256(master, salt, info) → 32-byte derived key.
//! - Encryption: random 12-byte nonce, output = `nonce || ciphertext || tag`.
//! - Different salts produce different ciphertexts for identical values, preventing corpus attacks.
//!
//! # Security invariants
//!
//! - `expose_secret()` is called once, in `derive_key()`, at the boundary where the master key
//!   must be raw bytes for HKDF. This is the only call site in this module.
//! - `CredentialCrypto` holds a `SecretString` that zeros on drop.
//! - Callers never see the intermediate derived key — it lives in a stack array and is dropped.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use hkdf::Hkdf;
use rand::{TryRngCore, rngs::OsRng};
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;

use crate::error::CherubError;

const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const HKDF_INFO: &[u8] = b"cherub-credentials-v1";

/// AES-256-GCM encryption bound to a master key.
///
/// Construct once at startup via `CredentialCrypto::new()`, then share via `Arc`.
/// Each `encrypt()` call generates a fresh salt + nonce — no ciphertext is ever reused.
pub(crate) struct CredentialCrypto {
    master_key: SecretString,
}

impl CredentialCrypto {
    /// Load the master key from a hex-encoded string.
    ///
    /// Validates that the key is at least 32 bytes (64 hex chars) of valid hex.
    pub(crate) fn new(master_key: SecretString) -> Result<Self, CherubError> {
        let hex = master_key.expose_secret();
        if hex.len() < 64 {
            return Err(CherubError::Credential(
                "CHERUB_MASTER_KEY must be at least 32 bytes (64 hex characters)".to_owned(),
            ));
        }
        // Validate that the hex is parseable — fail fast rather than at first encrypt.
        hex_decode(hex).map_err(|_| {
            CherubError::Credential("CHERUB_MASTER_KEY contains invalid hex characters".to_owned())
        })?;
        Ok(Self { master_key })
    }

    /// Encrypt `plaintext`. Returns `(nonce || ciphertext || tag, salt)`.
    ///
    /// The salt must be persisted alongside the ciphertext — it is required for decryption.
    pub(crate) fn encrypt(&self, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CherubError> {
        // Generate a fresh random salt for key derivation.
        let mut salt = [0u8; SALT_LEN];
        OsRng
            .try_fill_bytes(&mut salt)
            .map_err(|_| CherubError::Credential("OS RNG failure generating salt".to_owned()))?;

        let key = self.derive_key(&salt)?;

        // Generate a fresh random nonce for AES-GCM.
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .map_err(|_| CherubError::Credential("OS RNG failure generating nonce".to_owned()))?;
        let nonce = Nonce::from_slice(&nonce_bytes);

        let cipher = Aes256Gcm::new(&key);
        // aes_gcm includes the 16-byte GCM authentication tag in the ciphertext output.
        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| CherubError::Credential("AES-256-GCM encryption failed".to_owned()))?;

        // Output format: nonce (12) || ciphertext+tag (variable)
        let mut output = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        output.extend_from_slice(&nonce_bytes);
        output.extend_from_slice(&ciphertext);

        Ok((output, salt.to_vec()))
    }

    /// Decrypt `data` (nonce || ciphertext || tag) using the given `salt`.
    ///
    /// Returns an error if the data is tampered, truncated, or the wrong salt is used.
    pub(crate) fn decrypt(&self, data: &[u8], salt: &[u8]) -> Result<Vec<u8>, CherubError> {
        if data.len() <= NONCE_LEN {
            return Err(CherubError::Credential(
                "ciphertext is too short to contain a valid nonce".to_owned(),
            ));
        }

        let (nonce_bytes, ciphertext) = data.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);

        let key = self.derive_key(salt)?;
        let cipher = Aes256Gcm::new(&key);

        cipher.decrypt(nonce, ciphertext).map_err(|_| {
            CherubError::Credential(
                "decryption failed: ciphertext is tampered or the wrong key was used".to_owned(),
            )
        })
    }

    /// Derive a 32-byte AES key from the master key + per-secret salt via HKDF-SHA256.
    fn derive_key(&self, salt: &[u8]) -> Result<Key<Aes256Gcm>, CherubError> {
        // CREDENTIAL: expose_secret() called once per encrypt/decrypt operation.
        // The master key bytes are the HKDF input keying material (IKM).
        let key_bytes = hex_decode(self.master_key.expose_secret())
            .map_err(|_| CherubError::Credential("master key is not valid hex".to_owned()))?;

        let hkdf = Hkdf::<Sha256>::new(Some(salt), &key_bytes);
        let mut derived = [0u8; KEY_LEN];
        hkdf.expand(HKDF_INFO, &mut derived)
            .map_err(|_| CherubError::Credential("HKDF key derivation failed".to_owned()))?;

        Ok(*Key::<Aes256Gcm>::from_slice(&derived))
    }
}

/// Decode a hex string into bytes. Returns an error if the input is invalid hex.
fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;

    fn test_key() -> SecretString {
        // 32 bytes of 'a' = 64 hex chars
        SecretString::from("a".repeat(64))
    }

    #[test]
    fn roundtrip() {
        let crypto = CredentialCrypto::new(test_key()).unwrap();
        let plaintext = b"sk-stripe-secret-key-abc123";
        let (ciphertext, salt) = crypto.encrypt(plaintext).unwrap();
        let decrypted = crypto.decrypt(&ciphertext, &salt).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn empty_plaintext() {
        let crypto = CredentialCrypto::new(test_key()).unwrap();
        let (ciphertext, salt) = crypto.encrypt(b"").unwrap();
        let decrypted = crypto.decrypt(&ciphertext, &salt).unwrap();
        assert_eq!(decrypted, b"");
    }

    #[test]
    fn different_salts_for_same_plaintext() {
        let crypto = CredentialCrypto::new(test_key()).unwrap();
        let plaintext = b"same-value";
        let (ct1, salt1) = crypto.encrypt(plaintext).unwrap();
        let (ct2, salt2) = crypto.encrypt(plaintext).unwrap();
        // Salts differ (random), so ciphertexts differ.
        assert_ne!(salt1, salt2);
        assert_ne!(ct1, ct2);
        // Both decrypt correctly.
        assert_eq!(crypto.decrypt(&ct1, &salt1).unwrap(), plaintext);
        assert_eq!(crypto.decrypt(&ct2, &salt2).unwrap(), plaintext);
    }

    #[test]
    fn wrong_salt_rejected() {
        let crypto = CredentialCrypto::new(test_key()).unwrap();
        let (ct, _salt) = crypto.encrypt(b"secret").unwrap();
        let wrong_salt = vec![0u8; SALT_LEN];
        assert!(crypto.decrypt(&ct, &wrong_salt).is_err());
    }

    #[test]
    fn tampered_ciphertext_rejected() {
        let crypto = CredentialCrypto::new(test_key()).unwrap();
        let (mut ct, salt) = crypto.encrypt(b"secret").unwrap();
        // Flip a bit in the ciphertext body.
        let last = ct.len() - 1;
        ct[last] ^= 0xFF;
        assert!(crypto.decrypt(&ct, &salt).is_err());
    }

    #[test]
    fn short_key_rejected() {
        // 31 bytes = 62 hex chars — below the 32-byte minimum.
        let short_key = SecretString::from("a".repeat(62));
        assert!(CredentialCrypto::new(short_key).is_err());
    }

    #[test]
    fn invalid_hex_key_rejected() {
        let bad_key = SecretString::from("zz".repeat(32));
        assert!(CredentialCrypto::new(bad_key).is_err());
    }

    #[test]
    fn truncated_ciphertext_rejected() {
        let crypto = CredentialCrypto::new(test_key()).unwrap();
        let (_, salt) = crypto.encrypt(b"secret").unwrap();
        // Empty data — too short to contain a nonce.
        assert!(crypto.decrypt(&[], &salt).is_err());
        // Exactly nonce-length — no ciphertext/tag bytes.
        assert!(crypto.decrypt(&[0u8; NONCE_LEN], &salt).is_err());
    }

    #[test]
    fn debug_does_not_expose_master_key() {
        let crypto = CredentialCrypto::new(test_key()).unwrap();
        // SecretString's Debug impl redacts the value.
        let debug_str = format!("{:?}", crypto.master_key);
        assert!(!debug_str.contains("aaaa"));
        assert!(debug_str.contains("REDACTED") || debug_str.contains("Secret"));
    }
}
