//! Integration tests for PgMemoryStore (M6b + M6c).
//!
//! PostgreSQL starts automatically via testcontainers — no manual setup needed.
//!
//!   cargo test --features memory memory_store

#![cfg(feature = "memory")]

mod fixtures;

use std::sync::Arc;

use cherub::storage::pg_memory_store::PgMemoryStore;
use cherub::storage::{
    MemoryCategory, MemoryFilter, MemoryScope, MemoryStore, MemoryUpdate, NewMemory, SourceType,
};
use uuid::Uuid;

fn new_memory(user_id: &str, path: &str, content: &str) -> NewMemory {
    NewMemory {
        user_id: user_id.to_owned(),
        scope: MemoryScope::User,
        category: MemoryCategory::Preference,
        path: path.to_owned(),
        content: content.to_owned(),
        structured: None,
        source_session_id: None,
        source_turn_number: None,
        source_type: SourceType::Explicit,
        confidence: 1.0,
    }
}

#[tokio::test]
async fn store_and_recall() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgMemoryStore::new(tc.pool.clone());

    let user_id = format!("test-{}", Uuid::now_v7());
    let id = store
        .store(new_memory(
            &user_id,
            "preferences/food",
            "I prefer spicy food",
        ))
        .await
        .expect("store");

    assert_eq!(id.get_version_num(), 7, "should be UUID v7");

    let memories = store
        .recall(MemoryFilter {
            user_id: Some(user_id.clone()),
            ..Default::default()
        })
        .await
        .expect("recall");

    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].id, id);
    assert_eq!(memories[0].content, "I prefer spicy food");
    assert_eq!(memories[0].path, "preferences/food");
    assert_eq!(memories[0].scope, MemoryScope::User);
}

#[tokio::test]
async fn scope_filter() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgMemoryStore::new(tc.pool.clone());

    let user_id = format!("test-scope-{}", Uuid::now_v7());

    // Store memories in different scopes.
    store
        .store(NewMemory {
            user_id: user_id.clone(),
            scope: MemoryScope::User,
            category: MemoryCategory::Preference,
            path: "preferences/ui".to_owned(),
            content: "dark mode preferred".to_owned(),
            structured: None,
            source_session_id: None,
            source_turn_number: None,
            source_type: SourceType::Explicit,
            confidence: 1.0,
        })
        .await
        .expect("store user scope");

    store
        .store(NewMemory {
            user_id: user_id.clone(),
            scope: MemoryScope::Working,
            category: MemoryCategory::Observation,
            path: "working/task".to_owned(),
            content: "working on M6b".to_owned(),
            structured: None,
            source_session_id: None,
            source_turn_number: None,
            source_type: SourceType::Inferred,
            confidence: 0.8,
        })
        .await
        .expect("store working scope");

    // Filter by user scope only.
    let user_memories = store
        .recall(MemoryFilter {
            scope: Some(MemoryScope::User),
            user_id: Some(user_id.clone()),
            ..Default::default()
        })
        .await
        .expect("recall user scope");
    assert_eq!(user_memories.len(), 1);
    assert_eq!(user_memories[0].scope, MemoryScope::User);

    // Filter by working scope only.
    let working_memories = store
        .recall(MemoryFilter {
            scope: Some(MemoryScope::Working),
            user_id: Some(user_id.clone()),
            ..Default::default()
        })
        .await
        .expect("recall working scope");
    assert_eq!(working_memories.len(), 1);
    assert_eq!(working_memories[0].scope, MemoryScope::Working);
}

#[tokio::test]
async fn path_prefix_filter() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgMemoryStore::new(tc.pool.clone());

    let user_id = format!("test-path-{}", Uuid::now_v7());

    store
        .store(new_memory(&user_id, "preferences/food", "spicy"))
        .await
        .expect("store");
    store
        .store(new_memory(&user_id, "preferences/music", "jazz"))
        .await
        .expect("store");
    store
        .store(new_memory(&user_id, "identity/name", "Alice"))
        .await
        .expect("store");

    let prefs = store
        .recall(MemoryFilter {
            path_prefix: Some("preferences/".to_owned()),
            user_id: Some(user_id.clone()),
            ..Default::default()
        })
        .await
        .expect("recall");
    assert_eq!(prefs.len(), 2);
    for m in &prefs {
        assert!(m.path.starts_with("preferences/"), "path: {}", m.path);
    }
}

