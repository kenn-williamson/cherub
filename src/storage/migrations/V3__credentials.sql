-- M7a: Credential vault — encrypted credential storage.
-- AES-256-GCM per-secret encryption; master key lives outside the DB.

CREATE TABLE IF NOT EXISTS credentials (
    id              UUID        PRIMARY KEY DEFAULT uuidv7(),
    user_id         TEXT        NOT NULL,
    name            TEXT        NOT NULL,
    -- Stored as nonce || ciphertext || tag (AES-256-GCM, 12-byte nonce + 16-byte tag).
    encrypted_value BYTEA       NOT NULL,
    -- Per-secret HKDF salt (32 bytes, random). Different salts → different ciphertexts
    -- even for identical values, preventing corpus attacks.
    key_salt        BYTEA       NOT NULL,
    -- Informational: which provider this credential is for (e.g. "stripe", "openai").
    provider        TEXT,
    -- Capability scope: ["read"], ["write"], ["read", "write"], ["delete"], etc.
    -- Broker validates the HTTP method against this list before injection.
    capabilities    TEXT[]      NOT NULL DEFAULT '{}',
    -- Glob-style host patterns: ["api.stripe.com", "*.stripe.com"].
    -- Empty list means any host is allowed (not recommended for production).
    host_patterns   TEXT[]      NOT NULL DEFAULT '{}',
    -- Where to inject the credential into the HTTP request.
    -- JSON: "AuthorizationBearer" | {"Header":{"name":"X-Api-Key","prefix":null}} | {"QueryParam":{"name":"api_key"}}
    location        JSONB       NOT NULL DEFAULT '"AuthorizationBearer"',
    expires_at      TIMESTAMPTZ,
    last_used_at    TIMESTAMPTZ,
    usage_count     BIGINT      NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- One credential per (user, name) pair. Store updates overwrite the previous value.
    UNIQUE (user_id, name)
);

CREATE INDEX IF NOT EXISTS credentials_user_idx ON credentials (user_id);
CREATE INDEX IF NOT EXISTS credentials_lookup_idx ON credentials (user_id, name);
