pub mod anthropic;
pub(crate) mod wire;

use std::future::Future;

use crate::error::CherubError;

/// Abstraction over LLM providers. `dyn Provider` is a legitimate extension boundary
/// per project convention — multiple LLM backends will implement this trait.
pub trait Provider: Send + Sync {
    fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> impl Future<Output = Result<Message, CherubError>> + Send;
}

/// Content within a user message. Supports text and images for multimodal input.
pub enum UserContent {
    Text(String),
    Image { media_type: String, data: String },
}

/// Content blocks within an assistant message.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
