use std::time::Duration;

use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use tracing::{Instrument, info, info_span, warn};

use async_trait::async_trait;

use super::pricing::ModelPricing;
use super::wire::{self, RequestBody};
use super::{ApiUsage, Message, Provider, ToolDefinition};
use crate::error::CherubError;
use crate::retry::{RetryConfig, RetryVerdict, classify_status, compute_delay};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Anthropic Messages API provider. Non-streaming for M2.
pub struct AnthropicProvider {
    client: Client,
    api_key: SecretString,
    pub(crate) model: String,
    pub(crate) max_tokens: u32,
    api_url: String,
    retry_config: RetryConfig,
}

impl AnthropicProvider {
    pub fn new(api_key: SecretString, model: &str, max_tokens: u32) -> Result<Self, CherubError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| CherubError::Provider(e.to_string()))?;

        Ok(Self {
            client,
            api_key,
            model: model.to_owned(),
            max_tokens,
            api_url: API_URL.to_owned(),
            retry_config: RetryConfig::new(),
        })
    }

    /// Override the API URL. Intended for testing with wiremock.
    pub fn with_url(mut self, url: String) -> Self {
        self.api_url = url;
        self
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    /// Send a non-streaming completion request to the Anthropic API.
    /// Retries on transient errors (429, 5xx) with exponential backoff.
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        // Use Instrument instead of entered() — EnteredSpan is !Send, which
        // prevents the future from being Send across await points.
        async {
            let wire_messages = wire::messages_to_wire(messages);
            let wire_tools: Vec<_> = tools.iter().map(wire::WireTool::from).collect();

            let body = RequestBody {
                model: &self.model,
                max_tokens: self.max_tokens,
                system,
                messages: wire_messages,
                tools: wire_tools,
                stream: false,
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
                    RetryVerdict::Success => {
                        let resp: wire::ResponseBody = response
                            .json()
                            .await
                            .map_err(|e| CherubError::Provider(format!("JSON parse error: {e}")))?;

                        return Ok(wire::response_to_message(resp));
                    }
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

    fn pricing(&self) -> Option<ModelPricing> {
        // Cache rates: write = 125% of input, read = 10% of input.
        if self.model.starts_with("claude-opus-4") {
            Some(ModelPricing {
                input_per_mtok: 15.0,
                output_per_mtok: 75.0,
                cache_write_per_mtok: 18.75,
                cache_read_per_mtok: 1.50,
            })
        } else if self.model.starts_with("claude-sonnet-4") {
            Some(ModelPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cache_write_per_mtok: 3.75,
                cache_read_per_mtok: 0.30,
            })
        } else if self.model.starts_with("claude-haiku-4") {
            Some(ModelPricing {
                input_per_mtok: 0.80,
                output_per_mtok: 4.0,
                cache_write_per_mtok: 1.0,
                cache_read_per_mtok: 0.08,
            })
        } else if self.model.starts_with("claude-3-5-sonnet")
            || self.model.starts_with("claude-3.5-sonnet")
        {
            Some(ModelPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cache_write_per_mtok: 3.75,
                cache_read_per_mtok: 0.30,
            })
        } else if self.model.starts_with("claude-3-5-haiku")
            || self.model.starts_with("claude-3.5-haiku")
        {
            Some(ModelPricing {
                input_per_mtok: 0.80,
                output_per_mtok: 4.0,
                cache_write_per_mtok: 1.0,
                cache_read_per_mtok: 0.08,
            })
        } else if self.model.starts_with("claude-3-opus") {
            Some(ModelPricing {
                input_per_mtok: 15.0,
                output_per_mtok: 75.0,
                cache_write_per_mtok: 18.75,
                cache_read_per_mtok: 1.50,
            })
        } else if self.model.starts_with("claude-3-sonnet") {
            Some(ModelPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cache_write_per_mtok: 3.75,
                cache_read_per_mtok: 0.30,
            })
        } else if self.model.starts_with("claude-3-haiku") {
            Some(ModelPricing {
                input_per_mtok: 0.25,
                output_per_mtok: 1.25,
                cache_write_per_mtok: 0.3125,
                cache_read_per_mtok: 0.025,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_body_structure() {
        let messages = vec![Message::user_text("hello")];
        let tools = vec![ToolDefinition {
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
            system: "You are helpful.",
            messages: wire_messages,
            tools: wire_tools,
            stream: false,
        };

        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-20250514");
        assert_eq!(json["max_tokens"], 4096);
        assert_eq!(json["system"], "You are helpful.");
        assert_eq!(json["stream"], false);
        assert!(json["tools"].is_array());
        assert_eq!(json["tools"][0]["name"], "bash");
    }

    #[test]
    fn pricing_known_claude_models() {
        let key = SecretString::from("test-key");

        let sonnet = AnthropicProvider::new(key.clone(), "claude-sonnet-4-20250514", 4096).unwrap();
        let p = sonnet.pricing().expect("sonnet-4 should have pricing");
        assert!((p.input_per_mtok - 3.0).abs() < 1e-10);
        assert!((p.output_per_mtok - 15.0).abs() < 1e-10);
        assert!((p.cache_write_per_mtok - 3.75).abs() < 1e-10);
        assert!((p.cache_read_per_mtok - 0.30).abs() < 1e-10);

        let opus = AnthropicProvider::new(key.clone(), "claude-opus-4-20250514", 4096).unwrap();
        let p = opus.pricing().expect("opus-4 should have pricing");
        assert!((p.input_per_mtok - 15.0).abs() < 1e-10);
        assert!((p.output_per_mtok - 75.0).abs() < 1e-10);

        let haiku = AnthropicProvider::new(key.clone(), "claude-haiku-4-20250514", 4096).unwrap();
        assert!(haiku.pricing().is_some());
    }

    #[test]
    fn pricing_unknown_model_returns_none() {
        let key = SecretString::from("test-key");
        let provider = AnthropicProvider::new(key, "llama-3-70b", 4096).unwrap();
        assert!(provider.pricing().is_none());
    }
}
