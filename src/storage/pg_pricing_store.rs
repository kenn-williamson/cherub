use async_trait::async_trait;
use sqlx::Row;

use super::{Pool, PricingEntry, PricingStore};
use crate::error::CherubError;

/// PostgreSQL-backed pricing store. UPSERT/DELETE/SELECT on `model_pricing`.
pub struct PgPricingStore {
    pool: Pool,
}

impl PgPricingStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl PricingStore for PgPricingStore {
    async fn list(&self) -> Result<Vec<PricingEntry>, CherubError> {
        let rows = sqlx::query(
            "SELECT model_pattern, input_per_mtok, output_per_mtok, \
                    cache_write_per_mtok, cache_read_per_mtok \
             FROM model_pricing ORDER BY model_pattern",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CherubError::Storage(format!("list pricing failed: {e}")))?;

        Ok(rows
            .iter()
            .map(|row| PricingEntry {
                model_pattern: row.get(0),
                input_per_mtok: row.get(1),
                output_per_mtok: row.get(2),
                cache_write_per_mtok: row.get(3),
                cache_read_per_mtok: row.get(4),
            })
            .collect())
    }

    async fn set(&self, entry: PricingEntry) -> Result<(), CherubError> {
        sqlx::query(
            "INSERT INTO model_pricing \
                 (model_pattern, input_per_mtok, output_per_mtok, \
                  cache_write_per_mtok, cache_read_per_mtok, updated_at) \
             VALUES ($1, $2, $3, $4, $5, now()) \
             ON CONFLICT (model_pattern) DO UPDATE SET \
                 input_per_mtok = EXCLUDED.input_per_mtok, \
                 output_per_mtok = EXCLUDED.output_per_mtok, \
                 cache_write_per_mtok = EXCLUDED.cache_write_per_mtok, \
                 cache_read_per_mtok = EXCLUDED.cache_read_per_mtok, \
                 updated_at = now()",
        )
        .bind(&entry.model_pattern)
        .bind(entry.input_per_mtok)
        .bind(entry.output_per_mtok)
        .bind(entry.cache_write_per_mtok)
        .bind(entry.cache_read_per_mtok)
        .execute(&self.pool)
        .await
        .map_err(|e| CherubError::Storage(format!("upsert pricing failed: {e}")))?;

        Ok(())
    }

    async fn delete(&self, model_pattern: &str) -> Result<bool, CherubError> {
        let result = sqlx::query("DELETE FROM model_pricing WHERE model_pattern = $1")
            .bind(model_pattern)
            .execute(&self.pool)
            .await
            .map_err(|e| CherubError::Storage(format!("delete pricing failed: {e}")))?;

        Ok(result.rows_affected() > 0)
    }
}
