//! Integration tests for OpenAI-compatible API retry with exponential backoff.
//!
//! Uses wiremock to simulate OpenAI API responses. Each test spins up a
//! `MockServer` and creates an `OpenAiProvider` pointed at it.

use secrecy::SecretString;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cherub::providers::Provider;
use cherub::providers::openai::OpenAiProvider;

/// Valid mock 200 response body that matches the OpenAI Chat Completions wire format.
const MOCK_SUCCESS_BODY: &str = r#"{"choices":[{"message":{"content":"ok","tool_calls":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;

fn test_provider(base_url: &str, api_key: Option<SecretString>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "gpt-test", 1024)
        .unwrap()
        .with_base_url(base_url.to_owned())
}

#[tokio::test]
async fn retry_succeeds_after_transient_429() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOCK_SUCCESS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri(), Some(SecretString::from("test-key")));
    let messages = vec![cherub::providers::Message::user_text("hello")];

    let result = provider.complete("system", &messages, &[]).await;
    assert!(result.is_ok(), "should succeed after retry: {result:?}");
}

#[tokio::test]
async fn retry_succeeds_after_server_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOCK_SUCCESS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri(), Some(SecretString::from("test-key")));
    let messages = vec![cherub::providers::Message::user_text("hello")];

    let result = provider.complete("system", &messages, &[]).await;
    assert!(result.is_ok(), "should succeed after retry: {result:?}");
}

#[tokio::test]
async fn no_retry_on_client_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .expect(1) // Exactly one request — no retries
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri(), Some(SecretString::from("test-key")));
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
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .expect(4) // 1 initial + 3 retries = 4 total
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri(), Some(SecretString::from("test-key")));
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
        .and(path("/chat/completions"))
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
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOCK_SUCCESS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri(), Some(SecretString::from("test-key")));
    let messages = vec![cherub::providers::Message::user_text("hello")];

    let start = std::time::Instant::now();
    let result = provider.complete("system", &messages, &[]).await;
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "should succeed after retry: {result:?}");
    assert!(
        elapsed >= std::time::Duration::from_millis(900),
        "should have respected Retry-After header, elapsed: {elapsed:?}"
    );
}

#[tokio::test]
async fn no_auth_header_when_key_is_none() {
    let server = MockServer::start().await;

    // Mount a mock that requires NO authorization header.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOCK_SUCCESS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri(), None);
    let messages = vec![cherub::providers::Message::user_text("hello")];

    let result = provider.complete("system", &messages, &[]).await;
    assert!(result.is_ok(), "should succeed without auth: {result:?}");

    // Verify that no authorization header was sent.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        !requests[0].headers.contains_key("authorization"),
        "should not send authorization header when key is None"
    );
}

#[tokio::test]
async fn auth_header_present_when_key_is_set() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOCK_SUCCESS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri(), Some(SecretString::from("sk-test-key")));
    let messages = vec![cherub::providers::Message::user_text("hello")];

    let result = provider.complete("system", &messages, &[]).await;
    assert!(result.is_ok(), "should succeed with auth: {result:?}");

    let requests = server.received_requests().await.unwrap();
    let auth = requests[0]
        .headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(auth, "Bearer sk-test-key");
}

#[tokio::test]
async fn tool_calls_roundtrip() {
    let server = MockServer::start().await;

    let mock_response = r#"{
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": "call_abc123",
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": "{\"command\":\"ls /tmp\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 20, "completion_tokens": 15}
    }"#;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(mock_response))
        .expect(1)
        .mount(&server)
        .await;

    let provider = test_provider(&server.uri(), Some(SecretString::from("test-key")));
    let messages = vec![cherub::providers::Message::user_text("list files")];

    let (msg, usage) = provider.complete("system", &messages, &[]).await.unwrap();

    // Verify usage.
    let usage = usage.expect("usage should be present");
    assert_eq!(usage.input_tokens, 20);
    assert_eq!(usage.output_tokens, 15);

    // Verify message contains tool use.
    match msg {
        cherub::providers::Message::Assistant {
            content,
            stop_reason,
        } => {
            assert_eq!(stop_reason, cherub::providers::StopReason::ToolUse);
            assert_eq!(content.len(), 1);
            match &content[0] {
                cherub::providers::ContentBlock::ToolUse { id, name, input } => {
                    assert_eq!(id, "call_abc123");
                    assert_eq!(name, "bash");
                    assert_eq!(input["command"], "ls /tmp");
                }
                _ => panic!("expected ToolUse block"),
            }
        }
        _ => panic!("expected Assistant message"),
    }
}
