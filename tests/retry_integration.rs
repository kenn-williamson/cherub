//! Integration tests for API retry with exponential backoff.
//!
//! Uses wiremock to simulate Anthropic API responses. Each test spins up a
//! `MockServer` and creates an `AnthropicProvider` pointed at it.

use secrecy::SecretString;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cherub::providers::Provider;
use cherub::providers::anthropic::AnthropicProvider;

/// Valid mock 200 response body that matches the Anthropic API wire format.
const MOCK_SUCCESS_BODY: &str = r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;

fn test_provider(url: &str) -> AnthropicProvider {
    AnthropicProvider::new(SecretString::from("test-key"), "claude-test", 1024)
        .unwrap()
        .with_url(url.to_owned())
}

#[tokio::test]
async fn retry_succeeds_after_transient_429() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOCK_SUCCESS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri());
    let messages = vec![cherub::providers::Message::user_text("hello")];

    let result = provider.complete("system", &messages, &[]).await;
    assert!(result.is_ok(), "should succeed after retry: {result:?}");
}

#[tokio::test]
async fn retry_succeeds_after_server_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOCK_SUCCESS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri());
    let messages = vec![cherub::providers::Message::user_text("hello")];

    let result = provider.complete("system", &messages, &[]).await;
    assert!(result.is_ok(), "should succeed after retry: {result:?}");
}

#[tokio::test]
async fn no_retry_on_client_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .expect(1) // Exactly one request — no retries
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri());
    let messages = vec![cherub::providers::Message::user_text("hello")];

    let result = provider.complete("system", &messages, &[]).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("400"), "error should mention status: {err}");
    assert!(err.contains("0 retries"), "should report 0 retries: {err}");
}

#[tokio::test]
async fn max_retries_exhausted() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .expect(4) // 1 initial + 3 retries = 4 total
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri());
    let messages = vec![cherub::providers::Message::user_text("hello")];

    let result = provider.complete("system", &messages, &[]).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("503"), "error should mention status: {err}");
    assert!(err.contains("3 retries"), "should report 3 retries: {err}");
}

#[tokio::test]
async fn retry_after_header_respected() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_string("rate limited"),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOCK_SUCCESS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri());
    let messages = vec![cherub::providers::Message::user_text("hello")];

    let start = std::time::Instant::now();
    let result = provider.complete("system", &messages, &[]).await;
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "should succeed after retry: {result:?}");
    // The Retry-After header says 1 second, so we should have waited at least ~1s.
    assert!(
        elapsed >= std::time::Duration::from_millis(900),
        "should have respected Retry-After header, elapsed: {elapsed:?}"
    );
}
