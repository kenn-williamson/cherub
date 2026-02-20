//! PostgreSQL implementation of `CredentialStore`.
//!
//! Uses the `credentials` table from the V3 migration. All values are AES-256-GCM
//! encrypted before storage — the database never holds plaintext.
//!
//! `store()` uses INSERT ... ON CONFLICT ... DO UPDATE (upsert) so re-storing a
//! credential by the same name silently replaces the old encrypted value.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use secrecy::SecretString;
use uuid::Uuid;

use crate::error::CherubError;
use crate::storage::CredentialStore;
use crate::storage::credential_types::{
    Credential, CredentialLocation, CredentialRef, DecryptedCredential, NewCredential,
};
use crate::storage::crypto::CredentialCrypto;

/// PostgreSQL-backed credential store with AES-256-GCM encryption.
pub struct PgCredentialStore {
    pool: Pool,
    crypto: CredentialCrypto,
}

impl PgCredentialStore {
    /// Create a new store.
    ///
    /// Validates the master key immediately so misconfiguration is caught at startup.
    pub fn new(pool: Pool, master_key: SecretString) -> Result<Self, CherubError> {
        let crypto = CredentialCrypto::new(master_key)?;
        Ok(Self { pool, crypto })
    }

    fn pool_err(e: impl std::fmt::Display) -> CherubError {
        CherubError::Storage(format!("credential store pool error: {e}"))
    }

    fn query_err(e: impl std::fmt::Display) -> CherubError {
        CherubError::Storage(format!("credential store query error: {e}"))
    }

    fn not_found(name: &str) -> CherubError {
        CherubError::Credential(format!("credential not found: {name}"))
    }

    /// Convert a DB row into a `Credential`. Column order must match the SELECT below.
    fn row_to_credential(row: &tokio_postgres::Row) -> Result<Credential, CherubError> {
        let location_json: serde_json::Value = row.get("location");
        let location: CredentialLocation = serde_json::from_value(location_json)
            .map_err(|e| CherubError::Storage(format!("invalid credential location in DB: {e}")))?;

        Ok(Credential {
            id: row.get("id"),
            user_id: row.get("user_id"),
            name: row.get("name"),
            encrypted_value: row.get("encrypted_value"),
            key_salt: row.get("key_salt"),
            provider: row.get("provider"),
            capabilities: row.get("capabilities"),
            host_patterns: row.get("host_patterns"),
            location,
            expires_at: row.get("expires_at"),
            last_used_at: row.get("last_used_at"),
            usage_count: row.get("usage_count"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
    }

    fn row_to_ref(row: &tokio_postgres::Row) -> CredentialRef {
        CredentialRef {
            name: row.get("name"),
            provider: row.get("provider"),
            capabilities: row.get("capabilities"),
            host_patterns: row.get("host_patterns"),
        }
    }
}

#[async_trait]
impl CredentialStore for PgCredentialStore {
    async fn store(&self, cred: NewCredential) -> Result<Uuid, CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        // Encrypt the plaintext value before touching the DB.
        let (encrypted_value, key_salt) = self.crypto.encrypt(cred.value.as_bytes())?;

        let location_json = serde_json::to_value(&cred.location)
            .map_err(|e| CherubError::Storage(format!("failed to serialize location: {e}")))?;

        let caps: Vec<String> = cred.capabilities;
        let patterns: Vec<String> = cred.host_patterns;

        let row = conn
            .query_one(
                "INSERT INTO credentials \
                    (user_id, name, encrypted_value, key_salt, provider, \
                     capabilities, host_patterns, location, expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (user_id, name) DO UPDATE SET \
                    encrypted_value = EXCLUDED.encrypted_value, \
                    key_salt        = EXCLUDED.key_salt, \
                    provider        = EXCLUDED.provider, \
                    capabilities    = EXCLUDED.capabilities, \
                    host_patterns   = EXCLUDED.host_patterns, \
                    location        = EXCLUDED.location, \
                    expires_at      = EXCLUDED.expires_at, \
                    updated_at      = now() \
                 RETURNING id",
                &[
                    &cred.user_id,
                    &cred.name,
                    &encrypted_value,
                    &key_salt,
                    &cred.provider,
                    &caps,
                    &patterns,
                    &location_json,
                    &cred.expires_at,
                ],
            )
            .await
            .map_err(Self::query_err)?;

        Ok(row.get("id"))
    }

    async fn get(&self, user_id: &str, name: &str) -> Result<Credential, CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        let row = conn
            .query_opt(
                "SELECT id, user_id, name, encrypted_value, key_salt, provider, \
                        capabilities, host_patterns, location, expires_at, \
                        last_used_at, usage_count, created_at, updated_at \
                 FROM credentials \
                 WHERE user_id = $1 AND name = $2",
                &[&user_id, &name],
            )
            .await
            .map_err(Self::query_err)?;

        match row {
            Some(r) => Self::row_to_credential(&r),
            None => Err(Self::not_found(name)),
        }
    }

    async fn get_ref(&self, user_id: &str, name: &str) -> Result<CredentialRef, CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        let row = conn
            .query_opt(
                "SELECT name, provider, capabilities, host_patterns \
                 FROM credentials \
                 WHERE user_id = $1 AND name = $2",
                &[&user_id, &name],
            )
            .await
            .map_err(Self::query_err)?;

        match row {
            Some(r) => Ok(Self::row_to_ref(&r)),
            None => Err(Self::not_found(name)),
        }
    }

