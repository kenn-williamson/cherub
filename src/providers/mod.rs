pub mod anthropic;
pub(crate) mod wire;

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
        content: String,
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
