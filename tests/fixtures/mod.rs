//! Shared test fixtures for PostgreSQL integration tests.
//!
//! Includes `MockEmbeddingProvider` for hybrid-search integration tests (M6c).
//!
//! `TestContainer` starts a reusable pgvector/pgvector:pg18 container (slot-named
//! for nextest parallelism), runs migrations via `cherub::storage::connect()`, and
//! TRUNCATEs all data tables so each test starts with a clean database.
//!
//! No manual `docker compose up` or `DATABASE_URL` env var needed.

#[cfg(feature = "memory")]
use async_trait::async_trait;
#[cfg(feature = "memory")]
use cherub::error::CherubError;
#[cfg(feature = "memory")]
use cherub::storage::embedding::EmbeddingProvider;
use deadpool_postgres::Pool;
use secrecy::SecretString;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt, ReuseDirective,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

/// TRUNCATE all data tables at the start of each test.
///
/// With nextest, each slot runs one test at a time, so the TRUNCATE is safe — no
/// concurrent test is using the data. Use `cargo nextest run` (not `cargo test`)
/// for DB integration tests, or tests in the same slot will interfere.
/// CASCADE handles FK constraints (memory_chunks → memories, session_messages → sessions).
const RESET_SQL: &str =
    "TRUNCATE TABLE memory_chunks, memories, session_messages, sessions CASCADE;";

/// Holds a running PostgreSQL container and a connected pool.
///
/// Dropping this struct does NOT stop the container (ReuseDirective::Always) —
/// it stays alive for subsequent tests in the same nextest slot.
///
/// Each nextest slot gets its own container (`cherub-test-pg-{slot}`). Tests
/// within a slot are serialized by nextest, so the per-test TRUNCATE is race-free.
pub struct TestContainer {
    _container: ContainerAsync<GenericImage>,
    pub pool: Pool,
}

impl TestContainer {
    /// Start or reuse the slot-local test container, run migrations, and TRUNCATE
    /// all data tables so each test begins with a clean slate.
    pub async fn new() -> Self {
        // Slot-based naming so each nextest parallel slot gets its own container.
        // Falls back to "0" for plain `cargo test` (single-threaded).
        let slot_id = std::env::var("NEXTEST_TEST_GLOBAL_SLOT").unwrap_or_else(|_| "0".to_string());
        let container_name = format!("cherub-test-pg-{}", slot_id);

        // Outer retry: up to 3 container start attempts.
        for container_attempt in 1u32..=3 {
            let image = GenericImage::new("pgvector/pgvector", "pg18")
                .with_exposed_port(5432.tcp())
                .with_wait_for(WaitFor::message_on_stdout(
                    "database system is ready to accept connections",
                ))
                .with_env_var("POSTGRES_DB", "cherub_test")
                .with_env_var("POSTGRES_USER", "cherub")
                .with_env_var("POSTGRES_PASSWORD", "cherub_test")
                .with_container_name(&container_name)
                .with_reuse(ReuseDirective::Always);

            let container = match image.start().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to start container (attempt {container_attempt}/3): {e}");
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    continue;
                }
            };

            let port = match container.get_host_port_ipv4(5432).await {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Failed to get container port (attempt {container_attempt}/3): {e}");
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    continue;
                }
            };

            let db_url = format!(
                "postgres://cherub:cherub_test@127.0.0.1:{}/cherub_test",
                port
            );

            // Inner retry: 5 connection attempts with exponential backoff.
            // connect() runs refinery migrations (idempotent on reused containers).
            match Self::connect_with_retry(&db_url).await {
                Ok(pool) => {
                    // TRUNCATE all data tables so each test starts with a clean slate.
                    // Safe because nextest serializes tests within a slot.
                    let conn = pool.get().await.expect("get connection for truncate");
                    conn.batch_execute(RESET_SQL)
                        .await
                        .expect("truncate tables");
                    return Self {
                        _container: container,
                        pool,
                    };
                }
                Err(e) => {
                    eprintln!("Database connection failed (attempt {container_attempt}/3): {e}");
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    // continue outer loop to restart container
                }
            }
        }

        panic!("Failed to set up test database after 3 container attempts");
    }

    /// Attempt to connect up to 5 times with 1s/2s/4s/8s exponential backoff.
    async fn connect_with_retry(db_url: &str) -> Result<Pool, String> {
        for attempt in 1u32..=5 {
            match cherub::storage::connect(SecretString::from(db_url.to_owned())).await {
                Ok(pool) => return Ok(pool),
                Err(e) => {
                    if attempt == 5 {
                        return Err(format!("connection failed after 5 attempts: {e}"));
                    }
                    let delay = 1u64 << (attempt - 1); // 1s, 2s, 4s, 8s
                    tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
                }
            }
        }
        unreachable!()
    }
}

// ─── Mock embedding provider (M6c) ───────────────────────────────────────────

/// Deterministic mock `EmbeddingProvider` for integration tests.
///
/// Produces 1536-dimensional vectors from a simple hash of the input text.
/// Two strings with identical content produce identical vectors; distinct strings
/// produce distinct (but not semantically meaningful) vectors.
#[cfg(feature = "memory")]
pub struct MockEmbeddingProvider {
    /// When `true`, `embed()` always returns an error.
    pub fail: bool,
}

#[cfg(feature = "memory")]
impl MockEmbeddingProvider {
    pub fn new() -> Self {
        Self { fail: false }
    }

    pub fn failing() -> Self {
        Self { fail: true }
    }
}

#[cfg(feature = "memory")]
#[async_trait]
impl EmbeddingProvider for MockEmbeddingProvider {
    fn dimension(&self) -> usize {
        1536
    }

    fn model_name(&self) -> &str {
        "mock-embedding"
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, CherubError> {
        if self.fail {
            return Err(CherubError::Provider("mock embedding failure".into()));
        }
        // Deterministic hash-based vector: same text → same vector.
        let seed = text.bytes().fold(0u64, |acc, b| {
            acc.wrapping_mul(6364136223846793005).wrapping_add(b as u64)
        });
        let mut vec = Vec::with_capacity(1536);
        let mut state = seed;
        for _ in 0..1536 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // Map to [-1, 1] range.
            let val = ((state >> 33) as f32) / (u32::MAX as f32) * 2.0 - 1.0;
            vec.push(val);
        }
        // Normalize to unit length.
        let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            vec.iter_mut().for_each(|v| *v /= norm);
        }
        Ok(vec)
    }
}
