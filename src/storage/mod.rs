pub mod embedding;
pub mod pg_audit_store;
pub mod pg_cost_store;
pub mod pg_memory_store;
pub mod pg_session_store;
pub mod search;

#[cfg(feature = "credentials")]
pub mod credential_types;
#[cfg(feature = "credentials")]
pub(crate) mod crypto;
#[cfg(feature = "credentials")]
pub mod pg_credential_store;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
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
/// - `EmbeddingProvider` for vector search (M6c) — in `embedding` module
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

    /// Replace all messages for a session atomically (used after compaction).
    ///
    /// Deletes all existing messages and inserts the new set in a single transaction.
    /// Called by `Session::persist_compacted()` after compaction rewrites history.
    async fn replace_messages(
        &self,
        session_id: Uuid,
        messages: &[Message],
    ) -> Result<(), CherubError>;
}

// ─── Memory types (M6b) ───────────────────────────────────────────────────────

/// Which scope a memory belongs to.
///
/// - `Agent`: shared across all users; requires Commit tier to modify
/// - `User`: per-user memories; requires Act tier to modify
/// - `Working`: continuity context for the current session; requires Observe tier
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Agent,
    User,
    Working,
}

impl MemoryScope {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryScope::Agent => "agent",
            MemoryScope::User => "user",
            MemoryScope::Working => "working",
        }
    }
}

impl std::fmt::Display for MemoryScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MemoryScope {
    type Err = CherubError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "agent" => Ok(MemoryScope::Agent),
            "user" => Ok(MemoryScope::User),
            "working" => Ok(MemoryScope::Working),
            other => Err(CherubError::Storage(format!(
                "unknown memory scope: {other}"
            ))),
        }
    }
}

/// What kind of information a memory represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    Preference,
    Fact,
    Instruction,
    Identity,
    Observation,
}

impl MemoryCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryCategory::Preference => "preference",
            MemoryCategory::Fact => "fact",
            MemoryCategory::Instruction => "instruction",
            MemoryCategory::Identity => "identity",
            MemoryCategory::Observation => "observation",
        }
    }
}

impl std::str::FromStr for MemoryCategory {
    type Err = CherubError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "preference" => Ok(MemoryCategory::Preference),
            "fact" => Ok(MemoryCategory::Fact),
            "instruction" => Ok(MemoryCategory::Instruction),
            "identity" => Ok(MemoryCategory::Identity),
            "observation" => Ok(MemoryCategory::Observation),
            other => Err(CherubError::Storage(format!(
                "unknown memory category: {other}"
            ))),
        }
    }
}

/// How the memory was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    /// User stated it explicitly.
    Explicit,
    /// User confirmed an agent observation.
    Confirmed,
    /// Agent inferred it from behavior.
    Inferred,
}

impl SourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceType::Explicit => "explicit",
            SourceType::Confirmed => "confirmed",
            SourceType::Inferred => "inferred",
        }
    }
}

impl std::str::FromStr for SourceType {
    type Err = CherubError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "explicit" => Ok(SourceType::Explicit),
            "confirmed" => Ok(SourceType::Confirmed),
            "inferred" => Ok(SourceType::Inferred),
            other => Err(CherubError::Storage(format!(
                "unknown source type: {other}"
            ))),
        }
    }
}

/// A fully-loaded memory row from the database.
#[derive(Debug, Clone)]
pub struct Memory {
    pub id: Uuid,
    pub user_id: String,
    pub scope: MemoryScope,
    pub category: MemoryCategory,
    pub path: String,
    pub content: String,
    pub structured: Option<serde_json::Value>,
    pub source_session_id: Option<Uuid>,
    pub source_turn_number: Option<i32>,
    pub source_type: SourceType,
    pub confidence: f32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_referenced_at: Option<DateTime<Utc>>,
    pub superseded_by: Option<Uuid>,
}

/// Input for creating a new memory. ID and timestamps are DB-generated.
#[derive(Debug)]
pub struct NewMemory {
    pub user_id: String,
    pub scope: MemoryScope,
    pub category: MemoryCategory,
    pub path: String,
    pub content: String,
    pub structured: Option<serde_json::Value>,
    pub source_session_id: Option<Uuid>,
    pub source_turn_number: Option<i32>,
    pub source_type: SourceType,
    pub confidence: f32,
}

/// Filter parameters for `MemoryStore::recall()`.
#[derive(Debug, Default)]
pub struct MemoryFilter {
    pub scope: Option<MemoryScope>,
    pub category: Option<MemoryCategory>,
    /// Path prefix (LIKE 'prefix%').
    pub path_prefix: Option<String>,
    pub user_id: Option<String>,
    pub limit: Option<i64>,
}

