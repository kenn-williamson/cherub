//! Token estimation for context window management.
//!
//! Pure functions, no feature gates, no external dependencies.
//! Estimates are deliberately conservative — overestimating is safer than underestimating
//! because compaction triggers slightly early rather than hitting API limits.

use crate::providers::{ContentBlock, Message, ToolDefinition, UserContent};

/// Approximate chars-per-token for natural language text.
const CHARS_PER_TOKEN_TEXT: u32 = 4;

/// Approximate chars-per-token for JSON (tool_use inputs, tool_result content).
/// JSON is denser in tokens due to structural characters and short keys.
const CHARS_PER_TOKEN_JSON: u32 = 3;

/// Overhead per message for role tokens and framing.
const PER_MESSAGE_OVERHEAD: u32 = 4;

/// Estimate the total input token count for a provider call.
///
/// Walks all messages, estimating text at 4 chars/token and JSON-heavy content
/// (tool_use inputs, tool_result bodies) at 3 chars/token. Adds system prompt
/// and tool schema overhead.
pub fn estimate_tokens(system: &str, messages: &[Message], tools: &[ToolDefinition]) -> u32 {
    let mut total: u32 = 0;

    // System prompt
    total += (system.len() as u32) / CHARS_PER_TOKEN_TEXT;

    // Tool definitions (serialized as JSON schemas)
    for tool in tools {
        total += (tool.name.len() as u32) / CHARS_PER_TOKEN_TEXT;
        total += (tool.description.len() as u32) / CHARS_PER_TOKEN_TEXT;
        let schema_len = tool.input_schema.to_string().len() as u32;
        total += schema_len / CHARS_PER_TOKEN_JSON;
    }

    // Messages
    for msg in messages {
        total += PER_MESSAGE_OVERHEAD;
        match msg {
            Message::User { content } => {
                for c in content {
                    match c {
                        UserContent::Text(text) => {
                            total += (text.len() as u32) / CHARS_PER_TOKEN_TEXT;
                        }
                        UserContent::Image { .. } => {
                            // Images are billed separately by the API; estimate a fixed cost.
                            total += 1000;
                        }
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for block in content {
                    match block {
                        ContentBlock::Text { text } => {
                            total += (text.len() as u32) / CHARS_PER_TOKEN_TEXT;
                        }
                        ContentBlock::ToolUse { name, input, .. } => {
                            total += (name.len() as u32) / CHARS_PER_TOKEN_TEXT;
                            let input_len = input.to_string().len() as u32;
                            total += input_len / CHARS_PER_TOKEN_JSON;
                        }
                    }
                }
            }
            Message::ToolResult { content, .. } => {
                total += (content.len() as u32) / CHARS_PER_TOKEN_JSON;
            }
        }
    }

    total
}

/// Return the context window size for a given model name.
///
/// Conservative default: 200,000 tokens for all claude-* models.
/// Non-claude models get a safe 100,000 default.
pub fn context_window_size(model: &str) -> u32 {
    if model.starts_with("claude") {
        200_000
    } else {
        100_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_conversation() {
        let tokens = estimate_tokens("system prompt", &[], &[]);
        // "system prompt" = 13 chars → 13/4 = 3 tokens
        assert_eq!(tokens, 3);
    }

    #[test]
    fn single_user_message() {
        let messages = vec![Message::user_text("Hello, how are you?")];
        let tokens = estimate_tokens("", &messages, &[]);
        // "Hello, how are you?" = 19 chars → 19/4 = 4 tokens + 4 overhead = 8
        assert_eq!(tokens, 8);
    }

    #[test]
    fn tool_use_estimates_json() {
        let messages = vec![Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t1".to_owned(),
                name: "bash".to_owned(),
                input: json!({"command": "ls /tmp"}),
            }],
            stop_reason: crate::providers::StopReason::ToolUse,
        }];
        let tokens = estimate_tokens("", &messages, &[]);
        // 4 overhead + "bash" (4/4=1) + JSON input string at 3 chars/token
        assert!(tokens > 4);
    }

    #[test]
    fn tool_result_estimates_json() {
        let messages = vec![Message::ToolResult {
            tool_use_id: "t1".to_owned(),
            content: "file1.txt\nfile2.txt\nfile3.txt".to_owned(),
            is_error: false,
        }];
        let tokens = estimate_tokens("", &messages, &[]);
        // 4 overhead + 28 chars / 3 = 9 → total 13
        assert_eq!(tokens, 13);
    }

    #[test]
    fn tool_definitions_add_overhead() {
        let tools = vec![ToolDefinition {
            name: "bash".to_owned(),
            description: "Execute bash commands in a sandboxed environment".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The bash command to run"}
                },
                "required": ["command"]
            }),
        }];
        let with_tools = estimate_tokens("", &[], &tools);
        let without_tools = estimate_tokens("", &[], &[]);
        assert!(with_tools > without_tools);
    }

    #[test]
    fn image_adds_fixed_cost() {
        let messages = vec![Message::User {
            content: vec![UserContent::Image {
                media_type: "image/png".to_owned(),
                data: "base64data".to_owned(),
            }],
        }];
        let tokens = estimate_tokens("", &messages, &[]);
        // 4 overhead + 1000 fixed image cost
        assert_eq!(tokens, 1004);
    }

    #[test]
    fn context_window_claude_models() {
        assert_eq!(context_window_size("claude-sonnet-4-20250514"), 200_000);
        assert_eq!(context_window_size("claude-opus-4-20250514"), 200_000);
        assert_eq!(context_window_size("claude-haiku-3-5"), 200_000);
    }

    #[test]
    fn context_window_non_claude() {
        assert_eq!(context_window_size("gpt-4"), 100_000);
        assert_eq!(context_window_size("unknown"), 100_000);
    }

    #[test]
    fn multi_turn_conversation() {
        let messages = vec![
            Message::user_text("What files are in /tmp?"),
            Message::Assistant {
                content: vec![
                    ContentBlock::Text {
                        text: "Let me check.".to_owned(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".to_owned(),
                        name: "bash".to_owned(),
                        input: json!({"command": "ls /tmp"}),
                    },
                ],
                stop_reason: crate::providers::StopReason::ToolUse,
            },
            Message::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: "file1.txt\nfile2.txt".to_owned(),
                is_error: false,
            },
            Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: "There are two files in /tmp: file1.txt and file2.txt.".to_owned(),
                }],
                stop_reason: crate::providers::StopReason::EndTurn,
            },
        ];
        let tokens = estimate_tokens("You are a coding assistant.", &messages, &[]);
        // Should be a reasonable estimate — exact value less important than > 0
        assert!(tokens > 20);
    }
}
