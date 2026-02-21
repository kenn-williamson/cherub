//! PostgreSQL implementation of `AuditStore` (M10).
//!
//! The `audit_events` table is append-only. Rows are never updated or deleted.
//! Every enforcement decision and execution outcome is recorded.

use async_trait::async_trait;
use deadpool_postgres::Pool;
use uuid::Uuid;

use crate::error::CherubError;

use super::{AuditDecision, AuditEvent, AuditFilter, AuditStore, NewAuditEvent};

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
        let conn =
            self.pool.get().await.map_err(|e| {
                CherubError::Storage(format!("audit: failed to get connection: {e}"))
            })?;

        let row = conn
            .query_one(
                "INSERT INTO audit_events \
                 (session_id, user_id, turn_number, tool, action, decision, tier, duration_ms, is_error) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 RETURNING id",
                &[
                    &event.session_id,
                    &event.user_id,
                    &event.turn_number,
                    &event.tool,
                    &event.action,
                    &event.decision.as_str(),
                    &event.tier,
                    &event.duration_ms,
                    &event.is_error,
                ],
            )
            .await
            .map_err(|e| CherubError::Storage(format!("audit: insert failed: {e}")))?;

        Ok(row.get(0))
    }

    async fn list(&self, filter: AuditFilter) -> Result<Vec<AuditEvent>, CherubError> {
        let conn =
            self.pool.get().await.map_err(|e| {
                CherubError::Storage(format!("audit: failed to get connection: {e}"))
            })?;

        let limit = filter.limit.unwrap_or(100);

        // Build a parameterized query with optional filters.
        // We accumulate WHERE clauses and bind values together.
        // Box<dyn ... + Send> is required for the future to be Send across await points.
        let mut clauses: Vec<String> = Vec::new();
        let mut binds: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> = Vec::new();
        let mut idx: usize = 1;

        if let Some(ref tool) = filter.tool {
            clauses.push(format!("tool = ${idx}"));
            binds.push(Box::new(tool.clone()));
            idx += 1;
        }
        if let Some(decision) = filter.decision {
            clauses.push(format!("decision = ${idx}"));
            binds.push(Box::new(decision.as_str().to_owned()));
            idx += 1;
        }
        if let Some(ref user_id) = filter.user_id {
            clauses.push(format!("user_id = ${idx}"));
            binds.push(Box::new(user_id.clone()));
            idx += 1;
        }
        if let Some(session_id) = filter.session_id {
            clauses.push(format!("session_id = ${idx}"));
            binds.push(Box::new(session_id));
            idx += 1;
        }
        if let Some(since) = filter.since {
            clauses.push(format!("created_at >= ${idx}"));
            binds.push(Box::new(since));
            idx += 1;
        }

        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };

        let limit_clause = format!("LIMIT ${idx}");
        binds.push(Box::new(limit));

        let sql = format!(
            "SELECT id, session_id, user_id, turn_number, tool, action, decision, tier, \
             duration_ms, is_error, created_at \
             FROM audit_events \
             {where_clause} \
             ORDER BY created_at DESC \
             {limit_clause}"
        );

        // Collect bind params as &dyn ToSql for the query call.
        let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = binds
            .iter()
            .map(|b| b.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();

        let rows = conn
            .query(sql.as_str(), params.as_slice())
            .await
            .map_err(|e| CherubError::Storage(format!("audit: list query failed: {e}")))?;

        rows.into_iter()
            .map(|row| {
                let decision_str: String = row.get(6);
                let decision = decision_str.parse::<AuditDecision>()?;
                Ok(AuditEvent {
                    id: row.get(0),
                    session_id: row.get(1),
                    user_id: row.get(2),
                    turn_number: row.get(3),
                    tool: row.get(4),
                    action: row.get(5),
                    decision,
                    tier: row.get(7),
                    duration_ms: row.get(8),
                    is_error: row.get(9),
                    created_at: row.get(10),
                })
            })
            .collect()
    }
}