/// Fields that can be changed on an existing memory via `MemoryStore::update()`.
/// Creates a new memory row and sets `superseded_by` on the old one.
#[derive(Debug)]
pub struct MemoryUpdate {
    pub content: Option<String>,
    pub structured: Option<serde_json::Value>,
    pub confidence: Option<f32>,
}

/// Storage backend for the memory tool.
///
/// Implementations: `PgMemoryStore` (PostgreSQL, M6b), in-memory (tests).
/// This is a true `dyn Trait` boundary — backend selected at runtime.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Create a new memory. Returns the DB-assigned ID.
    async fn store(&self, memory: NewMemory) -> Result<Uuid, CherubError>;

    /// Retrieve memories matching the filter. Active records only
    /// (superseded_by IS NULL).
    async fn recall(&self, filter: MemoryFilter) -> Result<Vec<Memory>, CherubError>;

    /// Full-text search over memory content.
    /// Returns up to `limit` memories ranked by relevance.
    async fn search(
        &self,
        query: &str,
        scope: Option<MemoryScope>,
        user_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Memory>, CherubError>;

    /// Update a memory by creating a new row and chaining `superseded_by`.
    /// Returns the new memory's ID.
    async fn update(&self, id: Uuid, changes: MemoryUpdate) -> Result<Uuid, CherubError>;

    /// Soft-delete: mark a memory as superseded (points to itself as sentinel).
    async fn forget(&self, id: Uuid) -> Result<(), CherubError>;

    /// Touch `last_referenced_at` for a recalled memory.
    async fn touch(&self, id: Uuid) -> Result<(), CherubError>;
}

// ─── Credential types + trait (M7a) ──────────────────────────────────────────

#[cfg(feature = "credentials")]
pub use credential_types::{
    Credential, CredentialLocation, CredentialRef, DecryptedCredential, NewCredential,
};

/// Storage backend for the credential vault (M7a).
///
/// Implementations: `PgCredentialStore` (PostgreSQL, M7a).
/// This is a true `dyn Trait` boundary — backend selected at runtime.
#[cfg(feature = "credentials")]
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// Store a new credential or update an existing one (upsert by name).
    /// The plaintext value in `NewCredential.value` is encrypted before storage.
    async fn store(&self, cred: NewCredential) -> Result<Uuid, CherubError>;

    /// Retrieve the encrypted credential row. Does not decrypt.
    async fn get(&self, user_id: &str, name: &str) -> Result<Credential, CherubError>;

    /// Retrieve the agent-safe credential reference (no encrypted bytes).
    async fn get_ref(&self, user_id: &str, name: &str) -> Result<CredentialRef, CherubError>;

    /// List all credentials for a user (returns `CredentialRef` — no encrypted bytes).
    async fn list(&self, user_id: &str) -> Result<Vec<CredentialRef>, CherubError>;

    /// Delete a credential. Returns `Credential` error if not found.
    async fn delete(&self, user_id: &str, name: &str) -> Result<(), CherubError>;

    /// Check whether a credential exists for the user.
    async fn exists(&self, user_id: &str, name: &str) -> Result<bool, CherubError>;

    /// Decrypt an encrypted credential row into an ephemeral `DecryptedCredential`.
    ///
    /// The returned handle has no Clone/Display — only `credential_broker.rs`
    /// calls `expose()` on it (the single broker injection point).
    async fn decrypt(&self, cred: &Credential) -> Result<DecryptedCredential, CherubError>;

    /// Update `last_used_at` and increment `usage_count` for a credential.
    /// Called after successful injection. Fire-and-forget; failures are logged, not fatal.
    async fn record_usage(&self, user_id: &str, name: &str) -> Result<(), CherubError>;

    /// Check whether a credential has passed its `expires_at` timestamp.
    async fn is_expired(&self, user_id: &str, name: &str) -> Result<bool, CherubError>;
}

// ─── Audit log types (M10) ────────────────────────────────────────────────────

/// The outcome of an enforcement evaluation for a single tool invocation.
///
/// Stored as a lowercase text string in `audit_events.decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    /// Enforcement passed automatically (Observe or Act tier).
    Allow,
    /// Enforcement denied (no match, tool disabled, constraint failure, etc.).
    Reject,
    /// Commit-tier action — forwarded to the approval gate.
    Escalate,
    /// User approved an escalated action.
    Approve,
    /// User denied an escalated action.
    Deny,
}

impl AuditDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditDecision::Allow => "allow",
            AuditDecision::Reject => "reject",
            AuditDecision::Escalate => "escalate",
            AuditDecision::Approve => "approve",
            AuditDecision::Deny => "deny",
        }
    }
}

