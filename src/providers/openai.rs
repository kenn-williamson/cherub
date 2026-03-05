use std::time::Duration;

use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use tracing::{Instrument, info, info_span, warn};

use async_trait::async_trait;

use super::openai_wire::{self, ChatCompletionRequest, ChatCompletionResponse, OaiTool};
use super::{ApiUsage, Message, Provider, ToolDefinition};
use crate::error::CherubError;
use crate::retry::{RetryConfig, RetryVerdict, classify_status, compute_delay};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// OpenAI Chat Completions API provider. Covers any compatible endpoint:
/// OpenAI, Azure OpenAI, Gemini, Ollama, vLLM, LM Studio, Groq.
pub struct OpenAiProvider {
    client: Client,
    api_key: Option<SecretString>,
    pub(crate) model: String,
    pub(crate) max_tokens: u32,
    base_url: String,
    retry_config: RetryConfig,
}

impl OpenAiProvider {
    pub fn new(
        api_key: Option<SecretString>,
        model: &str,
        max_tokens: u32,
    ) -> Result<Self, CherubError> {
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
            base_url: DEFAULT_BASE_URL.to_owned(),
            retry_config: RetryConfig::new(),
        })
    }

    /// Override the base URL. For Ollama, vLLM, LM Studio, Groq, Azure, etc.
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    /// Send a non-streaming completion request to an OpenAI-compatible API.
    /// Retries on transient errors (429, 5xx) with exponential backoff.
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        async {
            let wire_messages = openai_wire::messages_to_openai_wire(system, messages);
            let wire_tools: Vec<OaiTool> = tools.iter().map(OaiTool::from).collect();

            let body = ChatCompletionRequest {
                model: &self.model,
                max_tokens: self.max_tokens,
                messages: wire_messages,
                tools: wire_tools,
            };

            let json_body = serde_json::to_vec(&body)
                .map_err(|e| CherubError::Provider(format!("JSON serialize error: {e}")))?;

            let url = format!("{}/chat/completions", self.base_url);

            for attempt in 0..=self.retry_config.max_retries {
                let mut req = self
                    .client
                    .post(&url)
                    .header("content-type", "application/json")
                    .body(json_body.clone());

                // Add auth header only when an API key is present.
                if let Some(ref key) = self.api_key {
                    req = req.header("authorization", format!("Bearer {}", key.expose_secret()));
                }

                let result = req.send().await;

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
                        let resp: ChatCompletionResponse = response
                            .json()
                            .await
                            .map_err(|e| CherubError::Provider(format!("JSON parse error: {e}")))?;

                        return Ok(openai_wire::openai_response_to_message(resp));
                    }
                    RetryVerdict::Transient(_) if attempt < self.retry_config.max_retries => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_structure() {
        use serde_json::json;

        let messages = vec![Message::user_text("hello")];
        let tools = vec![ToolDefinition {
            name: "bash".to_owned(),
            description: "Execute bash commands".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }),
        }];

        let wire_messages = openai_wire::messages_to_openai_wire("You are helpful.", &messages);
        let wire_tools: Vec<_> = tools.iter().map(OaiTool::from).collect();

        let body = ChatCompletionRequest {
            model: "gpt-4o",
            max_tokens: 4096,
            messages: wire_messages,
            tools: wire_tools,
        };

        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "gpt-4o");
        assert_eq!(json["max_tokens"], 4096);
        assert_eq!(json["messages"][0]["role"], "system");
        assert!(json["tools"].is_array());
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "bash");
    }
}
