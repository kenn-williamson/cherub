//! Private serde structs for OpenAI Chat Completions API JSON format.
//! These map 1:1 to the OpenAI wire format and cover any compatible endpoint
//! (OpenAI, Azure OpenAI, Gemini, Ollama, vLLM, LM Studio, Groq).

use serde::{Deserialize, Serialize};

use super::{ApiUsage, ContentBlock, Message, StopReason, ToolDefinition, UserContent};

// --- Request types ---

#[derive(Serialize)]
pub(crate) struct ChatCompletionRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OaiTool>,
}

#[derive(Serialize, Debug)]
pub(crate) struct OaiMessage {
    pub role: &'static str,
    pub content: OaiContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Content can be a plain string or an array of content parts (for images).
#[derive(Serialize, Debug)]
#[serde(untagged)]
pub(crate) enum OaiContent {
    Text(String),
    Null,
    Parts(Vec<OaiContentPart>),
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
pub(crate) enum OaiContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: OaiImageUrl },
}

#[derive(Serialize, Debug)]
pub(crate) struct OaiImageUrl {
    pub url: String,
}

#[derive(Serialize, Debug)]
pub(crate) struct OaiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: &'static str,
    pub function: OaiFunction,
}

#[derive(Serialize, Debug)]
pub(crate) struct OaiFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize)]
pub(crate) struct OaiTool {
    #[serde(rename = "type")]
    pub tool_type: &'static str,
    pub function: OaiToolFunction,
}

#[derive(Serialize)]
pub(crate) struct OaiToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// --- Response types ---

#[derive(Deserialize)]
pub(crate) struct ChatCompletionResponse {
    pub choices: Vec<OaiChoice>,
    pub usage: Option<OaiUsage>,
}

#[derive(Deserialize)]
pub(crate) struct OaiChoice {
    pub message: OaiResponseMessage,
    pub finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct OaiResponseMessage {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<OaiResponseToolCall>>,
}

#[derive(Deserialize)]
pub(crate) struct OaiResponseToolCall {
    pub id: String,
    pub function: OaiResponseFunction,
}

#[derive(Deserialize)]
pub(crate) struct OaiResponseFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Deserialize, Clone, Copy)]
pub(crate) struct OaiUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

// --- Conversions ---

impl From<&ToolDefinition> for OaiTool {
    fn from(def: &ToolDefinition) -> Self {
        Self {
            tool_type: "function",
            function: OaiToolFunction {
                name: def.name.clone(),
                description: def.description.clone(),
                parameters: def.input_schema.clone(),
            },
        }
    }
}