impl std::fmt::Display for AuditDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AuditDecision {
    type Err = CherubError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allow" => Ok(AuditDecision::Allow),
            "reject" => Ok(AuditDecision::Reject),
            "escalate" => Ok(AuditDecision::Escalate),
            "approve" => Ok(AuditDecision::Approve),
            "deny" => Ok(AuditDecision::Deny),
            other => Err(CherubError::Storage(format!(
                "unknown audit decision: {other}"
            ))),
        }
    }
}

/// Input for a new audit event. `id` and `created_at` are DB-generated.
#[derive(Debug)]
pub struct NewAuditEvent {
    pub session_id: Option<Uuid>,
    pub user_id: String,
    pub turn_number: Option<i32>,
    /// Tool name (e.g. "bash", "http", "memory").
    pub tool: String,
    /// Action string that was evaluated (e.g. "ls /tmp", "get:api.stripe.com").
    pub action: Option<String>,
    pub decision: AuditDecision,
    /// Tier string: "observe", "act", or "commit". None for reject.
    pub tier: Option<String>,
    /// Execution duration in milliseconds. None if not executed.
    pub duration_ms: Option<i64>,
    /// Whether the executed tool returned an error. None if not executed.
    pub is_error: Option<bool>,
}

/// A fully-loaded audit event row.
#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub id: Uuid,
    pub session_id: Option<Uuid>,
    pub user_id: String,
    pub turn_number: Option<i32>,
    pub tool: String,
    pub action: Option<String>,
    pub decision: AuditDecision,
    pub tier: Option<String>,
    pub duration_ms: Option<i64>,
    pub is_error: Option<bool>,
    pub created_at: DateTime<Utc>,
}

/// Filter for `AuditStore::list()`.
#[derive(Debug, Default)]
pub struct AuditFilter {
    pub tool: Option<String>,
    pub decision: Option<AuditDecision>,
    pub user_id: Option<String>,
    pub session_id: Option<Uuid>,
    pub since: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
}

/// Append-only event log for enforcement decisions and tool executions.
///
/// Implementations: `PgAuditStore` (PostgreSQL, M10).
/// This is a true `dyn Trait` boundary — backend selected at runtime.
#[async_trait]
pub trait AuditStore: Send + Sync {
    /// Append a new event. Returns the DB-assigned UUID.
    ///
    /// Failures are non-fatal from the caller's perspective — the runtime
    /// logs a warning and continues. The audit log must not prevent tool execution.
    async fn append(&self, event: NewAuditEvent) -> Result<Uuid, CherubError>;

    /// Query the audit log with optional filters.
    ///
    /// Results are ordered by `created_at DESC` (most recent first).
    /// Defaults to the last 100 events if `filter.limit` is not set.
    async fn list(&self, filter: AuditFilter) -> Result<Vec<AuditEvent>, CherubError>;
}

// ─── Cost tracking types (M12) ────────────────────────────────────────────────

/// What kind of LLM API call was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallType {
    /// Main agent loop inference.
    Inference,
    /// Context compaction summarization.
    Summarization,
    /// Pre-compaction memory extraction.
    Extraction,
}

impl CallType {
    pub fn as_str(self) -> &'static str {
        match self {
            CallType::Inference => "inference",
            CallType::Summarization => "summarization",
            CallType::Extraction => "extraction",
        }
    }
}

/// Input for recording a token usage event.
#[derive(Debug)]
pub struct NewTokenUsage {
    pub session_id: Option<Uuid>,
    pub user_id: String,
    pub turn_number: Option<i32>,
    pub model_name: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cost_usd: f64,
    pub call_type: CallType,
}

/// Aggregate cost summary over a time period or session.
#[derive(Debug, Clone)]
pub struct CostSummary {
    pub total_cost_usd: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub call_count: i64,
}

/// Per-day cost breakdown for history display.
#[derive(Debug, Clone)]
pub struct DailyCost {
    pub date: chrono::NaiveDate,
    pub total_cost_usd: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub call_count: i64,
}

/// Storage backend for token usage and cost tracking (M12).
///
/// Implementations: `PgCostStore` (PostgreSQL, M12).
/// This is a true `dyn Trait` boundary — backend selected at runtime.
#[async_trait]
pub trait CostStore: Send + Sync {
    /// Record a token usage event. Returns the DB-assigned UUID.
    async fn record(&self, usage: NewTokenUsage) -> Result<Uuid, CherubError>;

    /// Get the aggregate cost for a session.
    async fn session_cost(&self, session_id: Uuid) -> Result<CostSummary, CherubError>;

    /// Get the aggregate cost for a user since a given timestamp.
    async fn period_cost(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
    ) -> Result<CostSummary, CherubError>;

    /// Get daily cost breakdown for a user over the last N days.
    async fn daily_costs(&self, user_id: &str, days: u32) -> Result<Vec<DailyCost>, CherubError>;
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