#[tokio::test]
async fn full_text_search() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgMemoryStore::new(tc.pool.clone());

    let user_id = format!("test-search-{}", Uuid::now_v7());

    store
        .store(new_memory(
            &user_id,
            "preferences/food",
            "I love spicy Mexican food especially tacos",
        ))
        .await
        .expect("store");
    store
        .store(new_memory(
            &user_id,
            "preferences/music",
            "I enjoy jazz and classical piano music",
        ))
        .await
        .expect("store");

    let results = store
        .search("spicy food", None, Some(&user_id), 5)
        .await
        .expect("search");

    assert!(!results.is_empty(), "search should return results");
    assert!(
        results[0].content.contains("spicy"),
        "top result should be about spicy food"
    );
}

#[tokio::test]
async fn update_creates_superseded_by_chain() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgMemoryStore::new(tc.pool.clone());

    let user_id = format!("test-update-{}", Uuid::now_v7());
    let orig_id = store
        .store(new_memory(&user_id, "preferences/food", "I like mild food"))
        .await
        .expect("store");

    let new_id = store
        .update(
            orig_id,
            MemoryUpdate {
                content: Some("I like spicy food".to_owned()),
                structured: None,
                confidence: None,
            },
        )
        .await
        .expect("update");

    assert_ne!(orig_id, new_id, "update should produce a new ID");

    // The original should now be superseded.
    let active = store
        .recall(MemoryFilter {
            user_id: Some(user_id.clone()),
            ..Default::default()
        })
        .await
        .expect("recall");

    assert_eq!(active.len(), 1, "only the updated version should be active");
    assert_eq!(active[0].id, new_id);
    assert_eq!(active[0].content, "I like spicy food");
}

#[tokio::test]
async fn forget_soft_deletes() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgMemoryStore::new(tc.pool.clone());

    let user_id = format!("test-forget-{}", Uuid::now_v7());
    let id = store
        .store(new_memory(&user_id, "preferences/food", "I like pizza"))
        .await
        .expect("store");

    store.forget(id).await.expect("forget");

    let active = store
        .recall(MemoryFilter {
            user_id: Some(user_id.clone()),
            ..Default::default()
        })
        .await
        .expect("recall");

    assert!(
        active.is_empty(),
        "forgotten memory should not appear in recall"
    );
}

#[tokio::test]
async fn touch_updates_last_referenced_at() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgMemoryStore::new(tc.pool.clone());

    let user_id = format!("test-touch-{}", Uuid::now_v7());
    let id = store
        .store(new_memory(&user_id, "preferences/food", "I like sushi"))
        .await
        .expect("store");

    store.touch(id).await.expect("touch");

    let memories = store
        .recall(MemoryFilter {
            user_id: Some(user_id.clone()),
            ..Default::default()
        })
        .await
        .expect("recall");

    assert!(
        memories[0].last_referenced_at.is_some(),
        "touch should set last_referenced_at"
    );
}

// ─── M6c: Hybrid search integration tests ────────────────────────────────────

#[tokio::test]
async fn store_creates_embedding() {
    let tc = fixtures::TestContainer::new().await;
    let embedder = Arc::new(fixtures::MockEmbeddingProvider::new());
    let store = PgMemoryStore::with_embedder(tc.pool.clone(), embedder);

    let user_id = format!("test-embed-{}", Uuid::now_v7());
    let id = store
        .store(new_memory(&user_id, "preferences/lang", "I prefer Rust"))
        .await
        .expect("store");

    // Verify embedding was stored (column non-NULL).
    let conn = tc.pool.get().await.expect("conn");
    let row = conn
        .query_one(
            "SELECT embedding IS NOT NULL AS has_embedding FROM memories WHERE id = $1",
            &[&id],
        )
        .await
        .expect("query");
    let has_embedding: bool = row.get("has_embedding");
    assert!(
        has_embedding,
        "embedding column should be non-NULL when embedder is configured"
    );
}

#[tokio::test]
async fn store_without_embedder_leaves_null() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgMemoryStore::new(tc.pool.clone()); // no embedder

    let user_id = format!("test-no-embed-{}", Uuid::now_v7());
    let id = store
        .store(new_memory(&user_id, "preferences/lang", "I prefer Go"))
        .await
        .expect("store");

    let conn = tc.pool.get().await.expect("conn");
    let row = conn
        .query_one(
            "SELECT embedding IS NULL AS is_null FROM memories WHERE id = $1",
            &[&id],
        )
        .await
        .expect("query");
    let is_null: bool = row.get("is_null");
    assert!(
        is_null,
        "embedding should be NULL when no embedder is configured"
    );
}

