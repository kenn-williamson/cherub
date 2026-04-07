-- Task queue: persists autonomous agent work items across wake cycles.
--
-- Each row is a single tool invocation that needed human approval during
-- an autonomous (cron) turn. The agent queues it here instead of blocking,
-- notifies the user via Telegram, and execution resumes once approved.
--
-- Status lifecycle:
--   awaiting_approval → approved → done (happy path)
--   awaiting_approval → rejected     (user denied)
--   approved → failed                (execution error)

CREATE TABLE IF NOT EXISTS task_queue (
    id               UUID        PRIMARY KEY DEFAULT uuidv7(),
    user_id          TEXT        NOT NULL,
    session_id       UUID        REFERENCES sessions(id) ON DELETE SET NULL,
    -- 'awaiting_approval' | 'approved' | 'rejected' | 'running' | 'done' | 'failed'
    status           TEXT        NOT NULL DEFAULT 'awaiting_approval',
    tool             TEXT        NOT NULL,
    action           TEXT,
    params           JSONB       NOT NULL DEFAULT '{}',
    tier             TEXT        NOT NULL,
    -- Human-readable summary for the Telegram notification.
    description      TEXT        NOT NULL,
    result_output    TEXT,
    error_message    TEXT,
    -- Telegram message ID of the sent approval notification (for reference).
    tg_message_id    TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    approved_at      TIMESTAMPTZ,
    executed_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS task_queue_user_status_idx  ON task_queue (user_id, status);
CREATE INDEX IF NOT EXISTS task_queue_user_created_idx ON task_queue (user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS task_queue_session_idx      ON task_queue (session_id) WHERE session_id IS NOT NULL;
