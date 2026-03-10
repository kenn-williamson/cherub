use async_trait::async_trait;
use sqlx::Row;
use uuid::Uuid;

use crate::error::CherubError;
use crate::providers::Message;
use crate::storage::SessionStore;

use super::Pool;

/// PostgreSQL implementation of `SessionStore`.
///
/// Wraps a `sqlx::PgPool` for connection reuse across concurrent sessions.
pub struct PgSessionStore {
    pool: Pool,
}

impl PgSessionStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
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
        // Try to find an existing session.
        let row = sqlx::query("SELECT id FROM sessions WHERE connector = $1 AND connector_id = $2")
            .bind(connector)
            .bind(connector_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Self::query_err)?;

        let session_id: Uuid = if let Some(row) = row {
            row.get("id")
        } else {
            // Insert a new session. Generate the UUID in Rust (Uuid::now_v7 for time-sortable IDs).
            let new_id = Uuid::now_v7();
            sqlx::query("INSERT INTO sessions (id, connector, connector_id) VALUES ($1, $2, $3)")
                .bind(new_id)
                .bind(connector)
                .bind(connector_id)
                .execute(&self.pool)
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
        let message_json = serde_json::to_value(message).map_err(Self::serde_err)?;
        let role = message_role_str(message);
        let msg_id = Uuid::now_v7();

        sqlx::query(
            "INSERT INTO session_messages (id, session_id, ordinal, message_json, role) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (session_id, ordinal) DO UPDATE \
               SET message_json = EXCLUDED.message_json, role = EXCLUDED.role",
        )
        .bind(msg_id)
        .bind(session_id)
        .bind(ordinal)
        .bind(&message_json)
        .bind(role)
        .execute(&self.pool)
        .await
        .map_err(Self::query_err)?;

        // Touch the session's updated_at timestamp.
        sqlx::query("UPDATE sessions SET updated_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(Self::query_err)?;

        Ok(())
    }

    async fn load_messages(&self, session_id: Uuid) -> Result<Vec<Message>, CherubError> {
        let rows = sqlx::query(
            "SELECT message_json FROM session_messages \
             WHERE session_id = $1 ORDER BY ordinal ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Self::query_err)?;

        rows.into_iter()
            .map(|row| {
                let json: serde_json::Value = row.get("message_json");
                serde_json::from_value(json).map_err(Self::serde_err)
            })
            .collect()
    }
    async fn replace_messages(
        &self,
        session_id: Uuid,
        messages: &[Message],
    ) -> Result<(), CherubError> {
        let mut txn = self.pool.begin().await.map_err(Self::query_err)?;

        // Delete all existing messages for this session.
        sqlx::query("DELETE FROM session_messages WHERE session_id = $1")
            .bind(session_id)
            .execute(&mut *txn)
            .await
            .map_err(Self::query_err)?;

        // Re-insert with fresh ordinals.
        for (ordinal, message) in messages.iter().enumerate() {
            let msg_id = Uuid::now_v7();
            let ordinal = ordinal as i32;
            let message_json = serde_json::to_value(message).map_err(Self::serde_err)?;
            let role = message_role_str(message);

            sqlx::query(
                "INSERT INTO session_messages (id, session_id, ordinal, message_json, role) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(msg_id)
            .bind(session_id)
            .bind(ordinal)
            .bind(&message_json)
            .bind(role)
            .execute(&mut *txn)
            .await
            .map_err(Self::query_err)?;
        }

        // Touch the session's updated_at timestamp.
        sqlx::query("UPDATE sessions SET updated_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *txn)
            .await
            .map_err(Self::query_err)?;

        txn.commit().await.map_err(Self::query_err)?;

        tracing::info!(
            session_id = %session_id,
            message_count = messages.len(),
            "replaced session messages after compaction"
        );

        Ok(())
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
