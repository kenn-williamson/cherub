//! PostgreSQL implementation of `CostStore` (M12).
//!
//! The `token_usage` table is append-only. Rows are never updated or deleted.
//! Running totals are computed via SUM() queries over the append-only log.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use uuid::Uuid;

use crate::error::CherubError;

use super::{CostStore, CostSummary, DailyCost, NewTokenUsage};

/// PostgreSQL-backed cost store. Clone-cheap (pool is Arc-internally).
pub struct PgCostStore {
    pool: Pool,
}

impl PgCostStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CostStore for PgCostStore {
    async fn record(&self, usage: NewTokenUsage) -> Result<Uuid, CherubError> {
        let conn =
            self.pool.get().await.map_err(|e| {
                CherubError::Storage(format!("cost: failed to get connection: {e}"))
            })?;

        let row = conn
            .query_one(
                "INSERT INTO token_usage \
                 (session_id, user_id, turn_number, model_name, input_tokens, output_tokens, cost_usd, call_type) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 RETURNING id",
                &[
                    &usage.session_id,
                    &usage.user_id,
                    &usage.turn_number,
                    &usage.model_name,
                    &(usage.input_tokens as i32),
                    &(usage.output_tokens as i32),
                    &usage.cost_usd,
                    &usage.call_type.as_str(),
                ],
            )
            .await
            .map_err(|e| CherubError::Storage(format!("cost: insert failed: {e}")))?;

        Ok(row.get(0))
    }

    async fn session_cost(&self, session_id: Uuid) -> Result<CostSummary, CherubError> {
        let conn =
            self.pool.get().await.map_err(|e| {
                CherubError::Storage(format!("cost: failed to get connection: {e}"))
            })?;

        let row = conn
            .query_one(
                "SELECT COALESCE(SUM(cost_usd), 0)::FLOAT8, \
                        COALESCE(SUM(input_tokens), 0)::BIGINT, \
                        COALESCE(SUM(output_tokens), 0)::BIGINT, \
                        COUNT(*)::BIGINT \
                 FROM token_usage \
                 WHERE session_id = $1",
                &[&session_id],
            )
            .await
            .map_err(|e| CherubError::Storage(format!("cost: session query failed: {e}")))?;

        Ok(CostSummary {
            total_cost_usd: row.get(0),
            total_input_tokens: row.get(1),
            total_output_tokens: row.get(2),
            call_count: row.get(3),
        })
    }

    async fn period_cost(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
    ) -> Result<CostSummary, CherubError> {
        let conn =
            self.pool.get().await.map_err(|e| {
                CherubError::Storage(format!("cost: failed to get connection: {e}"))
            })?;

        let row = conn
            .query_one(
                "SELECT COALESCE(SUM(cost_usd), 0)::FLOAT8, \
                        COALESCE(SUM(input_tokens), 0)::BIGINT, \
                        COALESCE(SUM(output_tokens), 0)::BIGINT, \
                        COUNT(*)::BIGINT \
                 FROM token_usage \
                 WHERE user_id = $1 AND created_at >= $2",
                &[&user_id, &since],
            )
            .await
            .map_err(|e| CherubError::Storage(format!("cost: period query failed: {e}")))?;

        Ok(CostSummary {
            total_cost_usd: row.get(0),
            total_input_tokens: row.get(1),
            total_output_tokens: row.get(2),
            call_count: row.get(3),
        })
    }

    async fn daily_costs(&self, user_id: &str, days: u32) -> Result<Vec<DailyCost>, CherubError> {
        let conn =
            self.pool.get().await.map_err(|e| {
                CherubError::Storage(format!("cost: failed to get connection: {e}"))
            })?;

        let rows = conn
            .query(
                "SELECT created_at::DATE AS day, \
                        SUM(cost_usd)::FLOAT8, \
                        SUM(input_tokens)::BIGINT, \
                        SUM(output_tokens)::BIGINT, \
                        COUNT(*)::BIGINT \
                 FROM token_usage \
                 WHERE user_id = $1 AND created_at >= now() - ($2::INT || ' days')::INTERVAL \
                 GROUP BY day \
                 ORDER BY day DESC",
                &[&user_id, &(days as i32)],
            )
            .await
            .map_err(|e| CherubError::Storage(format!("cost: daily query failed: {e}")))?;

        rows.iter()
            .map(|row| {
                Ok(DailyCost {
                    date: row.get(0),
                    total_cost_usd: row.get(1),
                    total_input_tokens: row.get(2),
                    total_output_tokens: row.get(3),
                    call_count: row.get(4),
                })
            })
            .collect()
    }
}
