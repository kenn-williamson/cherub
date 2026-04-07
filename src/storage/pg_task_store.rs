//! PostgreSQL implementation of `TaskStore`.
//!
//! The `task_queue` table persists tool invocations that require human approval
//! during autonomous (cron) agent turns. Rows transition through a defined
//! status lifecycle: awaiting_approval → approved → running → done/failed.

use async_trait::async_trait;
use chrono::Utc;
use sqlx::Row;
use uuid::Uuid;

use crate::error::CherubError;

use super::{NewTask, Pool, Task, TaskStore};

fn query_err(e: sqlx::Error) -> CherubError {
    CherubError::Storage(format!("task_queue: {e}"))
}

/// PostgreSQL-backed task queue store. Clone-cheap (pool is Arc-internally).
pub struct PgTaskStore {
    pool: Pool,
}

impl PgTaskStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TaskStore for PgTaskStore {
    async fn create(&self, task: NewTask) -> Result<Uuid, CherubError> {
        let row = sqlx::query(
            "INSERT INTO task_queue \
             (user_id, session_id, tool, action, params, tier, description) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING id",
        )
        .bind(&task.user_id)
        .bind(task.session_id)
        .bind(&task.tool)
        .bind(&task.action)
        .bind(&task.params)
        .bind(&task.tier)
        .bind(&task.description)
        .fetch_one(&self.pool)
        .await
        .map_err(query_err)?;

        Ok(row.get("id"))
    }

    async fn set_tg_message_id(&self, id: Uuid, tg_message_id: &str) -> Result<(), CherubError> {
        sqlx::query("UPDATE task_queue SET tg_message_id = $1, updated_at = $2 WHERE id = $3")
            .bind(tg_message_id)
            .bind(Utc::now())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(query_err)?;

        Ok(())
    }

    async fn list_approved(&self, user_id: &str) -> Result<Vec<Task>, CherubError> {
        self.list_by_status(user_id, "approved").await
    }

    async fn list_pending(&self, user_id: &str) -> Result<Vec<Task>, CherubError> {
        self.list_by_status(user_id, "awaiting_approval").await
    }

    async fn mark_approved(&self, id: Uuid) -> Result<(), CherubError> {
        sqlx::query(
            "UPDATE task_queue \
             SET status = 'approved', approved_at = $1, updated_at = $1 \
             WHERE id = $2 AND status = 'awaiting_approval'",
        )
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(query_err)?;

        Ok(())
    }

    async fn mark_rejected(&self, id: Uuid) -> Result<(), CherubError> {
        sqlx::query(
            "UPDATE task_queue \
             SET status = 'rejected', updated_at = $1 \
             WHERE id = $2 AND status = 'awaiting_approval'",
        )
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(query_err)?;

        Ok(())
    }

    async fn mark_running(&self, id: Uuid) -> Result<(), CherubError> {
        sqlx::query(
            "UPDATE task_queue \
             SET status = 'running', updated_at = $1 \
             WHERE id = $2 AND status = 'approved'",
        )
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(query_err)?;

        Ok(())
    }

    async fn mark_done(&self, id: Uuid, output: &str) -> Result<(), CherubError> {
        sqlx::query(
            "UPDATE task_queue \
             SET status = 'done', result_output = $1, executed_at = $2, updated_at = $2 \
             WHERE id = $3",
        )
        .bind(output)
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(query_err)?;

        Ok(())
    }

    async fn mark_failed(&self, id: Uuid, error: &str) -> Result<(), CherubError> {
        sqlx::query(
            "UPDATE task_queue \
             SET status = 'failed', error_message = $1, executed_at = $2, updated_at = $2 \
             WHERE id = $3",
        )
        .bind(error)
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(query_err)?;

        Ok(())
    }
}

impl PgTaskStore {
    async fn list_by_status(&self, user_id: &str, status: &str) -> Result<Vec<Task>, CherubError> {
        let rows = sqlx::query(
            "SELECT id, user_id, session_id, status, tool, action, params, tier, description, \
                    result_output, error_message, created_at \
             FROM task_queue \
             WHERE user_id = $1 AND status = $2 \
             ORDER BY created_at ASC",
        )
        .bind(user_id)
        .bind(status)
        .fetch_all(&self.pool)
        .await
        .map_err(query_err)?;

        rows.into_iter()
            .map(|row| {
                Ok(Task {
                    id: row.get("id"),
                    user_id: row.get("user_id"),
                    session_id: row.get("session_id"),
                    status: row.get("status"),
                    tool: row.get("tool"),
                    action: row.get("action"),
                    params: row.get("params"),
                    tier: row.get("tier"),
                    description: row.get("description"),
                    result_output: row.get("result_output"),
                    error_message: row.get("error_message"),
                    created_at: row.get("created_at"),
                })
            })
            .collect()
    }
}
