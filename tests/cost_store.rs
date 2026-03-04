//! Integration tests for PgCostStore (M12).
//!
//! PostgreSQL starts automatically via testcontainers — no manual setup needed.
//!
//!   cargo nextest run --features sessions --test cost_store

#![cfg(feature = "sessions")]

mod fixtures;

use cherub::storage::pg_cost_store::PgCostStore;
use cherub::storage::pg_session_store::PgSessionStore;
use cherub::storage::{CallType, CostStore, NewTokenUsage, SessionStore};
use uuid::Uuid;

/// Helper: create a real session in the DB and return its ID.
async fn create_session(pool: &deadpool_postgres::Pool) -> Uuid {
    let store = PgSessionStore::new(pool.clone());
    let connector_id = format!("cost-test-{}", Uuid::now_v7());
    let (session_id, _) = store
        .get_or_create_session("test", &connector_id)
        .await
        .expect("create test session");
    session_id
}

fn make_usage(session_id: Option<Uuid>, user_id: &str, cost_usd: f64) -> NewTokenUsage {
    NewTokenUsage {
        session_id,
        user_id: user_id.to_owned(),
        turn_number: Some(1),
        model_name: "claude-sonnet-4-20250514".to_owned(),
        input_tokens: 1000,
        output_tokens: 200,
        cost_usd,
        call_type: CallType::Inference,
    }
}

#[tokio::test]
async fn record_returns_uuid() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgCostStore::new(tc.pool.clone());

    let id = store
        .record(make_usage(None, "test-user", 0.01))
        .await
        .expect("record");

    assert_eq!(id.get_version_num(), 7, "should be UUID v7");
}

#[tokio::test]
async fn session_cost_aggregates() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgCostStore::new(tc.pool.clone());
    let session_id = create_session(&tc.pool).await;

    // Record three entries for the same session.
    for i in 0..3 {
        store
            .record(NewTokenUsage {
                session_id: Some(session_id),
                user_id: "test-user".to_owned(),
                turn_number: Some(i),
                model_name: "claude-sonnet-4-20250514".to_owned(),
                input_tokens: 1000,
                output_tokens: 200,
                cost_usd: 0.01,
                call_type: CallType::Inference,
            })
            .await
            .expect("record");
    }

    let summary = store.session_cost(session_id).await.expect("session_cost");

    assert_eq!(summary.call_count, 3);
    assert!((summary.total_cost_usd - 0.03).abs() < 1e-9);
    assert_eq!(summary.total_input_tokens, 3000);
    assert_eq!(summary.total_output_tokens, 600);
}

#[tokio::test]
async fn session_cost_empty_session() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgCostStore::new(tc.pool.clone());

    let summary = store
        .session_cost(Uuid::now_v7())
        .await
        .expect("session_cost for empty session");

    assert_eq!(summary.call_count, 0);
    assert!((summary.total_cost_usd).abs() < 1e-9);
    assert_eq!(summary.total_input_tokens, 0);
    assert_eq!(summary.total_output_tokens, 0);
}

#[tokio::test]
async fn period_cost_filters_by_user_and_time() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgCostStore::new(tc.pool.clone());

    let user = format!("user-{}", Uuid::now_v7());
    let other_user = format!("other-{}", Uuid::now_v7());

    // Record for target user.
    store
        .record(make_usage(None, &user, 0.05))
        .await
        .expect("record user");

    // Record for a different user.
    store
        .record(make_usage(None, &other_user, 0.99))
        .await
        .expect("record other");

    // Query period cost for target user since an hour ago.
    let since = chrono::Utc::now() - chrono::Duration::hours(1);
    let summary = store.period_cost(&user, since).await.expect("period_cost");

    assert_eq!(summary.call_count, 1);
    assert!((summary.total_cost_usd - 0.05).abs() < 1e-9);
}

#[tokio::test]
async fn daily_costs_returns_today() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgCostStore::new(tc.pool.clone());

    let user = format!("daily-{}", Uuid::now_v7());

    store
        .record(make_usage(None, &user, 0.10))
        .await
        .expect("record");
    store
        .record(make_usage(None, &user, 0.20))
        .await
        .expect("record");

    let days = store.daily_costs(&user, 7).await.expect("daily_costs");

    assert_eq!(days.len(), 1, "should have one day entry");
    assert!((days[0].total_cost_usd - 0.30).abs() < 1e-9);
    assert_eq!(days[0].call_count, 2);
    assert_eq!(days[0].total_input_tokens, 2000);
    assert_eq!(days[0].total_output_tokens, 400);
}

#[tokio::test]
async fn different_call_types_recorded() {
    let tc = fixtures::TestContainer::new().await;
    let store = PgCostStore::new(tc.pool.clone());
    let session_id = create_session(&tc.pool).await;

    store
        .record(NewTokenUsage {
            session_id: Some(session_id),
            user_id: "test-user".to_owned(),
            turn_number: Some(1),
            model_name: "claude-sonnet-4-20250514".to_owned(),
            input_tokens: 5000,
            output_tokens: 1000,
            cost_usd: 0.05,
            call_type: CallType::Inference,
        })
        .await
        .expect("inference");

    store
        .record(NewTokenUsage {
            session_id: Some(session_id),
            user_id: "test-user".to_owned(),
            turn_number: None,
            model_name: "claude-sonnet-4-20250514".to_owned(),
            input_tokens: 3000,
            output_tokens: 500,
            cost_usd: 0.02,
            call_type: CallType::Summarization,
        })
        .await
        .expect("summarization");

    store
        .record(NewTokenUsage {
            session_id: Some(session_id),
            user_id: "test-user".to_owned(),
            turn_number: None,
            model_name: "claude-sonnet-4-20250514".to_owned(),
            input_tokens: 1000,
            output_tokens: 100,
            cost_usd: 0.005,
            call_type: CallType::Extraction,
        })
        .await
        .expect("extraction");

    let summary = store.session_cost(session_id).await.expect("session_cost");

    assert_eq!(summary.call_count, 3);
    assert!((summary.total_cost_usd - 0.075).abs() < 1e-9);
    assert_eq!(summary.total_input_tokens, 9000);
    assert_eq!(summary.total_output_tokens, 1600);
}
