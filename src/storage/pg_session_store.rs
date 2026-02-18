use async_trait::async_trait;
use deadpool_postgres::Pool;
use uuid::Uuid;

use crate::error::CherubError;
use crate::providers::Message;
use crate::storage::SessionStore;

/// PostgreSQL implementation of `SessionStore`.
///
/// Wraps a `deadpool_postgres::Pool` for connection reuse across concurrent sessions.
pub struct PgSessionStore {
    pool: Pool,
}

impl PgSessionStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    fn pool_err(e: impl std::fmt::Display) -> CherubError {
        CherubError::Storage(format!("pool error: {e}"))
    }

    fn query_err(e: impl std::fmt::Display) -> CherubError {
        CherubError::Storage(format!("query error: {e}"))
    }

    fn serde_err(e: impl std::fmt::Display) -> CherubError {
        CherubError::Storage(format!("serde error: {e}"))
    }
}

#[async_trait]
impl SessionStore for PgSessionStore {
    async fn get_or_create_session(
        &self,
        connector: &str,
        connector_id: &str,
    ) -> Result<(Uuid, Vec<Message>), CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        // Try to find an existing session.
        let row = conn
            .query_opt(
                "SELECT id FROM sessions WHERE connector = $1 AND connector_id = $2",
                &[&connector, &connector_id],
            )
            .await
            .map_err(Self::query_err)?;

        let session_id: Uuid = if let Some(row) = row {
            row.get(0)
        } else {
            // Insert a new session. Generate the UUID in Rust (Uuid::now_v7 for time-sortable IDs).
            let new_id = Uuid::now_v7();
            conn.execute(
                "INSERT INTO sessions (id, connector, connector_id) VALUES ($1, $2, $3)",
                &[&new_id, &connector, &connector_id],
            )
            .await
            .map_err(Self::query_err)?;
            tracing::info!(
                session_id = %new_id,
                connector,
                connector_id,
                "created new session"
            );
            new_id
        };

        let messages = self.load_messages(session_id).await?;
        Ok((session_id, messages))
    }

    async fn push_message(
        &self,
        session_id: Uuid,
        ordinal: i32,
        message: &Message,
    ) -> Result<(), CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        let message_json = serde_json::to_value(message).map_err(Self::serde_err)?;
        let role = message_role_str(message);
        let msg_id = Uuid::now_v7();

        conn.execute(
            "INSERT INTO session_messages (id, session_id, ordinal, message_json, role) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (session_id, ordinal) DO UPDATE \
               SET message_json = EXCLUDED.message_json, role = EXCLUDED.role",
            &[&msg_id, &session_id, &ordinal, &message_json, &role],
        )
        .await
        .map_err(Self::query_err)?;

        // Touch the session's updated_at timestamp.
        conn.execute(
            "UPDATE sessions SET updated_at = now() WHERE id = $1",
            &[&session_id],
        )
        .await
        .map_err(Self::query_err)?;

        Ok(())
    }

    async fn load_messages(&self, session_id: Uuid) -> Result<Vec<Message>, CherubError> {
        let conn = self.pool.get().await.map_err(Self::pool_err)?;

        let rows = conn
            .query(
                "SELECT message_json FROM session_messages \
                 WHERE session_id = $1 ORDER BY ordinal ASC",
                &[&session_id],
            )
            .await
            .map_err(Self::query_err)?;

        rows.into_iter()
            .map(|row| {
                let json: serde_json::Value = row.get(0);
                serde_json::from_value(json).map_err(Self::serde_err)
            })
            .collect()
    }
}

/// Extract the role string from a message for the denormalized `role` column.
fn message_role_str(msg: &Message) -> &'static str {
    match msg {
        Message::User { .. } => "user",
        Message::Assistant { .. } => "assistant",
        Message::ToolResult { .. } => "tool_result",
    }
}
