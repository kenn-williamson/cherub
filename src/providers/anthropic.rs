use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use tracing::{Instrument, info, info_span, warn};

use async_trait::async_trait;

use super::wire::{self, RequestBody};
use super::{ApiUsage, Message, Provider, ToolDefinition};
use crate::error::CherubError;
use crate::retry::{RetryConfig, RetryVerdict, classify_status, compute_delay};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Maximum silence between streamed bytes before we treat the connection as
/// dead. The API emits deltas and periodic pings well within this window even
/// during extended thinking, so a longer gap means a stalled connection — not a
/// slow-but-healthy generation. This is the liveness check; there is no total
/// cap on a healthy stream, so Claude may think for as long as it needs.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Absolute backstop on a single streaming response, far beyond any real
/// completion. Guards only against a pathological stream that keeps emitting
/// bytes (e.g. endless pings) without ever completing.
const STREAM_MAX_DURATION: Duration = Duration::from_secs(900);

/// Anthropic Messages API provider. Streams responses over SSE and reassembles
/// them into a complete message (the streaming is internal to `complete`).
pub struct AnthropicProvider {
    client: Client,
    api_key: SecretString,
    pub(crate) model: String,
    pub(crate) max_tokens: u32,
    api_url: String,
    retry_config: RetryConfig,
    thinking_budget: Option<u32>,
}

impl AnthropicProvider {
    pub fn new(api_key: SecretString, model: &str, max_tokens: u32) -> Result<Self, CherubError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // No read_timeout or total timeout: responses are streamed (stream: true)
            // and consumed with a per-chunk idle timeout (STREAM_IDLE_TIMEOUT). A total
            // request timeout would kill long-but-healthy generations (e.g. extended
            // thinking); liveness is instead judged by whether bytes keep arriving.
            .build()
            .map_err(|e| CherubError::Provider(e.to_string()))?;

