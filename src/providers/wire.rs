//! Private serde structs for Anthropic API JSON format.
//! These map 1:1 to the Anthropic Messages API wire format.

use serde::{Deserialize, Serialize};

use super::{
    ApiUsage, ContentBlock, Message, StopReason, ToolDefinition, ToolResultImage, UserContent,
};

// --- Request types ---

#[derive(Serialize)]
pub(crate) struct ThinkingConfig {
    #[serde(rename = "type")]
    pub config_type: &'static str,
    pub budget_tokens: u32,
}

#[derive(Serialize)]
#[serde(tag = "type")]
pub(crate) enum CacheControl {
    #[serde(rename = "ephemeral")]
    Ephemeral,
}

/// System prompt as a list of text blocks. Allows attaching cache_control to
/// the last block so the system prompt + preceding tools are cached together.
#[derive(Serialize)]
pub(crate) struct SystemBlock<'a> {
    #[serde(rename = "type")]
    pub block_type: &'static str,
    pub text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
pub(crate) struct RequestBody<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub system: Vec<SystemBlock<'a>>,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
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
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        /// Either a plain string (text-only) or an array of content blocks (text + images).
        /// The Anthropic API accepts both formats for tool_result content.
        content: WireToolResultContent,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "image")]
    Image {
        source: WireImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

impl WireContentBlock {
    /// Mark this content block as a cache breakpoint, if it supports it.
    /// Thinking blocks don't support cache_control — they are no-ops.
    pub(crate) fn mark_cache(&mut self) {
        match self {
            Self::Text { cache_control, .. }
            | Self::ToolUse { cache_control, .. }
            | Self::ToolResult { cache_control, .. }
            | Self::Image { cache_control, .. } => {
                *cache_control = Some(CacheControl::Ephemeral);
            }
            Self::Thinking { .. } | Self::RedactedThinking { .. } => {}
        }
    }
}

#[derive(Serialize)]
pub(crate) struct WireImageSource {
    #[serde(rename = "type")]
    pub source_type: &'static str,
    pub media_type: String,
    pub data: String,
}

/// Content of a `tool_result` block. Either a plain string or an array of content blocks.
/// The Anthropic API accepts both: `"content": "text"` or `"content": [{...}, ...]`.
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum WireToolResultContent {
    Text(String),
    Blocks(Vec<WireToolResultBlock>),
}

/// A content block inside a multi-block tool result (text or image).
#[derive(Serialize)]
#[serde(tag = "type")]
pub(crate) enum WireToolResultBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: WireImageSource },
}

#[derive(Serialize)]
pub(crate) struct WireTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

// --- Response types ---

#[derive(Deserialize)]
pub(crate) struct ResponseBody {
    pub content: Vec<ResponseContentBlock>,
    pub stop_reason: String,
    pub usage: Option<WireUsage>,
}

#[derive(Deserialize, Clone, Copy)]
pub(crate) struct WireUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
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
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

// --- Streaming (SSE) types ---
//
// The Anthropic streaming API emits server-sent events as `event: <type>` /
// `data: <json>` line pairs separated by blank lines. The `data` JSON always
// carries its own `type` field (redundant with the `event:` line), so the
// parser ignores `event:` lines and deserializes each `data:` payload straight
// into `StreamEvent`. Events are accumulated into a `ResponseBody` identical to
// what the non-streaming endpoint would have returned in one shot.

/// A single streaming event, deserialized from a `data:` payload.
#[derive(Deserialize)]
#[serde(tag = "type")]
pub(crate) enum StreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: StreamMessageStart },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: StreamBlockStart,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: StreamDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop,
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: StreamMessageDelta,
        // `usage` is a top-level sibling of `delta` in this event, not nested.
        #[serde(default)]
        usage: Option<StreamDeltaUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: StreamError },
}

