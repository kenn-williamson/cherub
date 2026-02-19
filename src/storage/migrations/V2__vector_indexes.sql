-- M6c: HNSW vector indexes for hybrid search.
-- These are deferred from V1 because indexes on empty tables have no benefit.
-- ef_construction=64, m=16: good balance of build cost vs. query recall.

CREATE INDEX IF NOT EXISTS memories_embedding_idx
    ON memories USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

CREATE INDEX IF NOT EXISTS memory_chunks_embedding_idx
    ON memory_chunks USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);
