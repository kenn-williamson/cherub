use std::time::Duration;

use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use tracing::{info, info_span, warn};

use super::wire::{self, RequestBody};
use super::{Message, ToolDefinition};
use crate::error::CherubError;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Anthropic Messages API provider. Non-streaming for M2.
pub struct AnthropicProvider {
    client: Client,
    api_key: SecretString,
    pub(crate) model: String,
    pub(crate) max_tokens: u32,
}

impl AnthropicProvider {
    pub fn new(api_key: SecretString, model: &str, max_tokens: u32) -> Result<Self, CherubError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| CherubError::Provider(e.to_string()))?;

        Ok(Self {
            client,
            api_key,
            model: model.to_owned(),
            max_tokens,
        })
    }

    /// Send a non-streaming completion request to the Anthropic API.
    pub async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<Message, CherubError> {
        let _span = info_span!("api_call", model = %self.model).entered();

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

        // NEVER log the API key — SecretString redacts on Debug, but we never format it either.
        let response = self
            .client
            .post(API_URL)
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| CherubError::Provider(e.to_string()))?;

        let status = response.status();
        info!(status = %status);

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            warn!(status = %status, "API error response");
            return Err(CherubError::Provider(format!("API error {status}: {body}")));
        }

        let resp: wire::ResponseBody = response
            .json()
            .await
            .map_err(|e| CherubError::Provider(format!("JSON parse error: {e}")))?;

        Ok(wire::response_to_message(resp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_body_structure() {
        let messages = vec![Message::User {
            content: "hello".to_owned(),
        }];
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
}
