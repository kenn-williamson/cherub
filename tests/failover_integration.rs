//! Integration tests for failover provider with wiremock.
//!
//! Uses wiremock to simulate two Anthropic API servers. Tests that failover
//! works correctly when the first server is down and the second is healthy,
//! when both are down, and that the circuit breaker activates after repeated failures.

use secrecy::SecretString;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cherub::providers::Provider;
use cherub::providers::anthropic::AnthropicProvider;
use cherub::providers::failover::FailoverProvider;

/// Valid mock 200 response body matching the Anthropic API wire format.
const MOCK_SUCCESS_BODY: &str = r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;

fn anthropic_provider(url: &str) -> Box<dyn Provider> {
    Box::new(
        AnthropicProvider::new(SecretString::from("test-key"), "claude-test", 1024)
            .unwrap()
            .with_url(url.to_owned()),
    )
}

#[tokio::test]
async fn first_down_second_succeeds() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    // Server 1: returns 400 (permanent error — no retries, fast failure).
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(400).set_body_string("down"))
        .expect(1)
        .mount(&server1)
        .await;

    // Server 2: returns 200.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOCK_SUCCESS_BODY))
        .expect(1)
        .mount(&server2)
        .await;

    let failover = FailoverProvider::new(
        vec![
            anthropic_provider(&server1.uri()),
            anthropic_provider(&server2.uri()),
        ],
        vec!["primary".to_owned(), "secondary".to_owned()],
    );

    let messages = vec![cherub::providers::Message::user_text("hello")];
    let result = failover.complete("system", &messages, &[]).await;
    assert!(result.is_ok(), "should succeed via fallback: {result:?}");
    assert_eq!(failover.model_name(), "claude-test");
}

#[tokio::test]
async fn both_servers_down() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    // Both return 400 (permanent error — no retries, fast failure).
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(400).set_body_string("down-1"))
        .expect(1)
        .mount(&server1)
        .await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(400).set_body_string("down-2"))
        .expect(1)
        .mount(&server2)
        .await;

    let failover = FailoverProvider::new(
        vec![
            anthropic_provider(&server1.uri()),
            anthropic_provider(&server2.uri()),
        ],
        vec!["primary".to_owned(), "secondary".to_owned()],
    );

    let messages = vec![cherub::providers::Message::user_text("hello")];
    let result = failover.complete("system", &messages, &[]).await;
    assert!(result.is_err(), "should fail when all providers are down");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("provider error"),
        "should be a provider error: {err}"
    );
}

#[tokio::test]
async fn circuit_breaker_activation() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    // Server 1: always returns 400 (permanent error — provider retries exhaust, returns Provider error).
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server1)
        .await;

    // Server 2: always succeeds.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOCK_SUCCESS_BODY))
        .mount(&server2)
        .await;

    let failover = FailoverProvider::new(
        vec![
            anthropic_provider(&server1.uri()),
            anthropic_provider(&server2.uri()),
        ],
        vec!["primary".to_owned(), "secondary".to_owned()],
    )
    .with_failure_threshold(2)
    .with_cooldown(std::time::Duration::from_secs(300));

    let messages = vec![cherub::providers::Message::user_text("hello")];

    // Call 1: server1 fails (1 failure), server2 succeeds.
    let r1 = failover.complete("system", &messages, &[]).await;
    assert!(r1.is_ok());

    // Call 2: server1 fails again (2 failures = threshold), circuit trips. server2 succeeds.
    let r2 = failover.complete("system", &messages, &[]).await;
    assert!(r2.is_ok());

    // Call 3: server1 is skipped (circuit open). server2 succeeds without trying server1.
    // We can verify by checking that server1 received exactly 2 requests (from calls 1 and 2),
    // not 3.
    let r3 = failover.complete("system", &messages, &[]).await;
    assert!(r3.is_ok());

    let s1_requests = server1.received_requests().await.unwrap();
    // Server 1 is called in call 1 and call 2.
    // 400 is a permanent error (no retries), so 1 request per failover attempt.
    // After circuit trips, call 3 skips server1 entirely.
    assert_eq!(
        s1_requests.len(),
        2,
        "server1 should receive requests from exactly 2 failover attempts"
    );
}