#[tokio::test]
async fn hybrid_search_returns_results() {
    let tc = fixtures::TestContainer::new().await;
    let embedder = Arc::new(fixtures::MockEmbeddingProvider::new());
    let store = PgMemoryStore::with_embedder(tc.pool.clone(), embedder);

    let user_id = format!("test-hybrid-{}", Uuid::now_v7());
    store
        .store(new_memory(
            &user_id,
            "prefs/food",
            "I love spicy tacos and burritos",
        ))
        .await
        .expect("store");
    store
        .store(new_memory(
            &user_id,
            "prefs/music",
            "I enjoy jazz and blues guitar",
        ))
        .await
        .expect("store");

    let results = store
        .search("spicy food", None, Some(&user_id), 5)
        .await
        .expect("search");

    assert!(!results.is_empty(), "hybrid search should return results");
    // The food memory should appear (FTS match on "spicy").
    assert!(
        results.iter().any(|m| m.content.contains("spicy")),
        "food memory should be in results"
    );
}

#[tokio::test]
async fn hybrid_search_fts_fallback() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgMemoryStore::new(tc.pool.clone()); // no embedder → FTS-only

    let user_id = format!("test-fts-fallback-{}", Uuid::now_v7());
    store
        .store(new_memory(&user_id, "prefs/food", "I love spicy tacos"))
        .await
        .expect("store");

    let results = store
        .search("spicy", None, Some(&user_id), 5)
        .await
        .expect("search");

    assert!(
        !results.is_empty(),
        "FTS-only search should still work without embedder"
    );
    assert!(results[0].content.contains("spicy"));
}

#[tokio::test]
async fn update_re_embeds_content() {
    let tc = fixtures::TestContainer::new().await;
    let embedder = Arc::new(fixtures::MockEmbeddingProvider::new());
    let store = PgMemoryStore::with_embedder(tc.pool.clone(), embedder);

    let user_id = format!("test-reembed-{}", Uuid::now_v7());
    let orig_id = store
        .store(new_memory(&user_id, "prefs/food", "I like mild food"))
        .await
        .expect("store");

    let new_id = store
        .update(
            orig_id,
            MemoryUpdate {
                content: Some("I like spicy food now".to_owned()),
                structured: None,
                confidence: None,
            },
        )
        .await
        .expect("update");

    // Both rows should have non-NULL embeddings.
    let conn = tc.pool.get().await.expect("conn");
    let rows = conn
        .query(
            "SELECT id, embedding IS NOT NULL AS has_embedding FROM memories WHERE id = ANY($1)",
            &[&vec![orig_id, new_id]],
        )
        .await
        .expect("query");

    for row in &rows {
        let has_embedding: bool = row.get("has_embedding");
        assert!(
            has_embedding,
            "both old and new rows should have embeddings"
        );
    }
}

#[tokio::test]
async fn embedding_failure_does_not_block_store() {
    let tc = fixtures::TestContainer::new().await;
    let failing_embedder = Arc::new(fixtures::MockEmbeddingProvider::failing());
    let store = PgMemoryStore::with_embedder(tc.pool.clone(), failing_embedder);

    let user_id = format!("test-failembed-{}", Uuid::now_v7());
    // Store should succeed even though embedding fails.
    let id = store
        .store(new_memory(&user_id, "prefs/food", "I like pizza"))
        .await
        .expect("store should succeed even when embedding fails");

    let conn = tc.pool.get().await.expect("conn");
    let row = conn
        .query_one(
            "SELECT embedding IS NULL AS is_null FROM memories WHERE id = $1",
            &[&id],
        )
        .await
        .expect("query");
    let is_null: bool = row.get("is_null");
    assert!(
        is_null,
        "embedding should be NULL when embedding fails (graceful degradation)"
    );
}

#[tokio::test]
async fn user_isolation() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgMemoryStore::new(tc.pool.clone());

    let user_a = format!("user-a-{}", Uuid::now_v7());
    let user_b = format!("user-b-{}", Uuid::now_v7());

    store
        .store(new_memory(
            &user_a,
            "preferences/food",
            "user A likes sushi",
        ))
        .await
        .expect("store user A");
    store
        .store(new_memory(
            &user_b,
            "preferences/food",
            "user B likes pizza",
        ))
        .await
        .expect("store user B");

    let a_memories = store
        .recall(MemoryFilter {
            user_id: Some(user_a.clone()),
            ..Default::default()
        })
        .await
        .expect("recall user A");
    let b_memories = store
        .recall(MemoryFilter {
            user_id: Some(user_b.clone()),
            ..Default::default()
        })
        .await
        .expect("recall user B");

    assert_eq!(a_memories.len(), 1);
    assert_eq!(b_memories.len(), 1);
    assert!(a_memories[0].content.contains("sushi"));
    assert!(b_memories[0].content.contains("pizza"));
}
