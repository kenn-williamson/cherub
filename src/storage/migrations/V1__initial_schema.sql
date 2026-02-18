-- M6a: Initial schema — sessions + messages + memory tables (memories populated in M6b)
-- Targeting PostgreSQL 18 with pgvector extension.
-- All Rust-generated IDs use Uuid::now_v7() (time-sortable, better B-tree indexing).

CREATE EXTENSION IF NOT EXISTS vector;

-- ─── Sessions ────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS sessions (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    connector   TEXT        NOT NULL,
    -- 'cli' uses 'default'; Telegram uses the chat_id string.
    connector_id TEXT       NOT NULL DEFAULT 'default',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- One active session per connector channel.
    CONSTRAINT sessions_connector_channel_unique UNIQUE (connector, connector_id)
);

CREATE INDEX IF NOT EXISTS sessions_connector_idx ON sessions (connector, connector_id);

-- ─── Session Messages ─────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS session_messages (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id  UUID        NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal     INTEGER     NOT NULL,
    -- Full message serialized as JSONB (PostgreSQL TOAST handles large values like images).
    message_json JSONB      NOT NULL,
    -- Denormalized for fast filtering without JSON parsing.
    role        TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT session_messages_ordinal_unique UNIQUE (session_id, ordinal)
);

CREATE INDEX IF NOT EXISTS session_messages_session_idx ON session_messages (session_id, ordinal);

-- ─── Memories (schema defined now; populated in M6b) ─────────────────────────

CREATE TABLE IF NOT EXISTS memories (
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id             TEXT        NOT NULL,

    -- What
    category            TEXT        NOT NULL,   -- 'preference', 'fact', 'instruction', 'identity', 'observation'
    path                TEXT        NOT NULL,   -- Workspace-style: 'preferences/food', 'identity/values'
    content             TEXT        NOT NULL,   -- Natural language, human-readable
    structured          JSONB,                  -- Machine-queryable structured data

    -- Provenance
    source_session_id   UUID        REFERENCES sessions(id),
    source_turn_number  INTEGER,
    source_type         TEXT        NOT NULL,   -- 'explicit', 'confirmed', 'inferred'
    confidence          REAL        NOT NULL DEFAULT 1.0,

    -- Lifecycle
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_referenced_at  TIMESTAMPTZ,
    expires_at          TIMESTAMPTZ,
    superseded_by       UUID        REFERENCES memories(id),

    -- Search (embedding indexes deferred to M6c — empty tables don't benefit)
    embedding           VECTOR(1536),
    tsv                 TSVECTOR    GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,

    -- Enforcement
    tier                TEXT        NOT NULL DEFAULT 'act'
);

CREATE INDEX IF NOT EXISTS memories_user_id_idx ON memories (user_id);
CREATE INDEX IF NOT EXISTS memories_path_idx    ON memories (user_id, path);
CREATE INDEX IF NOT EXISTS memories_tsv_idx     ON memories USING GIN (tsv);

-- ─── Memory Chunks (for long-document chunked search; populated in M6b) ──────

CREATE TABLE IF NOT EXISTS memory_chunks (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    memory_id   UUID        NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    chunk_index INTEGER     NOT NULL,
    content     TEXT        NOT NULL,
    -- Vector indexes deferred to M6c
    embedding   VECTOR(1536),
    tsv         TSVECTOR    GENERATED ALWAYS AS (to_tsvector('english', content)) STORED
);

CREATE INDEX IF NOT EXISTS memory_chunks_memory_idx ON memory_chunks (memory_id, chunk_index);
CREATE INDEX IF NOT EXISTS memory_chunks_tsv_idx    ON memory_chunks USING GIN (tsv);
