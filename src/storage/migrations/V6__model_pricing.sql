CREATE TABLE IF NOT EXISTS model_pricing (
    model_pattern        TEXT             PRIMARY KEY,
    input_per_mtok       DOUBLE PRECISION NOT NULL,
    output_per_mtok      DOUBLE PRECISION NOT NULL,
    cache_write_per_mtok DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    cache_read_per_mtok  DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    updated_at           TIMESTAMPTZ      NOT NULL DEFAULT now()
);