#[derive(Deserialize)]
pub(crate) struct StreamMessageStart {
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub(crate) enum StreamBlockStart {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub(crate) enum StreamDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    // Signature deltas accompany thinking blocks but aren't surfaced in
    // ResponseContentBlock (the non-streaming path drops them too) — ignored.
    #[serde(rename = "signature_delta")]
    Signature {},
}

#[derive(Deserialize)]
pub(crate) struct StreamMessageDelta {
    #[serde(default)]
    pub stop_reason: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct StreamDeltaUsage {
    #[serde(default)]
    pub output_tokens: u32,
}

#[derive(Deserialize)]
pub(crate) struct StreamError {
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(default)]
    pub message: String,
}

/// A failure while consuming the stream. `retryable` distinguishes transient
/// problems (overloaded, dropped mid-stream) from fatal ones (malformed data).
pub(crate) struct StreamFailure {
    pub retryable: bool,
    pub message: String,
}

/// In-progress content block, accumulated from deltas.
enum BlockBuilder {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    Thinking(String),
    RedactedThinking(String),
}

/// Accumulates streaming events into a complete `ResponseBody`.
#[derive(Default)]
pub(crate) struct StreamAccumulator {
    blocks: Vec<BlockBuilder>,
    stop_reason: Option<String>,
    input_tokens: u32,
    output_tokens: u32,
    cache_creation_tokens: u32,
    cache_read_tokens: u32,
}

impl StreamAccumulator {
    /// Apply one event. Returns `Err` only for an `error` event.
    pub(crate) fn apply(&mut self, event: StreamEvent) -> Result<(), StreamFailure> {
        match event {
            StreamEvent::MessageStart { message } => {
                if let Some(u) = message.usage {
                    self.input_tokens = u.input_tokens;
                    self.output_tokens = u.output_tokens;
                    self.cache_creation_tokens = u.cache_creation_input_tokens;
                    self.cache_read_tokens = u.cache_read_input_tokens;
                }
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let builder = match content_block {
                    StreamBlockStart::Text { text } => BlockBuilder::Text(text),
                    StreamBlockStart::ToolUse { id, name } => BlockBuilder::ToolUse {
                        id,
                        name,
                        json: String::new(),
                    },
                    StreamBlockStart::Thinking { thinking } => BlockBuilder::Thinking(thinking),
                    StreamBlockStart::RedactedThinking { data } => {
                        BlockBuilder::RedactedThinking(data)
                    }
                };
                // Indices arrive in order; fill any gap defensively.
                while self.blocks.len() <= index {
                    self.blocks.push(BlockBuilder::Text(String::new()));
                }
                self.blocks[index] = builder;
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(block) = self.blocks.get_mut(index) {
                    match (block, delta) {
                        (BlockBuilder::Text(s), StreamDelta::Text { text }) => {
                            s.push_str(&text);
                        }
                        (
                            BlockBuilder::ToolUse { json, .. },
                            StreamDelta::InputJson { partial_json },
                        ) => {
                            json.push_str(&partial_json);
                        }
                        (BlockBuilder::Thinking(s), StreamDelta::Thinking { thinking }) => {
                            s.push_str(&thinking);
                        }
                        // Mismatched delta/block or signature_delta — ignore.
                        _ => {}
                    }
                }
            }
            StreamEvent::MessageDelta { delta, usage } => {
                if let Some(sr) = delta.stop_reason {
                    self.stop_reason = Some(sr);
                }
                if let Some(u) = usage {
                    self.output_tokens = u.output_tokens;
                }
            }
            StreamEvent::ContentBlockStop | StreamEvent::MessageStop | StreamEvent::Ping => {}
            StreamEvent::Error { error } => {
                let retryable =
                    matches!(error.error_type.as_str(), "overloaded_error" | "api_error");
                return Err(StreamFailure {
                    retryable,
                    message: format!("{}: {}", error.error_type, error.message),
                });
            }
        }
        Ok(())
    }