    async fn list(&self, user_id: &str) -> Result<Vec<CredentialRef>, CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        let rows = conn
            .query(
                "SELECT name, provider, capabilities, host_patterns \
                 FROM credentials \
                 WHERE user_id = $1 \
                 ORDER BY name ASC",
                &[&user_id],
            )
            .await
            .map_err(Self::query_err)?;

        Ok(rows.iter().map(Self::row_to_ref).collect())
    }

    async fn delete(&self, user_id: &str, name: &str) -> Result<(), CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        let n = conn
            .execute(
                "DELETE FROM credentials WHERE user_id = $1 AND name = $2",
                &[&user_id, &name],
            )
            .await
            .map_err(Self::query_err)?;

        if n == 0 {
            Err(Self::not_found(name))
        } else {
            Ok(())
        }
    }

    async fn exists(&self, user_id: &str, name: &str) -> Result<bool, CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        let row = conn
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM credentials WHERE user_id = $1 AND name = $2)",
                &[&user_id, &name],
            )
            .await
            .map_err(Self::query_err)?;

        Ok(row.get(0))
    }

    async fn decrypt(&self, cred: &Credential) -> Result<DecryptedCredential, CherubError> {
        let plaintext = self.crypto.decrypt(&cred.encrypted_value, &cred.key_salt)?;
        let value_str = String::from_utf8(plaintext).map_err(|_| {
            CherubError::Credential("decrypted credential value is not valid UTF-8".to_owned())
        })?;
        Ok(DecryptedCredential::new(
            cred.name.clone(),
            SecretString::from(value_str),
        ))
    }

    async fn record_usage(&self, user_id: &str, name: &str) -> Result<(), CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        conn.execute(
            "UPDATE credentials \
             SET last_used_at = now(), usage_count = usage_count + 1 \
             WHERE user_id = $1 AND name = $2",
            &[&user_id, &name],
        )
        .await
        .map_err(Self::query_err)?;

        Ok(())
    }

    async fn is_expired(&self, user_id: &str, name: &str) -> Result<bool, CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        let row = conn
            .query_opt(
                "SELECT expires_at FROM credentials WHERE user_id = $1 AND name = $2",
                &[&user_id, &name],
            )
            .await
            .map_err(Self::query_err)?;

        match row {
            None => Err(Self::not_found(name)),
            Some(r) => {
                let expires_at: Option<DateTime<Utc>> = r.get("expires_at");
                Ok(expires_at.is_some_and(|exp| exp < Utc::now()))
            }
        }
    }
}
