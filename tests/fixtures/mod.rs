//! Shared test fixtures for PostgreSQL integration tests.
//!
//! `TestContainer` starts a reusable pgvector/pgvector:pg18 container (slot-named
//! for nextest parallelism), runs migrations via `cherub::storage::connect()`, and
//! TRUNCATEs all data tables so each test starts with a clean database.
//!
//! No manual `docker compose up` or `DATABASE_URL` env var needed.

use deadpool_postgres::Pool;
use secrecy::SecretString;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt, ReuseDirective,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

/// TRUNCATE all data tables between tests.
///
/// Reusable containers persist data across test runs; this clears it.
/// CASCADE handles FK constraints (memory_chunks → memories, session_messages → sessions).
const RESET_SQL: &str = "
    TRUNCATE TABLE memory_chunks, memories, session_messages, sessions CASCADE;
";

/// Holds a running PostgreSQL container and a connected pool.
///
/// Dropping this struct does NOT stop the container (ReuseDirective::Always) —
/// it stays alive for subsequent tests in the same nextest slot.
pub struct TestContainer {
    _container: ContainerAsync<GenericImage>,
    pub pool: Pool,
}

impl TestContainer {
    /// Start or reuse the test container, run migrations, and TRUNCATE all tables.
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
                    // TRUNCATE all data tables for clean test state.
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
