pub mod anthropic;
pub mod pricing;
pub(crate) mod wire;

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::error::CherubError;

/// Token usage reported by the API after a completion call.
#[derive(Debug, Clone, Copy)]
pub struct ApiUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Abstraction over LLM providers. `dyn Provider` is a legitimate extension boundary
/// per project convention — multiple LLM backends will implement this trait.
pub trait Provider: Send + Sync {
    fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> impl Future<Output = Result<(Message, Option<ApiUsage>), CherubError>> + Send;

    /// The model identifier string (e.g. "claude-sonnet-4-20250514").
    fn model_name(&self) -> &str;

    /// Maximum output tokens configured for this provider.
    fn max_output_tokens(&self) -> u32;
}

/// Content within a user message. Supports text and images for multimodal input.
///
/// Uses adjacent tagging (`tag` + `content`) because `Text(String)` is a newtype
/// variant — internal tagging can't serialize a newtype containing a scalar.
/// JSON: `{"type":"text","content":"hello"}` / `{"type":"image","content":{...}}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum UserContent {
    Text(String),
    Image { media_type: String, data: String },
}

/// Content blocks within an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

/// Messages exchanged between the runtime and LLM providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: Vec<UserContent>,
    },
    Assistant {
        content: Vec<ContentBlock>,
        stop_reason: StopReason,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

impl Message {
    /// Convenience constructor for text-only user messages.
    pub fn user_text(s: &str) -> Self {
        Message::User {
            content: vec![UserContent::Text(s.to_owned())],
        }
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

/// Schema definition for a tool, sent to the provider so the model knows what tools are available.
pub struct ToolDefinition {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: serde_json::Value,
}
