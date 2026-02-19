//! Live integration tests for the OpenAI embedding provider (M6c).
//!
//! These tests are `#[ignore]` — they require a real `OPENAI_API_KEY` and make
//! outbound HTTP requests. Run them explicitly:
//!
//!   OPENAI_API_KEY=sk-... cargo test --features memory embedding_live -- --ignored

#![cfg(feature = "memory")]

use std::sync::Arc;

use cherub::storage::embedding::{EmbeddingProvider, OpenAiEmbeddingProvider};
use secrecy::SecretString;

fn provider() -> Arc<OpenAiEmbeddingProvider> {
    let key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set to run live embedding tests");
    Arc::new(OpenAiEmbeddingProvider::new(SecretString::from(key)).expect("create provider"))
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY and outbound HTTP"]
async fn embed_returns_1536_dimensions() {
    let p = provider();
    let vec = p.embed("hello world").await.expect("embed");
    assert_eq!(
        vec.len(),
        1536,
        "text-embedding-3-small should produce 1536-dim vectors"
    );
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY and outbound HTTP"]
async fn embed_batch_returns_same_count() {
    let p = provider();
    let texts = vec![
        "the quick brown fox".to_owned(),
        "jumps over the lazy dog".to_owned(),
        "Rust is a systems programming language".to_owned(),
    ];
    let vecs = p.embed_batch(&texts).await.expect("embed_batch");
    assert_eq!(vecs.len(), 3);
    for v in &vecs {
        assert_eq!(v.len(), 1536);
    }
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY and outbound HTTP"]
async fn identical_text_produces_identical_vector() {
    let p = provider();
    let v1 = p.embed("semantic search is useful").await.expect("embed 1");
    let v2 = p.embed("semantic search is useful").await.expect("embed 2");
    assert_eq!(v1, v2, "identical text should produce identical embedding");
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY and outbound HTTP"]
async fn vector_is_unit_normalized() {
    let p = provider();
    let v = p.embed("normalization test").await.expect("embed");
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-3,
        "OpenAI embeddings should be approximately unit-normalized, got norm={norm}"
    );
}
