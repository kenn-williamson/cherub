pub mod pg_session_store;

use async_trait::async_trait;
use deadpool_postgres::{Config, Pool, Runtime};
use secrecy::{ExposeSecret, SecretString};
use tokio_postgres::NoTls;
use uuid::Uuid;

use crate::error::CherubError;
use crate::providers::Message;

// Embed migrations at compile time so the binary is self-contained.
mod embedded {
    refinery::embed_migrations!("src/storage/migrations");
}

/// Per-concern storage trait for session persistence.
///
/// Each type of persistent thing owns its trait:
/// - `SessionStore` for sessions (M6a)
/// - `MemoryStore` for memories (M6b)
/// - `EmbeddingProvider` for vector search (M6c)
///
/// AgentLoop never accumulates type parameters — Session owns its store.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Look up or create a session for the given connector channel.
    ///
    /// `connector` identifies the connector type (e.g. "cli", "telegram").
    /// `connector_id` identifies the channel within that connector (e.g. chat_id).
    ///   Pass `None` or `"default"` for single-channel connectors like the CLI.
    ///
    /// Returns `(session_id, previous_messages)`. Messages are in ordinal order.
    async fn get_or_create_session(
        &self,
        connector: &str,
        connector_id: &str,
    ) -> Result<(Uuid, Vec<Message>), CherubError>;

    /// Persist a single message at the given ordinal position.
    ///
    /// Called by `Session::persist_last()` after each push. Failures are non-fatal
    /// (the caller logs a warning and continues in-memory).
    async fn push_message(
        &self,
        session_id: Uuid,
        ordinal: i32,
        message: &Message,
    ) -> Result<(), CherubError>;

    /// Load all messages for a session in ordinal order.
    async fn load_messages(&self, session_id: Uuid) -> Result<Vec<Message>, CherubError>;
}

/// Connect to PostgreSQL, run pending migrations, and return a connection pool.
///
/// Accepts the database URL as a `SecretString` — the embedded password is only
/// exposed at the single point where deadpool-postgres requires a plain `String`.
/// The binary is self-contained: migrations are embedded at compile time via
/// `refinery::embed_migrations!`. No migration files need to be deployed.
pub async fn connect(database_url: SecretString) -> Result<Pool, CherubError> {
    let mut cfg = Config::new();
    // CREDENTIAL: expose_secret() is the only access point for the DB password.
    cfg.url = Some(database_url.expose_secret().to_owned());
    let pool = cfg
        .create_pool(Some(Runtime::Tokio1), NoTls)
        .map_err(|e| CherubError::Storage(format!("failed to create pool: {e}")))?;

    // Run migrations on startup. refinery is idempotent — already-applied
    // migrations are skipped. Failures are fatal (bad DB state = don't start).
    {
        let mut conn = pool.get().await.map_err(|e| {
            CherubError::Storage(format!("failed to get connection for migrations: {e}"))
        })?;
        embedded::migrations::runner()
            .run_async(&mut **conn)
            .await
            .map_err(|e| CherubError::Storage(format!("migration failed: {e}")))?;
    }

    tracing::info!("database migrations complete");
    Ok(pool)
}
