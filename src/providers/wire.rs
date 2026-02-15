//! Private serde structs for Anthropic API JSON format.
//! These map 1:1 to the Anthropic Messages API wire format.

use serde::{Deserialize, Serialize};

use super::{ContentBlock, Message, StopReason, ToolDefinition};

// --- Request types ---

#[derive(Serialize)]
pub(crate) struct RequestBody<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub system: &'a str,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    pub stream: bool,
}

#[derive(Serialize)]
pub(crate) struct WireMessage {
    pub role: &'static str,
    pub content: WireContent,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum WireContent {
    Text(String),
    Blocks(Vec<WireContentBlock>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
pub(crate) enum WireContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Serialize)]
pub(crate) struct WireTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

// --- Response types ---

#[derive(Deserialize)]
pub(crate) struct ResponseBody {
    pub content: Vec<ResponseContentBlock>,
    pub stop_reason: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub(crate) enum ResponseContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

// --- Conversions ---

impl From<&ToolDefinition> for WireTool {
    fn from(def: &ToolDefinition) -> Self {
        Self {
            name: def.name.clone(),
            description: def.description.clone(),
            input_schema: def.input_schema.clone(),
        }
    }
}

/// Convert internal messages to wire format.
/// Consecutive ToolResult messages are merged into a single `user` message
/// with multiple `tool_result` content blocks (Anthropic API requirement).
pub(crate) fn messages_to_wire(messages: &[Message]) -> Vec<WireMessage> {
    let mut wire = Vec::new();
    let mut pending_results: Vec<WireContentBlock> = Vec::new();

    for msg in messages {
        match msg {
            Message::User { content } => {
                flush_results(&mut wire, &mut pending_results);
                wire.push(WireMessage {
                    role: "user",
                    content: WireContent::Text(content.clone()),
                });
            }
            Message::Assistant { content, .. } => {
                flush_results(&mut wire, &mut pending_results);
                let blocks = content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text } => WireContentBlock::Text { text: text.clone() },
                        ContentBlock::ToolUse { id, name, input } => WireContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        },
                    })
                    .collect();
                wire.push(WireMessage {
                    role: "assistant",
                    content: WireContent::Blocks(blocks),
                });
            }
            Message::ToolResult { tool_use_id, content, is_error } => {
                pending_results.push(WireContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                });
            }
        }
    }

    flush_results(&mut wire, &mut pending_results);
    wire
}

fn flush_results(wire: &mut Vec<WireMessage>, pending: &mut Vec<WireContentBlock>) {
    if !pending.is_empty() {
        wire.push(WireMessage {
            role: "user",
            content: WireContent::Blocks(std::mem::take(pending)),
        });
    }
}

/// Convert a wire response to our internal Message type.
pub(crate) fn response_to_message(resp: ResponseBody) -> Message {
    let stop_reason = match resp.stop_reason.as_str() {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    };

    let content = resp
        .content
        .into_iter()
        .map(|block| match block {
            ResponseContentBlock::Text { text } => ContentBlock::Text { text },
            ResponseContentBlock::ToolUse { id, name, input } => {
                ContentBlock::ToolUse { id, name, input }
            }
        })
        .collect();

    Message::Assistant { content, stop_reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_message_serializes() {
        let wire = messages_to_wire(&[Message::User {
            content: "hello".to_owned(),
        }]);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "user");
        let json = serde_json::to_value(&wire[0]).unwrap();
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn consecutive_tool_results_merge() {
        let messages = vec![
            Message::ToolResult {
                tool_use_id: "id1".to_owned(),
                content: "output1".to_owned(),
                is_error: false,
            },
            Message::ToolResult {
                tool_use_id: "id2".to_owned(),
                content: "output2".to_owned(),
                is_error: true,
            },
        ];
        let wire = messages_to_wire(&messages);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "user");
        let json = serde_json::to_value(&wire[0]).unwrap();
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "id1");
        assert_eq!(blocks[1]["type"], "tool_result");
        assert_eq!(blocks[1]["tool_use_id"], "id2");
        assert!(blocks[1]["is_error"].as_bool().unwrap());
    }

    #[test]
    fn request_body_json_structure() {
        let body = RequestBody {
            model: "claude-sonnet-4-20250514",
            max_tokens: 4096,
            system: "You are helpful.",
            messages: vec![WireMessage {
                role: "user",
                content: WireContent::Text("hi".to_owned()),
            }],
            tools: vec![WireTool {
                name: "bash".to_owned(),
                description: "Run bash".to_owned(),
                input_schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
            }],
            stream: false,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-20250514");
        assert_eq!(json["stream"], false);
        assert_eq!(json["tools"][0]["name"], "bash");
    }

    #[test]
    fn response_parsing_text_only() {
        let resp = ResponseBody {
            content: vec![ResponseContentBlock::Text {
                text: "Hello!".to_owned(),
            }],
            stop_reason: "end_turn".to_owned(),
        };
        let msg = response_to_message(resp);
        match msg {
            Message::Assistant { content, stop_reason } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "Hello!"),
                    _ => panic!("expected Text block"),
                }
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn response_parsing_tool_use() {
        let resp = ResponseBody {
            content: vec![
                ResponseContentBlock::Text {
                    text: "Let me run that.".to_owned(),
                },
                ResponseContentBlock::ToolUse {
                    id: "toolu_123".to_owned(),
                    name: "bash".to_owned(),
                    input: json!({"command": "ls /tmp"}),
                },
            ],
            stop_reason: "tool_use".to_owned(),
        };
        let msg = response_to_message(resp);
        match msg {
            Message::Assistant { content, stop_reason } => {
                assert_eq!(stop_reason, StopReason::ToolUse);
                assert_eq!(content.len(), 2);
                match &content[1] {
                    ContentBlock::ToolUse { id, name, input } => {
                        assert_eq!(id, "toolu_123");
                        assert_eq!(name, "bash");
                        assert_eq!(input["command"], "ls /tmp");
                    }
                    _ => panic!("expected ToolUse block"),
                }
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn response_json_deserialization() {
        let json_str = r#"{
            "content": [
                {"type": "text", "text": "Sure."},
                {"type": "tool_use", "id": "toolu_abc", "name": "bash", "input": {"command": "pwd"}}
            ],
            "stop_reason": "tool_use"
        }"#;
        let resp: ResponseBody = serde_json::from_str(json_str).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert_eq!(resp.stop_reason, "tool_use");
    }
}