    /// Assemble the accumulated blocks into a `ResponseBody`.
    pub(crate) fn finish(self) -> ResponseBody {
        let content = self
            .blocks
            .into_iter()
            .map(|b| match b {
                BlockBuilder::Text(text) => ResponseContentBlock::Text { text },
                BlockBuilder::ToolUse { id, name, json } => {
                    // Empty input arrives as no deltas; treat as `{}`.
                    let input = if json.trim().is_empty() {
                        serde_json::Value::Object(serde_json::Map::new())
                    } else {
                        serde_json::from_str(&json)
                            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
                    };
                    ResponseContentBlock::ToolUse { id, name, input }
                }
                BlockBuilder::Thinking(thinking) => ResponseContentBlock::Thinking { thinking },
                BlockBuilder::RedactedThinking(data) => {
                    ResponseContentBlock::RedactedThinking { data }
                }
            })
            .collect();

        ResponseBody {
            content,
            stop_reason: self.stop_reason.unwrap_or_else(|| "end_turn".to_owned()),
            usage: Some(WireUsage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cache_creation_input_tokens: self.cache_creation_tokens,
                cache_read_input_tokens: self.cache_read_tokens,
            }),
        }
    }
}

/// Feed a chunk of raw SSE bytes into the accumulator. `buf` holds bytes not yet
/// forming a complete line across calls; `data_lines` holds `data:` payloads of
/// the event currently being assembled. Complete events are applied immediately.
pub(crate) fn feed_sse(
    acc: &mut StreamAccumulator,
    buf: &mut Vec<u8>,
    data_lines: &mut Vec<String>,
    chunk: &[u8],
) -> Result<(), StreamFailure> {
    buf.extend_from_slice(chunk);
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let raw: Vec<u8> = buf.drain(..=pos).collect();
        // Drop the trailing '\n' and an optional preceding '\r'.
        let mut line = &raw[..raw.len() - 1];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() {
            // Blank line: end of an event. Dispatch if we collected data.
            if !data_lines.is_empty() {
                let payload = data_lines.join("\n");
                data_lines.clear();
                let event: StreamEvent =
                    serde_json::from_str(&payload).map_err(|e| StreamFailure {
                        retryable: false,
                        message: format!("SSE JSON parse error: {e}"),
                    })?;
                acc.apply(event)?;
            }
        } else if let Some(rest) = line.strip_prefix(b"data:") {
            let rest = rest.strip_prefix(b" ").unwrap_or(rest);
            let text = std::str::from_utf8(rest).map_err(|_| StreamFailure {
                retryable: false,
                message: "invalid UTF-8 in SSE data".to_owned(),
            })?;
            data_lines.push(text.to_owned());
        }
        // `event:` / `id:` / comment lines are ignored.
    }
    Ok(())
}

// --- Conversions ---

impl From<&ToolDefinition> for WireTool {
    fn from(def: &ToolDefinition) -> Self {
        Self {
            name: def.name.clone(),
            description: def.description.clone(),
            input_schema: def.input_schema.clone(),
            cache_control: None,
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
            UserContent::Text(text) => WireContentBlock::Text {
                text: text.clone(),
                cache_control: None,
            },
            UserContent::Image { media_type, data } => WireContentBlock::Image {
                source: WireImageSource {
                    source_type: "base64",
                    media_type: media_type.clone(),
                    data: data.clone(),
                },
                cache_control: None,
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
                        ContentBlock::Text { text } => WireContentBlock::Text {
                            text: text.clone(),
                            cache_control: None,
                        },
                        ContentBlock::ToolUse { id, name, input } => WireContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                            cache_control: None,
                        },
                        ContentBlock::Thinking { thinking } => WireContentBlock::Thinking {
                            thinking: thinking.clone(),
                        },
                        ContentBlock::RedactedThinking { data } => {
                            WireContentBlock::RedactedThinking { data: data.clone() }
                        }
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
                images,
                is_error,
            } => {
                let wire_content = if images.is_empty() {
                    WireToolResultContent::Text(content.clone())
                } else {
                    tool_result_content_to_wire(content, images)
                };
                pending_results.push(WireContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: wire_content,
                    is_error: *is_error,
                    cache_control: None,
                });
            }
        }
    }

    flush_results(&mut wire, &mut pending_results);
    wire
}

