-- M12: Cost tracking — append-only token usage log.
--
-- Records every LLM API call with token counts and computed cost.
-- The table is write-only from the runtime perspective — rows are never
-- updated or deleted. Running totals are computed via SUM() queries.
--
-- call_type column values:
--   inference     — main agent loop inference call
--   summarization — context compaction summarization
--   extraction    — pre-compaction memory extraction

CREATE TABLE IF NOT EXISTS token_usage (
    id              UUID        PRIMARY KEY DEFAULT uuidv7(),
    -- Session the call belongs to. NULL for out-of-session operations.
    session_id      UUID,
    user_id         TEXT        NOT NULL,
    turn_number     INT,
    -- Model identifier (e.g. "claude-sonnet-4-20250514").
    model_name      TEXT        NOT NULL,
    input_tokens    INT         NOT NULL,
    output_tokens   INT         NOT NULL,
    -- Computed cost in USD based on model pricing at time of call.
    cost_usd        DOUBLE PRECISION NOT NULL,
    -- What kind of LLM call this was.
    call_type       TEXT        NOT NULL DEFAULT 'inference',
    -- Immutable timestamp. DEFAULT is authoritative; application must not set this.
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT fk_session FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

-- Primary query pattern: sum costs per user over a time range.
CREATE INDEX IF NOT EXISTS token_usage_user_time_idx ON token_usage (user_id, created_at DESC);
-- Secondary: per-session cost lookups.
CREATE INDEX IF NOT EXISTS token_usage_session_idx ON token_usage (session_id) WHERE session_id IS NOT NULL;
