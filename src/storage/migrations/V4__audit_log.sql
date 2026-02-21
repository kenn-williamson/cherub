-- M10: Audit log — append-only event stream.
--
-- Records every enforcement decision (allow, reject, escalate, approve, deny)
-- and execution outcome (success, error). The table is write-only from the
-- runtime perspective — rows are never updated or deleted.
--
-- decision column values:
--   allow      — enforcement passed automatically (Observe or Act tier)
--   reject     — enforcement denied (no match, tool disabled, etc.)
--   escalate   — Commit-tier action sent to approval gate
--   approve    — user approved an escalated action
--   deny       — user denied an escalated action

CREATE TABLE IF NOT EXISTS audit_events (
    id              UUID        PRIMARY KEY DEFAULT uuidv7(),
    -- Session the event belongs to. NULL for out-of-session operations
    -- (e.g. credential CLI commands).
    session_id      UUID,
    user_id         TEXT        NOT NULL,
    turn_number     INT,
    -- Which tool was invoked (e.g. "bash", "http", "memory").
    tool            TEXT        NOT NULL,
    -- The specific action string that was evaluated (e.g. "ls /tmp",
    -- "get:api.stripe.com"). NULL if extraction failed before we had one.
    action          TEXT,
    -- Enforcement outcome: allow | reject | escalate | approve | deny
    decision        TEXT        NOT NULL,
    -- Tier that granted or triggered the decision: observe | act | commit.
    -- NULL for reject (no tier reached).
    tier            TEXT,
    -- Wall-clock duration of the tool execution in milliseconds.
    -- NULL if the tool was rejected or denied before execution.
    duration_ms     BIGINT,
    -- Whether the executed tool returned an error result.
    -- NULL if not executed.
    is_error        BOOLEAN,
    -- Immutable timestamp. DEFAULT is authoritative; application must not set this.
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Indexes sized for operational queries: by session, by tool, by decision, by time.
CREATE INDEX IF NOT EXISTS audit_session_idx    ON audit_events (session_id) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS audit_user_idx       ON audit_events (user_id);
CREATE INDEX IF NOT EXISTS audit_tool_idx       ON audit_events (tool);
CREATE INDEX IF NOT EXISTS audit_decision_idx   ON audit_events (decision);
CREATE INDEX IF NOT EXISTS audit_time_idx       ON audit_events (created_at DESC);