/// Mark the last content block of the last message as a cache breakpoint.
/// This caches the entire conversation prefix on each turn — subsequent turns
/// read from this point and only pay full price for new content.
pub(crate) fn mark_conversation_cache(messages: &mut [WireMessage]) {
    let Some(last_msg) = messages.last_mut() else {
        return;
    };
    match &mut last_msg.content {
        WireContent::Text(_) => {
            // Text-only content can't carry cache_control; convert to a single-block form.
            let text = std::mem::take(&mut last_msg.content);
            let WireContent::Text(s) = text else {
                unreachable!()
            };
            last_msg.content = WireContent::Blocks(vec![WireContentBlock::Text {
                text: s,
                cache_control: Some(CacheControl::Ephemeral),
            }]);
        }
        WireContent::Blocks(blocks) => {
            // Find the last cacheable block (skip Thinking which can't be marked).
            for block in blocks.iter_mut().rev() {
                if !matches!(
                    block,
                    WireContentBlock::Thinking { .. } | WireContentBlock::RedactedThinking { .. }
                ) {
                    block.mark_cache();
                    break;
                }
            }
        }
    }
}

impl Default for WireContent {
    fn default() -> Self {
        WireContent::Blocks(Vec::new())
    }
}

/// Build multi-block tool result content (text + images) for the Anthropic wire format.
fn tool_result_content_to_wire(text: &str, images: &[ToolResultImage]) -> WireToolResultContent {
    let mut blocks = Vec::with_capacity(1 + images.len());
    if !text.is_empty() {
        blocks.push(WireToolResultBlock::Text {
            text: text.to_owned(),
        });
    }
    for img in images {
        blocks.push(WireToolResultBlock::Image {
            source: WireImageSource {
                source_type: "base64",
                media_type: img.media_type.clone(),
                data: img.data.clone(),
            },
        });
    }
    WireToolResultContent::Blocks(blocks)
}

fn flush_results(wire: &mut Vec<WireMessage>, pending: &mut Vec<WireContentBlock>) {
    if !pending.is_empty() {
        wire.push(WireMessage {
            role: "user",
            content: WireContent::Blocks(std::mem::take(pending)),
        });
    }
}

