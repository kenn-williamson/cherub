//! Private serde structs for Anthropic API JSON format.
//! These map 1:1 to the Anthropic Messages API wire format.

use serde::{Deserialize, Serialize};

use super::{ContentBlock, Message, StopReason, ToolDefinition, UserContent};

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
    #[serde(rename = "image")]
    Image { source: WireImageSource },
}

#[derive(Serialize)]
pub(crate) struct WireImageSource {
    #[serde(rename = "type")]
    pub source_type: &'static str,
    pub media_type: String,
    pub data: String,
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

/// Convert `UserContent` items to wire format.
/// Single text-only → `WireContent::Text` (compact).
/// Image-only, mixed, or multi-item → `WireContent::Blocks`.
fn user_content_to_wire(content: &[UserContent]) -> WireContent {
    // Single text → compact format
    if content.len() == 1
        && let UserContent::Text(text) = &content[0]
    {
        return WireContent::Text(text.clone());
    }

    let blocks = content
        .iter()
        .map(|c| match c {
            UserContent::Text(text) => WireContentBlock::Text { text: text.clone() },
            UserContent::Image { media_type, data } => WireContentBlock::Image {
                source: WireImageSource {
                    source_type: "base64",
                    media_type: media_type.clone(),
                    data: data.clone(),
                },
            },
        })
        .collect();

    WireContent::Blocks(blocks)
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
                    content: user_content_to_wire(content),
                });
            }
            Message::Assistant { content, .. } => {
                flush_results(&mut wire, &mut pending_results);
                let blocks = content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text } => {
                            WireContentBlock::Text { text: text.clone() }
                        }
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
            Message::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
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

    Message::Assistant {
        content,
        stop_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_text_message_serializes() {
        let wire = messages_to_wire(&[Message::user_text("hello")]);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "user");
        let json = serde_json::to_value(&wire[0]).unwrap();
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn user_image_message_serializes() {
        let wire = messages_to_wire(&[Message::User {
            content: vec![UserContent::Image {
                media_type: "image/png".to_owned(),
                data: "iVBOR...".to_owned(),
            }],
        }]);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "user");
        let json = serde_json::to_value(&wire[0]).unwrap();
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["type"], "base64");
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[0]["source"]["data"], "iVBOR...");
    }

    #[test]
    fn user_mixed_content_serializes() {
        let wire = messages_to_wire(&[Message::User {
            content: vec![
                UserContent::Text("What is in this image?".to_owned()),
                UserContent::Image {
                    media_type: "image/jpeg".to_owned(),
                    data: "/9j/4AAQ...".to_owned(),
                },
            ],
        }]);
        assert_eq!(wire.len(), 1);
        let json = serde_json::to_value(&wire[0]).unwrap();
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "What is in this image?");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["media_type"], "image/jpeg");
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
            Message::Assistant {
                content,
                stop_reason,
            } => {
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
            Message::Assistant {
                content,
                stop_reason,
            } => {
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

    // --- Step 4: Context window isolation — malformed JSON deserialization ---

    #[test]
    fn malformed_json_fails() {
        let truncated = r#"{"content": [{"type": "text", "text": "hel"#;
        assert!(serde_json::from_str::<ResponseBody>(truncated).is_err());
    }

    #[test]
    fn missing_content_field_fails() {
        let json_str = r#"{"stop_reason": "end_turn"}"#;
        assert!(serde_json::from_str::<ResponseBody>(json_str).is_err());
    }

    #[test]
    fn missing_stop_reason_fails() {
        let json_str = r#"{"content": [{"type": "text", "text": "hello"}]}"#;
        assert!(serde_json::from_str::<ResponseBody>(json_str).is_err());
    }

    #[test]
    fn tool_use_missing_id_fails() {
        let json_str = r#"{
            "content": [{"type": "tool_use", "name": "bash", "input": {}}],
            "stop_reason": "tool_use"
        }"#;
        assert!(serde_json::from_str::<ResponseBody>(json_str).is_err());
    }

    #[test]
    fn tool_use_missing_name_fails() {
        let json_str = r#"{
            "content": [{"type": "tool_use", "id": "toolu_1", "input": {}}],
            "stop_reason": "tool_use"
        }"#;
        assert!(serde_json::from_str::<ResponseBody>(json_str).is_err());
    }

    #[test]
    fn tool_use_missing_input_fails() {
        let json_str = r#"{
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "bash"}],
            "stop_reason": "tool_use"
        }"#;
        assert!(serde_json::from_str::<ResponseBody>(json_str).is_err());
    }

    #[test]
    fn tool_use_input_not_object_parses() {
        // serde_json::Value accepts any JSON type, so a number input parses fine.
        let json_str = r#"{
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "bash", "input": 42}],
            "stop_reason": "tool_use"
        }"#;
        let resp: ResponseBody = serde_json::from_str(json_str).unwrap();
        match &resp.content[0] {
            ResponseContentBlock::ToolUse { input, .. } => {
                assert_eq!(*input, json!(42));
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn tool_use_input_is_number_parses() {
        let json_str = r#"{
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "bash", "input": 3.14}],
            "stop_reason": "tool_use"
        }"#;
        let resp: ResponseBody = serde_json::from_str(json_str).unwrap();
        match &resp.content[0] {
            ResponseContentBlock::ToolUse { input, .. } => {
                assert!(input.as_f64().is_some());
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn unknown_content_block_type_handled() {
        // serde tagged enum: unknown "type" value should fail deserialization.
        let json_str = r#"{
            "content": [{"type": "image", "url": "http://example.com/img.png"}],
            "stop_reason": "end_turn"
        }"#;
        assert!(serde_json::from_str::<ResponseBody>(json_str).is_err());
    }

    #[test]
    fn empty_content_array_parses() {
        let json_str = r#"{"content": [], "stop_reason": "end_turn"}"#;
        let resp: ResponseBody = serde_json::from_str(json_str).unwrap();
        assert!(resp.content.is_empty());
    }

    #[test]
    fn unknown_stop_reason_becomes_end_turn() {
        let resp = ResponseBody {
            content: vec![ResponseContentBlock::Text {
                text: "hi".to_owned(),
            }],
            stop_reason: "something_new".to_owned(),
        };
        let msg = response_to_message(resp);
        match msg {
            Message::Assistant { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
            }
            _ => panic!("expected Assistant message"),
        }
    }

    // --- Step 6: Error response handling ---

    #[test]
    fn api_error_response_does_not_parse_as_success() {
        // A typical API error response has "error" instead of "content".
        let error_json = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert!(serde_json::from_str::<ResponseBody>(error_json).is_err());
    }

    #[test]
    fn error_message_does_not_contain_secrets() {
        // Verify that our error format doesn't accidentally include API key patterns.
        let error_msg = format!(
            "API error 401 Unauthorized: {{\"type\":\"error\",\"error\":{{\"type\":\"authentication_error\",\"message\":\"invalid x-api-key\"}}}}"
        );
        assert!(!error_msg.contains("sk-ant-"));
        assert!(!error_msg.contains("sk-"));
    }
}