        Ok(Self {
            client,
            api_key,
            model: model.to_owned(),
            max_tokens,
            api_url: API_URL.to_owned(),
            retry_config: RetryConfig::new(),
            thinking_budget: None,
        })
    }

    /// Override the API URL. Intended for testing with wiremock.
    pub fn with_url(mut self, url: String) -> Self {
        self.api_url = url;
        self
    }

    /// Enable extended thinking with the given token budget.
    ///
    /// `budget` must be less than `max_tokens`. If `budget >= max_tokens`,
    /// it is clamped to `max_tokens - 1`.
    pub fn with_thinking_budget(mut self, budget: u32) -> Self {
        self.thinking_budget = Some(budget.min(self.max_tokens.saturating_sub(1)));
        self
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    /// Send a streaming completion request to the Anthropic API, consuming the
    /// SSE stream into a complete message. Retries on transient errors (429,
    /// 5xx, dropped/idle streams) with exponential backoff.
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        // Use Instrument instead of entered() — EnteredSpan is !Send, which
        // prevents the future from being Send across await points.
        async {
            let mut wire_messages = wire::messages_to_wire(messages);
            // Mark the last content block of the last message as a cache breakpoint.
            // This caches the entire conversation history; subsequent turns read
            // this prefix and only pay full price for new content.
            wire::mark_conversation_cache(&mut wire_messages);
            let mut wire_tools: Vec<_> = tools.iter().map(wire::WireTool::from).collect();
            // Mark the last tool definition as a cache breakpoint. Render order
            // is tools → system → messages, so this caches the entire tool set
            // (everything after it in tools/system stays cacheable when followed
            // by another marker on system).
            if let Some(last) = wire_tools.last_mut() {
                last.cache_control = Some(wire::CacheControl::Ephemeral);
            }

            // System prompt as a single block with cache_control. This caches
            // tools + system together as the stable prefix.
            let system_blocks = vec![wire::SystemBlock {
                block_type: "text",
                text: system,
                cache_control: Some(wire::CacheControl::Ephemeral),
            }];

            let thinking = self.thinking_budget.map(|budget| wire::ThinkingConfig {
                config_type: "enabled",
                budget_tokens: budget,
            });

            let body = RequestBody {
                model: &self.model,
                max_tokens: self.max_tokens,
                system: system_blocks,
                messages: wire_messages,
                tools: wire_tools,
                stream: true,
                thinking,
            };

            let json_body = serde_json::to_vec(&body)
                .map_err(|e| CherubError::Provider(format!("JSON serialize error: {e}")))?;

            for attempt in 0..=self.retry_config.max_retries {
                // NEVER log the API key — SecretString redacts on Debug, but we never format it either.
                let result = self
                    .client
                    .post(&self.api_url)
                    .header("x-api-key", self.api_key.expose_secret())
                    .header("anthropic-version", API_VERSION)
                    .header("content-type", "application/json")
                    .body(json_body.clone())
                    .send()
                    .await;

                let response = match result {
                    Ok(r) => r,
                    Err(e)
                        if (e.is_connect() || e.is_timeout())
                            && attempt < self.retry_config.max_retries =>
                    {
                        let delay = compute_delay(&self.retry_config, attempt);
                        warn!(
                            error = %e,
                            attempt,
                            delay_ms = delay.as_millis() as u64,
                            "retrying API call (connection/timeout error)"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    Err(e) => {
                        let retries = attempt;
                        return Err(CherubError::Provider(format!(
                            "connection error: {e} (after {retries} retries)"
                        )));
                    }
                };

                let status = response.status().as_u16();
                info!(status);

                match classify_status(status) {
                    RetryVerdict::Success => match consume_stream(response).await {
                        StreamOutcome::Done(resp) => return Ok(wire::response_to_message(resp)),
                        StreamOutcome::Retry(msg) if attempt < self.retry_config.max_retries => {
                            let delay = compute_delay(&self.retry_config, attempt);
                            warn!(
                                error = %msg,
                                attempt,
                                delay_ms = delay.as_millis() as u64,
                                "retrying API call (stream interrupted)"
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        StreamOutcome::Retry(msg) => {
                            return Err(CherubError::Provider(format!(
                                "stream error: {msg} (after {attempt} retries)"
                            )));
                        }
                        StreamOutcome::Fatal(msg) => {
                            return Err(CherubError::Provider(format!("stream error: {msg}")));
                        }
                    },
                    RetryVerdict::Transient(_) if attempt < self.retry_config.max_retries => {
                        // Parse Retry-After header (Anthropic sends seconds as integer).
                        let retry_after = response
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .map(Duration::from_secs);

                        let delay = retry_after
                            .unwrap_or_else(|| compute_delay(&self.retry_config, attempt));
                        warn!(
                            status,
                            attempt,
                            delay_ms = delay.as_millis() as u64,
                            "retrying API call"
                        );
                        tokio::time::sleep(delay).await;
                    }
                    RetryVerdict::Transient(_) | RetryVerdict::Permanent => {
                        let body_text = response.text().await.unwrap_or_default();
                        let retries = attempt;
                        warn!(status, "API error response");
                        return Err(CherubError::Provider(format!(
                            "API error {status}: {body_text} (after {retries} retries)"
                        )));
                    }
                }
            }

            // Unreachable: the loop always returns or continues.
            unreachable!("retry loop exhausted without returning")
        }
        .instrument(info_span!("api_call", model = %self.model))
        .await
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn max_output_tokens(&self) -> u32 {
        self.max_tokens
    }
}

/// Result of consuming a streamed response.
enum StreamOutcome {
    /// Fully assembled response.
    Done(wire::ResponseBody),
    /// Transient failure (idle timeout, dropped connection, overloaded) — retry.
    Retry(String),
    /// Non-retryable failure (malformed stream, permanent error event).
    Fatal(String),
}

/// Read an SSE response body to completion, assembling it into a `ResponseBody`.
/// Liveness is enforced by an idle timeout between chunks plus an absolute
/// backstop; a healthy stream may otherwise take as long as it needs.
async fn consume_stream(response: reqwest::Response) -> StreamOutcome {
    let started = tokio::time::Instant::now();
    let mut stream = response.bytes_stream();
    let mut acc = wire::StreamAccumulator::default();
    let mut buf: Vec<u8> = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();

    loop {
        if started.elapsed() >= STREAM_MAX_DURATION {
            return StreamOutcome::Retry("stream exceeded maximum duration".to_owned());
        }
        let chunk = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()).await {
            Err(_) => return StreamOutcome::Retry("idle timeout: no data received".to_owned()),
            Ok(None) => break, // stream ended cleanly
            Ok(Some(Ok(bytes))) => bytes,
            Ok(Some(Err(e))) => return StreamOutcome::Retry(format!("stream read error: {e}")),
        };

        if let Err(f) = wire::feed_sse(&mut acc, &mut buf, &mut data_lines, &chunk) {
            return if f.retryable {
                StreamOutcome::Retry(f.message)
            } else {
                StreamOutcome::Fatal(f.message)
            };
        }
    }

    StreamOutcome::Done(acc.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_body_structure() {
        let messages = vec![Message::user_text("hello")];
        let tools = [ToolDefinition {
            name: "bash".to_owned(),
            description: "Execute bash commands".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The bash command to run" }
                },
                "required": ["command"]
            }),
        }];

        let wire_messages = wire::messages_to_wire(&messages);
        let wire_tools: Vec<_> = tools.iter().map(wire::WireTool::from).collect();

        let body = RequestBody {
            model: "claude-sonnet-4-20250514",
            max_tokens: 4096,
            system: vec![wire::SystemBlock {
                block_type: "text",
                text: "You are helpful.",
                cache_control: None,
            }],
            messages: wire_messages,
            tools: wire_tools,
            stream: false,
            thinking: None,
        };

        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-20250514");
        assert_eq!(json["max_tokens"], 4096);
        assert_eq!(json["system"][0]["text"], "You are helpful.");
        assert_eq!(json["system"][0]["type"], "text");
        assert_eq!(json["stream"], false);
        assert!(json["tools"].is_array());
        assert_eq!(json["tools"][0]["name"], "bash");
    }
}