/// Convert a wire response to our internal Message type, plus optional API usage.
pub(crate) fn response_to_message(resp: ResponseBody) -> (Message, Option<ApiUsage>) {
    let stop_reason = match resp.stop_reason.as_str() {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    };

    let usage = resp.usage.map(|u| ApiUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_tokens: u.cache_creation_input_tokens,
        cache_read_tokens: u.cache_read_input_tokens,
    });

    let content = resp
        .content
        .into_iter()
        .map(|block| match block {
            ResponseContentBlock::Text { text } => ContentBlock::Text { text },
            ResponseContentBlock::ToolUse { id, name, input } => {
                ContentBlock::ToolUse { id, name, input }
            }
            ResponseContentBlock::Thinking { thinking } => ContentBlock::Thinking { thinking },
            ResponseContentBlock::RedactedThinking { data } => {
                ContentBlock::RedactedThinking { data }
            }
        })
        .collect();

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
                images: vec![],
                is_error: false,
            },
            Message::ToolResult {
                tool_use_id: "id2".to_owned(),
                content: "output2".to_owned(),
                images: vec![],
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
            system: vec![SystemBlock {
                block_type: "text",
                text: "You are helpful.",
                cache_control: None,
            }],
            messages: vec![WireMessage {
                role: "user",
                content: WireContent::Text("hi".to_owned()),
            }],
            tools: vec![WireTool {
                name: "bash".to_owned(),
                description: "Run bash".to_owned(),
                input_schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
                cache_control: None,
            }],
            stream: false,
            thinking: None,
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
            usage: None,
        };
        let (msg, usage) = response_to_message(resp);
        assert!(usage.is_none());
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
            usage: None,
        };
        let (msg, _) = response_to_message(resp);
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
            usage: None,
        };
        let (msg, _) = response_to_message(resp);
        match msg {
            Message::Assistant { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn usage_parsed_when_present() {
        let json_str = r#"{
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 150, "output_tokens": 42}
        }"#;
        let resp: ResponseBody = serde_json::from_str(json_str).unwrap();
        let (_, usage) = response_to_message(resp);
        let usage = usage.expect("usage should be present");
        assert_eq!(usage.input_tokens, 150);
        assert_eq!(usage.output_tokens, 42);
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
        let error_msg = "API error 401 Unauthorized: {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"invalid x-api-key\"}}".to_string();
        assert!(!error_msg.contains("sk-ant-"));
        assert!(!error_msg.contains("sk-"));
    }

    // --- Cache token deserialization ---

    #[test]
    fn usage_with_cache_tokens_parsed() {
        let json_str = r#"{
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_creation_input_tokens": 2000,
                "cache_read_input_tokens": 5000
            }
        }"#;
        let resp: ResponseBody = serde_json::from_str(json_str).unwrap();
        let (_, usage) = response_to_message(resp);
        let usage = usage.expect("usage should be present");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_creation_tokens, 2000);
        assert_eq!(usage.cache_read_tokens, 5000);
    }

    #[test]
    fn response_parsing_thinking_blocks() {
        let json_str = r#"{
            "content": [
                {"type": "thinking", "thinking": "Let me work through this..."},
                {"type": "text", "text": "The answer is 42."}
            ],
            "stop_reason": "end_turn"
        }"#;
        let resp: ResponseBody = serde_json::from_str(json_str).unwrap();
        assert_eq!(resp.content.len(), 2);
        let (msg, _) = response_to_message(resp);
        match msg {
            Message::Assistant { content, .. } => {
                assert_eq!(content.len(), 2);
                match &content[0] {
                    ContentBlock::Thinking { thinking } => {
                        assert_eq!(thinking, "Let me work through this...");
                    }
                    _ => panic!("expected Thinking block"),
                }
                match &content[1] {
                    ContentBlock::Text { text } => assert_eq!(text, "The answer is 42."),
                    _ => panic!("expected Text block"),
                }
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn response_parsing_redacted_thinking() {
        let json_str = r#"{
            "content": [
                {"type": "redacted_thinking", "data": "c29tZSBiYXNlNjQ="},
                {"type": "text", "text": "Done."}
            ],
            "stop_reason": "end_turn"
        }"#;
        let resp: ResponseBody = serde_json::from_str(json_str).unwrap();
        let (msg, _) = response_to_message(resp);
        match msg {
            Message::Assistant { content, .. } => {
                assert_eq!(content.len(), 2);
                match &content[0] {
                    ContentBlock::RedactedThinking { data } => {
                        assert_eq!(data, "c29tZSBiYXNlNjQ=");
                    }
                    _ => panic!("expected RedactedThinking block"),
                }
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn thinking_blocks_round_trip_through_wire() {
        // Build a message with thinking blocks, serialize to wire, verify format.
        let messages = vec![Message::Assistant {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm...".to_owned(),
                },
                ContentBlock::RedactedThinking {
                    data: "abc123".to_owned(),
                },
                ContentBlock::Text {
                    text: "answer".to_owned(),
                },
            ],
            stop_reason: StopReason::EndTurn,
        }];
        let wire = messages_to_wire(&messages);
        assert_eq!(wire.len(), 1);
        let json = serde_json::to_value(&wire[0]).unwrap();
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "hmm...");
        assert_eq!(blocks[1]["type"], "redacted_thinking");
        assert_eq!(blocks[1]["data"], "abc123");
        assert_eq!(blocks[2]["type"], "text");
        assert_eq!(blocks[2]["text"], "answer");
    }

    #[test]
    fn request_body_with_thinking_config() {
        let body = RequestBody {
            model: "claude-sonnet-4-20250514",
            max_tokens: 16000,
            system: vec![SystemBlock {
                block_type: "text",
                text: "test",
                cache_control: None,
            }],
            messages: vec![],
            tools: vec![],
            stream: false,
            thinking: Some(ThinkingConfig {
                config_type: "enabled",
                budget_tokens: 4000,
            }),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["thinking"]["type"], "enabled");
        assert_eq!(json["thinking"]["budget_tokens"], 4000);
    }

    #[test]
    fn request_body_without_thinking_config() {
        let body = RequestBody {
            model: "claude-sonnet-4-20250514",
            max_tokens: 4096,
            system: vec![SystemBlock {
                block_type: "text",
                text: "test",
                cache_control: None,
            }],
            messages: vec![],
            tools: vec![],
            stream: false,
            thinking: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn usage_without_cache_tokens_defaults_to_zero() {
        let json_str = r#"{
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        }"#;
        let resp: ResponseBody = serde_json::from_str(json_str).unwrap();
        let (_, usage) = response_to_message(resp);
        let usage = usage.expect("usage should be present");
        assert_eq!(usage.cache_creation_tokens, 0);
        assert_eq!(usage.cache_read_tokens, 0);
    }

    /// Drive the SSE parser with `chunks` and assemble the result.
    fn run_sse(chunks: &[&[u8]]) -> Result<ResponseBody, String> {
        let mut acc = StreamAccumulator::default();
        let mut buf = Vec::new();
        let mut data_lines = Vec::new();
        for chunk in chunks {
            feed_sse(&mut acc, &mut buf, &mut data_lines, chunk).map_err(|f| f.message)?;
        }
        Ok(acc.finish())
    }

    #[test]
    fn sse_assembles_text_with_usage() {
        let sse = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":1,\"cache_read_input_tokens\":80}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let resp = run_sse(&[sse]).expect("stream should assemble");
        let (msg, usage) = response_to_message(resp);
        match msg {
            Message::Assistant {
                content,
                stop_reason,
            } => {
                assert!(matches!(stop_reason, StopReason::EndTurn));
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
                    other => panic!("expected text, got {other:?}"),
                }
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        let usage = usage.expect("usage present");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cache_read_tokens, 80);
    }

    #[test]
    fn sse_assembles_tool_use_from_partial_json() {
        // tool_use input arrives as input_json_delta fragments that must concatenate.
        let sse = b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.txt\\\"}\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n";
        let resp = run_sse(&[sse]).expect("stream should assemble");
        let (msg, _) = response_to_message(resp);
        let Message::Assistant {
            content,
            stop_reason,
        } = msg
        else {
            panic!("expected assistant");
        };
        assert!(matches!(stop_reason, StopReason::ToolUse));
        match &content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "read");
                assert_eq!(input["path"], "a.txt");
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn sse_survives_event_split_across_chunks() {
        // Split mid-line to prove bytes are buffered until a line is complete
        // (UTF-8 is only decoded on whole lines, so a multibyte char that lands
        // before the newline is reassembled intact regardless of chunk boundary).
        let full = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"caf\u{00e9}\"}}\n\n";
        let bytes = full.as_bytes();
        // Split at an arbitrary boundary partway through the first event's line.
        let (a, b) = bytes.split_at(40);
        let resp = run_sse(&[a, b]).expect("stream should assemble across chunks");
        let (msg, _) = response_to_message(resp);
        let Message::Assistant { content, .. } = msg else {
            panic!("expected assistant");
        };
        match &content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "caf\u{00e9}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn sse_overloaded_error_is_retryable() {
        let sse = b"data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let mut acc = StreamAccumulator::default();
        let mut buf = Vec::new();
        let mut data_lines = Vec::new();
        let err = feed_sse(&mut acc, &mut buf, &mut data_lines, sse)
            .expect_err("error event should surface");
        assert!(err.retryable);
    }

    #[test]
    fn sse_malformed_json_is_fatal() {
        let sse = b"data: {not valid json}\n\n";
        let mut acc = StreamAccumulator::default();
        let mut buf = Vec::new();
        let mut data_lines = Vec::new();
        let err = feed_sse(&mut acc, &mut buf, &mut data_lines, sse)
            .expect_err("malformed data should fail");
        assert!(!err.retryable);
    }
}