/// Convert internal messages to OpenAI wire format.
/// System prompt is prepended as a `{"role": "system"}` message.
/// Tool results become individual `{"role": "tool"}` messages (no merging needed).
pub(crate) fn messages_to_openai_wire(system: &str, messages: &[Message]) -> Vec<OaiMessage> {
    let mut wire = Vec::with_capacity(messages.len() + 1);

    // System prompt is a system message in the OpenAI format.
    wire.push(OaiMessage {
        role: "system",
        content: OaiContent::Text(system.to_owned()),
        tool_calls: None,
        tool_call_id: None,
    });

    for msg in messages {
        match msg {
            Message::User { content } => {
                wire.push(OaiMessage {
                    role: "user",
                    content: user_content_to_oai(content),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            Message::Assistant { content, .. } => {
                let (text_parts, tool_calls) = assistant_content_to_oai(content);
                wire.push(OaiMessage {
                    role: "assistant",
                    content: text_parts,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                });
            }
            Message::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                wire.push(OaiMessage {
                    role: "tool",
                    content: OaiContent::Text(content.clone()),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id.clone()),
                });
            }
        }
    }

    wire
}

/// Convert user content items to OpenAI format.
fn user_content_to_oai(content: &[UserContent]) -> OaiContent {
    // Single text → compact string format
    if content.len() == 1
        && let UserContent::Text(text) = &content[0]
    {
        return OaiContent::Text(text.clone());
    }

    let parts = content
        .iter()
        .map(|c| match c {
            UserContent::Text(text) => OaiContentPart::Text { text: text.clone() },
            UserContent::Image { media_type, data } => OaiContentPart::ImageUrl {
                image_url: OaiImageUrl {
                    url: format!("data:{media_type};base64,{data}"),
                },
            },
        })
        .collect();

    OaiContent::Parts(parts)
}

/// Split assistant content blocks into text content and tool calls.
fn assistant_content_to_oai(content: &[ContentBlock]) -> (OaiContent, Vec<OaiToolCall>) {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in content {
        match block {
            ContentBlock::Text { text } => {
                text_parts.push(text.clone());
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(OaiToolCall {
                    id: id.clone(),
                    call_type: "function",
                    function: OaiFunction {
                        name: name.clone(),
                        arguments: serde_json::to_string(input).unwrap_or_default(),
                    },
                });
            }
        }
    }

    let content = if text_parts.is_empty() {
        OaiContent::Null
    } else {
        OaiContent::Text(text_parts.join("\n"))
    };

    (content, tool_calls)
}

/// Convert an OpenAI response to our internal Message type, plus optional API usage.
pub(crate) fn openai_response_to_message(
    resp: ChatCompletionResponse,
) -> (Message, Option<ApiUsage>) {
    let usage = resp
        .usage
        .map(|u| ApiUsage::new(u.prompt_tokens, u.completion_tokens));

    let choice = match resp.choices.into_iter().next() {
        Some(c) => c,
        None => {
            return (
                Message::Assistant {
                    content: vec![],
                    stop_reason: StopReason::EndTurn,
                },
                usage,
            );
        }
    };

    let finish_reason = choice.finish_reason.as_deref().unwrap_or("stop");
    let stop_reason = match finish_reason {
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    };

    let mut content = Vec::new();

    // Add text content if present.
    if let Some(text) = choice.message.content
        && !text.is_empty()
    {
        content.push(ContentBlock::Text { text });
    }

    // Add tool calls if present.
    if let Some(tool_calls) = choice.message.tool_calls {
        for tc in tool_calls {
            let input: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);
            content.push(ContentBlock::ToolUse {
                id: tc.id,
                name: tc.function.name,
                input,
            });
        }
    }

    let message = Message::Assistant {
        content,
        stop_reason,
    };

    (message, usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn system_message_prepended() {
        let wire = messages_to_openai_wire("You are helpful.", &[]);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "system");
        let json = serde_json::to_value(&wire[0]).unwrap();
        assert_eq!(json["content"], "You are helpful.");
    }

    #[test]
    fn user_text_message_serializes() {
        let wire = messages_to_openai_wire("sys", &[Message::user_text("hello")]);
        assert_eq!(wire.len(), 2); // system + user
        assert_eq!(wire[1].role, "user");
        let json = serde_json::to_value(&wire[1]).unwrap();
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn user_image_message_serializes() {
        let wire = messages_to_openai_wire(
            "sys",
            &[Message::User {
                content: vec![UserContent::Image {
                    media_type: "image/png".to_owned(),
                    data: "iVBOR...".to_owned(),
                }],
            }],
        );
        let json = serde_json::to_value(&wire[1]).unwrap();
        let parts = json["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(
            parts[0]["image_url"]["url"],
            "data:image/png;base64,iVBOR..."
        );
    }

    #[test]
    fn user_mixed_content_serializes() {
        let wire = messages_to_openai_wire(
            "sys",
            &[Message::User {
                content: vec![
                    UserContent::Text("What is this?".to_owned()),
                    UserContent::Image {
                        media_type: "image/jpeg".to_owned(),
                        data: "/9j/4AAQ...".to_owned(),
                    },
                ],
            }],
        );
        let json = serde_json::to_value(&wire[1]).unwrap();
        let parts = json["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "What is this?");
        assert_eq!(parts[1]["type"], "image_url");
    }

    #[test]
    fn assistant_tool_calls_serialized_with_stringified_arguments() {
        let wire = messages_to_openai_wire(
            "sys",
            &[Message::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "call_123".to_owned(),
                    name: "bash".to_owned(),
                    input: json!({"command": "ls /tmp"}),
                }],
                stop_reason: StopReason::ToolUse,
            }],
        );
        let json = serde_json::to_value(&wire[1]).unwrap();
        // content should be null when only tool_calls present
        assert!(json["content"].is_null());
        let tool_calls = json["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_123");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "bash");
        // arguments is a JSON string, not an object
        let args = tool_calls[0]["function"]["arguments"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["command"], "ls /tmp");
    }

    #[test]
    fn tool_result_as_role_tool() {
        let wire = messages_to_openai_wire(
            "sys",
            &[Message::ToolResult {
                tool_use_id: "call_123".to_owned(),
                content: "file1.txt\nfile2.txt".to_owned(),
                is_error: false,
            }],
        );
        let json = serde_json::to_value(&wire[1]).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_123");
        assert_eq!(json["content"], "file1.txt\nfile2.txt");
    }

    #[test]
    fn response_parsing_text_only() {
        let json_str = r#"{
            "choices": [{
                "message": {"content": "Hello!", "tool_calls": null},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json_str).unwrap();
        let (msg, usage) = openai_response_to_message(resp);
        let usage = usage.expect("usage should be present");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.cache_creation_tokens, 0);
        assert_eq!(usage.cache_read_tokens, 0);
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
    fn response_parsing_tool_calls() {
        let json_str = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"pwd\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 10}
        }"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json_str).unwrap();
        let (msg, _) = openai_response_to_message(resp);
        match msg {
            Message::Assistant {
                content,
                stop_reason,
            } => {
                assert_eq!(stop_reason, StopReason::ToolUse);
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentBlock::ToolUse { id, name, input } => {
                        assert_eq!(id, "call_abc");
                        assert_eq!(name, "bash");
                        assert_eq!(input["command"], "pwd");
                    }
                    _ => panic!("expected ToolUse block"),
                }
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn response_finish_reason_length() {
        let json_str = r#"{
            "choices": [{
                "message": {"content": "truncated...", "tool_calls": null},
                "finish_reason": "length"
            }]
        }"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json_str).unwrap();
        let (msg, usage) = openai_response_to_message(resp);
        assert!(usage.is_none());
        match msg {
            Message::Assistant { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::MaxTokens);
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn response_null_content_with_tool_calls() {
        let json_str = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "test", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        }"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json_str).unwrap();
        let (msg, _) = openai_response_to_message(resp);
        match msg {
            Message::Assistant { content, .. } => {
                // Should only have the tool use, no empty text block
                assert_eq!(content.len(), 1);
                assert!(matches!(&content[0], ContentBlock::ToolUse { .. }));
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn response_empty_choices() {
        let json_str = r#"{"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 0}}"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json_str).unwrap();
        let (msg, _) = openai_response_to_message(resp);
        match msg {
            Message::Assistant {
                content,
                stop_reason,
            } => {
                assert!(content.is_empty());
                assert_eq!(stop_reason, StopReason::EndTurn);
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn malformed_json_fails() {
        let truncated = r#"{"choices": [{"message"#;
        assert!(serde_json::from_str::<ChatCompletionResponse>(truncated).is_err());
    }

    #[test]
    fn missing_choices_field_fails() {
        let json_str = r#"{"usage": {"prompt_tokens": 1, "completion_tokens": 0}}"#;
        assert!(serde_json::from_str::<ChatCompletionResponse>(json_str).is_err());
    }

    #[test]
    fn malformed_tool_arguments_become_null() {
        let json_str = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_bad",
                        "type": "function",
                        "function": {"name": "test", "arguments": "not valid json!!!"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json_str).unwrap();
        let (msg, _) = openai_response_to_message(resp);
        match msg {
            Message::Assistant { content, .. } => match &content[0] {
                ContentBlock::ToolUse { input, .. } => {
                    assert!(input.is_null());
                }
                _ => panic!("expected ToolUse"),
            },
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn tool_definition_to_oai_tool() {
        let def = ToolDefinition {
            name: "bash".to_owned(),
            description: "Execute commands".to_owned(),
            input_schema: json!({"type": "object", "properties": {"cmd": {"type": "string"}}}),
        };
        let tool = OaiTool::from(&def);
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["type"], "function");
        assert_eq!(json["function"]["name"], "bash");
        assert_eq!(json["function"]["description"], "Execute commands");
        assert!(json["function"]["parameters"]["properties"]["cmd"].is_object());
    }

    #[test]
    fn request_body_structure() {
        let messages = vec![Message::user_text("hello")];
        let tools = vec![ToolDefinition {
            name: "bash".to_owned(),
            description: "Run bash".to_owned(),
            input_schema: json!({"type": "object"}),
        }];

        let wire_messages = messages_to_openai_wire("system prompt", &messages);
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
        // First message is system
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][0]["content"], "system prompt");
        // Second message is user
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][1]["content"], "hello");
        // Tools
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "bash");
    }
}
