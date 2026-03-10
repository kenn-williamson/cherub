-- Consolidated initial schema: sessions, messages, memories, credentials,
-- audit log, cost tracking, and model pricing.

CREATE EXTENSION IF NOT EXISTS vector;

-- ─── Sessions ────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS sessions (
    id          UUID        PRIMARY KEY DEFAULT uuidv7(),
    connector   TEXT        NOT NULL,
    connector_id TEXT       NOT NULL DEFAULT 'default',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sessions_connector_channel_unique UNIQUE (connector, connector_id)
);

CREATE INDEX IF NOT EXISTS sessions_connector_idx ON sessions (connector, connector_id);

-- ─── Session Messages ─────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS session_messages (
    id          UUID        PRIMARY KEY DEFAULT uuidv7(),
    session_id  UUID        NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal     INTEGER     NOT NULL,
    message_json JSONB      NOT NULL,
    role        TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT session_messages_ordinal_unique UNIQUE (session_id, ordinal)
);

CREATE INDEX IF NOT EXISTS session_messages_session_idx ON session_messages (session_id, ordinal);

-- ─── Memories ─────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS memories (
    id                  UUID        PRIMARY KEY DEFAULT uuidv7(),
    user_id             TEXT        NOT NULL,
    scope               TEXT        NOT NULL DEFAULT 'user',
    category            TEXT        NOT NULL,
    path                TEXT        NOT NULL,
    content             TEXT        NOT NULL,
    structured          JSONB,
    source_session_id   UUID        REFERENCES sessions(id),
    source_turn_number  INTEGER,
    source_type         TEXT        NOT NULL,
    confidence          REAL        NOT NULL DEFAULT 1.0,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_referenced_at  TIMESTAMPTZ,
    expires_at          TIMESTAMPTZ,
    superseded_by       UUID        REFERENCES memories(id),
    embedding           VECTOR(1536),
    tsv                 TSVECTOR    GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
    tier                TEXT        NOT NULL DEFAULT 'act'
);

CREATE INDEX IF NOT EXISTS memories_user_id_idx  ON memories (user_id);
CREATE INDEX IF NOT EXISTS memories_scope_idx    ON memories (scope);
CREATE INDEX IF NOT EXISTS memories_path_idx     ON memories (user_id, path);
CREATE INDEX IF NOT EXISTS memories_tsv_idx      ON memories USING GIN (tsv);

-- ─── Memory Chunks ────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS memory_chunks (
    id          UUID        PRIMARY KEY DEFAULT uuidv7(),
    memory_id   UUID        NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    chunk_index INTEGER     NOT NULL,
    content     TEXT        NOT NULL,
    embedding   VECTOR(1536),
    tsv         TSVECTOR    GENERATED ALWAYS AS (to_tsvector('english', content)) STORED
);

CREATE INDEX IF NOT EXISTS memory_chunks_memory_idx ON memory_chunks (memory_id, chunk_index);
CREATE INDEX IF NOT EXISTS memory_chunks_tsv_idx    ON memory_chunks USING GIN (tsv);

-- ─── HNSW vector indexes ──────────────────────────────────────────────────────

CREATE INDEX IF NOT EXISTS memories_embedding_idx
    ON memories USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

CREATE INDEX IF NOT EXISTS memory_chunks_embedding_idx
    ON memory_chunks USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

-- ─── Credentials ──────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS credentials (
    id              UUID        PRIMARY KEY DEFAULT uuidv7(),
    user_id         TEXT        NOT NULL,
    name            TEXT        NOT NULL,
    encrypted_value BYTEA       NOT NULL,
    key_salt        BYTEA       NOT NULL,
    provider        TEXT,
    capabilities    TEXT[]      NOT NULL DEFAULT '{}',
    host_patterns   TEXT[]      NOT NULL DEFAULT '{}',
    location        JSONB       NOT NULL DEFAULT '"AuthorizationBearer"',
    expires_at      TIMESTAMPTZ,
    last_used_at    TIMESTAMPTZ,
    usage_count     BIGINT      NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, name)
);

CREATE INDEX IF NOT EXISTS credentials_user_idx ON credentials (user_id);
CREATE INDEX IF NOT EXISTS credentials_lookup_idx ON credentials (user_id, name);

-- ─── Audit Events ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS audit_events (
    id              UUID        PRIMARY KEY DEFAULT uuidv7(),
    session_id      UUID,
    user_id         TEXT        NOT NULL,
    turn_number     INT,
    tool            TEXT        NOT NULL,
    action          TEXT,
    decision        TEXT        NOT NULL,
    tier            TEXT,
    duration_ms     BIGINT,
    is_error        BOOLEAN,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS audit_session_idx    ON audit_events (session_id) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS audit_user_idx       ON audit_events (user_id);
CREATE INDEX IF NOT EXISTS audit_tool_idx       ON audit_events (tool);
CREATE INDEX IF NOT EXISTS audit_decision_idx   ON audit_events (decision);
CREATE INDEX IF NOT EXISTS audit_time_idx       ON audit_events (created_at DESC);

-- ─── Token Usage ──────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS token_usage (
    id              UUID        PRIMARY KEY DEFAULT uuidv7(),
    session_id      UUID,
    user_id         TEXT        NOT NULL,
    turn_number     INT,
    model_name      TEXT        NOT NULL,
    input_tokens    INT         NOT NULL,
    output_tokens   INT         NOT NULL,
    cost_usd        DOUBLE PRECISION NOT NULL,
    call_type       TEXT        NOT NULL DEFAULT 'inference',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT fk_session FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS token_usage_user_time_idx ON token_usage (user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS token_usage_session_idx ON token_usage (session_id) WHERE session_id IS NOT NULL;

-- ─── Model Pricing ────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS model_pricing (
    model_pattern        TEXT             PRIMARY KEY,
    input_per_mtok       DOUBLE PRECISION NOT NULL,
    output_per_mtok      DOUBLE PRECISION NOT NULL,
    cache_write_per_mtok DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    cache_read_per_mtok  DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    updated_at           TIMESTAMPTZ      NOT NULL DEFAULT now()
);
