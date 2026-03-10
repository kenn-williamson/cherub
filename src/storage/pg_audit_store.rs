//! PostgreSQL implementation of `AuditStore` (M10).
//!
//! The `audit_events` table is append-only. Rows are never updated or deleted.
//! Every enforcement decision and execution outcome is recorded.

use async_trait::async_trait;
use sqlx::Row;
use uuid::Uuid;

use crate::error::CherubError;

use super::{AuditDecision, AuditEvent, AuditFilter, AuditStore, NewAuditEvent, Pool};

/// PostgreSQL-backed audit store. Clone-cheap (pool is Arc-internally).
pub struct PgAuditStore {
    pool: Pool,
}

impl PgAuditStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AuditStore for PgAuditStore {
    async fn append(&self, event: NewAuditEvent) -> Result<Uuid, CherubError> {
        let row = sqlx::query(
            "INSERT INTO audit_events \
             (session_id, user_id, turn_number, tool, action, decision, tier, duration_ms, is_error) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             RETURNING id",
        )
        .bind(event.session_id)
        .bind(&event.user_id)
        .bind(event.turn_number)
        .bind(&event.tool)
        .bind(&event.action)
        .bind(event.decision.as_str())
        .bind(&event.tier)
        .bind(event.duration_ms)
        .bind(event.is_error)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CherubError::Storage(format!("audit: insert failed: {e}")))?;

        Ok(row.get("id"))
    }

    async fn list(&self, filter: AuditFilter) -> Result<Vec<AuditEvent>, CherubError> {
        let limit = filter.limit.unwrap_or(100);

        // Build a parameterized query with optional filters using QueryBuilder.
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT id, session_id, user_id, turn_number, tool, action, decision, tier, \
             duration_ms, is_error, created_at \
             FROM audit_events",
        );

        let mut has_where = false;
        let push_and = |qb: &mut sqlx::QueryBuilder<sqlx::Postgres>, has_where: &mut bool| {
            if *has_where {
                qb.push(" AND ");
            } else {
                qb.push(" WHERE ");
                *has_where = true;
            }
        };

        if let Some(ref tool) = filter.tool {
            push_and(&mut qb, &mut has_where);
            qb.push("tool = ").push_bind(tool.clone());
        }
        if let Some(decision) = filter.decision {
            push_and(&mut qb, &mut has_where);
            qb.push("decision = ")
                .push_bind(decision.as_str().to_owned());
        }
        if let Some(ref user_id) = filter.user_id {
            push_and(&mut qb, &mut has_where);
            qb.push("user_id = ").push_bind(user_id.clone());
        }
        if let Some(session_id) = filter.session_id {
            push_and(&mut qb, &mut has_where);
            qb.push("session_id = ").push_bind(session_id);
        }
        if let Some(since) = filter.since {
            push_and(&mut qb, &mut has_where);
            qb.push("created_at >= ").push_bind(since);
        }

        qb.push(" ORDER BY created_at DESC LIMIT ").push_bind(limit);

        let rows = qb
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CherubError::Storage(format!("audit: list query failed: {e}")))?;

        rows.into_iter()
            .map(|row| {
                let decision_str: String = row.get("decision");
                let decision = decision_str.parse::<AuditDecision>()?;
                Ok(AuditEvent {
                    id: row.get("id"),
                    session_id: row.get("session_id"),
                    user_id: row.get("user_id"),
                    turn_number: row.get("turn_number"),
                    tool: row.get("tool"),
                    action: row.get("action"),
                    decision,
                    tier: row.get("tier"),
                    duration_ms: row.get("duration_ms"),
                    is_error: row.get("is_error"),
                    created_at: row.get("created_at"),
                })
            })
            .collect()
    }
}
