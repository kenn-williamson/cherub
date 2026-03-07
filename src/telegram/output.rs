use std::sync::Mutex;

use teloxide::prelude::*;
use teloxide::types::{MessageId, ParseMode};

use crate::runtime::output::{OutputEvent, OutputSink};

/// Maximum Telegram message length (API limit).
const MAX_MESSAGE_LEN: usize = 4096;

/// Summary of a tool invocation for batched output.
struct ToolSummary {
    command: String,
    outcome: &'static str,
}

/// Accumulated output for one turn, flushed at `turn_end()`.
struct TurnBatch {
    recapitulation: Option<String>,
    tool_summaries: Vec<ToolSummary>,
    texts: Vec<String>,
    warnings: Vec<String>,
}

impl TurnBatch {
    fn new() -> Self {
        Self {
            recapitulation: None,
            tool_summaries: Vec::new(),
            texts: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// Output sink that sends messages to a Telegram chat.
///
/// When `verbose` is false (default), output is batched per-turn and sent as a
/// single composed message at `turn_end()`. When `verbose` is true, events are
/// sent immediately (legacy behavior).
pub struct TelegramSink {
    bot: Bot,
    chat_id: ChatId,
    verbose: bool,
    batch: Mutex<Option<TurnBatch>>,
    status_message_id: Mutex<Option<MessageId>>,
}

impl TelegramSink {
    pub fn new(bot: Bot, chat_id: ChatId, verbose: bool) -> Self {
        Self {
            bot,
            chat_id,
            verbose,
            batch: Mutex::new(None),
            status_message_id: Mutex::new(None),
        }
    }

    /// Send a message, splitting at newlines if it exceeds the Telegram limit.
    async fn send(&self, text: &str) {
        if text.is_empty() {
            return;
        }

        for chunk in split_message(text) {
            // Try Markdown first, fall back to plain text if parsing fails.
            let result = self
                .bot
                .send_message(self.chat_id, &chunk)
                .parse_mode(ParseMode::MarkdownV2)
                .await;

            if result.is_err() {
                let _ = self.bot.send_message(self.chat_id, &chunk).await;
            }
        }
    }

    /// Send a plain text message without Markdown formatting.
    async fn send_plain(&self, text: &str) {
        if text.is_empty() {
            return;
        }

        for chunk in split_message(text) {
            let _ = self.bot.send_message(self.chat_id, &chunk).await;
        }
    }

    /// Send or edit the status message (progress indicator).
    async fn update_status(&self, text: &str) {
        let existing = { *self.status_message_id.lock().unwrap() };
        if let Some(msg_id) = existing {
            // Edit existing status message.
            let _ = self
                .bot
                .edit_message_text(self.chat_id, msg_id, text)
                .await;
        } else {
            // Send new status message.
            if let Ok(msg) = self.bot.send_message(self.chat_id, text).await {
                *self.status_message_id.lock().unwrap() = Some(msg.id);
            }
        }
    }

    /// Delete the status message if present.
    async fn clear_status(&self) {
        let msg_id = { self.status_message_id.lock().unwrap().take() };
        if let Some(msg_id) = msg_id {
            let _ = self.bot.delete_message(self.chat_id, msg_id).await;
        }
    }

    /// Whether batching is active for this event.
    fn is_batching(&self) -> bool {
        !self.verbose && self.batch.lock().unwrap().is_some()
    }
}

impl OutputSink for TelegramSink {
    async fn emit(&self, event: OutputEvent<'_>) {
        // Progress/ProgressClear always go through status message, regardless of mode.
        match &event {
            OutputEvent::Progress { tool, status } => {
                let text = format!("⏳ {tool}: {status}");
                self.update_status(&text).await;
                return;
            }
            OutputEvent::ProgressClear => {
                self.clear_status().await;
                return;
            }
            _ => {}
        }

        // If batching, accumulate instead of sending.
        if self.is_batching() {
            let mut guard = self.batch.lock().unwrap();
            if let Some(ref mut batch) = *guard {
                match event {
                    OutputEvent::Recapitulation(text) => {
                        batch.recapitulation = Some(text.to_owned());
                    }
                    OutputEvent::Text(text) => {
                        batch.texts.push(text.to_owned());
                    }
                    OutputEvent::ToolAllowed { command, .. } => {
                        batch.tool_summaries.push(ToolSummary {
                            command: command.to_owned(),
                            outcome: "allowed",
                        });
                    }
                    OutputEvent::ToolRejected { command, .. } => {
                        batch.tool_summaries.push(ToolSummary {
                            command: command.to_owned(),
                            outcome: "rejected",
                        });
                    }
                    OutputEvent::ToolApproved { command, .. } => {
                        batch.tool_summaries.push(ToolSummary {
                            command: command.to_owned(),
                            outcome: "approved",
                        });
                    }
                    OutputEvent::ToolDenied { command, .. } => {
                        batch.tool_summaries.push(ToolSummary {
                            command: command.to_owned(),
                            outcome: "denied",
                        });
                    }
                    OutputEvent::Warning(msg) => {
                        batch.warnings.push(msg.to_owned());
                    }
                    OutputEvent::Thinking(_)
                    | OutputEvent::ToolOutput(_)
                    | OutputEvent::ToolError(_) => {
                        // Thinking is discarded; tool output/error are internal details.
                    }
                    OutputEvent::Progress { .. } | OutputEvent::ProgressClear => {
                        unreachable!()
                    }
                }
                return;
            }
        }

        // Verbose mode or no batch active: send immediately.
        match event {
            OutputEvent::Text(text) => self.send_plain(text).await,
            OutputEvent::Recapitulation(text) => {
                // Send as italic in plain mode.
                let msg = format!("_{text}_");
                self.send_plain(&msg).await;
            }
            OutputEvent::ToolAllowed { tool, command } => {
                let msg = format!("[ALLOWED] {tool}: {command}");
                self.send_plain(&msg).await;
            }
            OutputEvent::ToolRejected { tool, command } => {
                let msg = format!("[REJECTED] {tool}: {command}");
                self.send_plain(&msg).await;
            }
            OutputEvent::ToolApproved { tool, command } => {
                let msg = format!("[APPROVED] {tool}: {command}");
                self.send_plain(&msg).await;
            }
            OutputEvent::ToolDenied { tool, command } => {
                let msg = format!("[DENIED] {tool}: {command}");
                self.send_plain(&msg).await;
            }
            OutputEvent::ToolOutput(output) => {
                // Wrap tool output in a code block for readability.
                let msg = format!("```\n{output}\n```");
                self.send(&msg).await;
            }
            OutputEvent::ToolError(err) => {
                let msg = format!("[ERROR] {err}");
                self.send_plain(&msg).await;
            }
            OutputEvent::Warning(msg) => {
                let text = format!("[WARNING] {msg}");
                self.send_plain(&text).await;
            }
            OutputEvent::Thinking(_) => {
                // Thinking is debug-level, not shown to Telegram users.
            }
            OutputEvent::Progress { .. } | OutputEvent::ProgressClear => {
                unreachable!()
            }
        }
    }

    async fn turn_start(&self) {
        if !self.verbose {
            *self.batch.lock().unwrap() = Some(TurnBatch::new());
            self.update_status("Working...").await;
        }
    }

    async fn turn_end(&self) {
        if !self.verbose {
            let batch = { self.batch.lock().unwrap().take() };
            self.clear_status().await;
            if let Some(batch) = batch {
                let msg = compose_turn_message(&batch);
                if !msg.is_empty() {
                    self.send_plain(&msg).await;
                }
            }
        }
    }
}

/// Compose a single turn message from batched output. Pure function.
///
/// Format:
/// ```text
/// _Understanding: [recapitulation]_
///
/// Tools: `cmd1` (allowed), `cmd2` (rejected)
///
/// [final text]
///
/// ⚠️ [warnings]
/// ```
fn compose_turn_message(batch: &TurnBatch) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(ref recap) = batch.recapitulation {
        parts.push(format!("_{recap}_"));
    }

    if !batch.tool_summaries.is_empty() {
        let tools: Vec<String> = batch
            .tool_summaries
            .iter()
            .map(|s| format!("`{}` ({})", s.command, s.outcome))
            .collect();
        parts.push(format!("Tools: {}", tools.join(", ")));
    }

    for text in &batch.texts {
        parts.push(text.clone());
    }

    for w in &batch.warnings {
        parts.push(format!("⚠️ {w}"));
    }

    let full = parts.join("\n\n");

    // Truncate to Telegram limit. Prefer cutting tool details first.
    if full.len() <= MAX_MESSAGE_LEN {
        return full;
    }

    // Rebuild without tool details.
    let mut fallback: Vec<String> = Vec::new();
    if let Some(ref recap) = batch.recapitulation {
        fallback.push(format!("_{recap}_"));
    }
    if !batch.tool_summaries.is_empty() {
        fallback.push(format!("Tools: {} executed", batch.tool_summaries.len()));
    }
    for text in &batch.texts {
        fallback.push(text.clone());
    }
    for w in &batch.warnings {
        fallback.push(format!("⚠️ {w}"));
    }
    let mut result = fallback.join("\n\n");
    if result.len() > MAX_MESSAGE_LEN {
        result.truncate(MAX_MESSAGE_LEN - 3);
        result.push_str("...");
    }
    result
}

/// Split text into chunks that fit within the Telegram message limit.
/// Splits at newline boundaries when possible.
fn split_message(text: &str) -> Vec<String> {
    if text.len() <= MAX_MESSAGE_LEN {
        return vec![text.to_owned()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= MAX_MESSAGE_LEN {
            chunks.push(remaining.to_owned());
            break;
        }

        // Find the last newline within the limit.
        let split_at = remaining[..MAX_MESSAGE_LEN]
            .rfind('\n')
            .map(|pos| pos + 1) // Include the newline in the current chunk
            .unwrap_or(MAX_MESSAGE_LEN); // Hard split if no newline found

        chunks.push(remaining[..split_at].to_owned());
        remaining = &remaining[split_at..];
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_message_not_split() {
        let chunks = split_message("hello");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "hello");
    }

    #[test]
    fn empty_message_not_split() {
        let chunks = split_message("");
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn long_message_splits_at_newline() {
        let line = "x".repeat(2000);
        let text = format!("{line}\n{line}\n{line}");
        let chunks = split_message(&text);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= MAX_MESSAGE_LEN);
        }
    }

    #[test]
    fn compose_empty_batch() {
        let batch = TurnBatch::new();
        assert_eq!(compose_turn_message(&batch), "");
    }

    #[test]
    fn compose_recap_only() {
        let batch = TurnBatch {
            recapitulation: Some("You want to list files".to_owned()),
            ..TurnBatch::new()
        };
        assert_eq!(
            compose_turn_message(&batch),
            "_You want to list files_"
        );
    }

    #[test]
    fn compose_tools_and_text() {
        let batch = TurnBatch {
            recapitulation: None,
            tool_summaries: vec![
                ToolSummary {
                    command: "ls src/".to_owned(),
                    outcome: "allowed",
                },
                ToolSummary {
                    command: "grep pattern".to_owned(),
                    outcome: "allowed",
                },
            ],
            texts: vec!["Here are the results.".to_owned()],
            warnings: Vec::new(),
        };
        let msg = compose_turn_message(&batch);
        assert!(msg.contains("Tools: `ls src/` (allowed), `grep pattern` (allowed)"));
        assert!(msg.contains("Here are the results."));
    }

    #[test]
    fn compose_full_message() {
        let batch = TurnBatch {
            recapitulation: Some("Listing project files".to_owned()),
            tool_summaries: vec![ToolSummary {
                command: "ls".to_owned(),
                outcome: "allowed",
            }],
            texts: vec!["Done.".to_owned()],
            warnings: vec!["Response truncated".to_owned()],
        };
        let msg = compose_turn_message(&batch);
        assert!(msg.contains("_Listing project files_"));
        assert!(msg.contains("Tools: `ls` (allowed)"));
        assert!(msg.contains("Done."));
        assert!(msg.contains("⚠️ Response truncated"));
    }

    #[test]
    fn compose_truncates_at_limit() {
        let long_text = "x".repeat(5000);
        let batch = TurnBatch {
            recapitulation: None,
            tool_summaries: Vec::new(),
            texts: vec![long_text],
            warnings: Vec::new(),
        };
        let msg = compose_turn_message(&batch);
        assert!(msg.len() <= MAX_MESSAGE_LEN);
        assert!(msg.ends_with("..."));
    }

    #[test]
    fn compose_verbose_noop() {
        // Verbose mode doesn't use compose — this just verifies TurnBatch::new works.
        let batch = TurnBatch::new();
        assert!(batch.recapitulation.is_none());
        assert!(batch.tool_summaries.is_empty());
        assert!(batch.texts.is_empty());
        assert!(batch.warnings.is_empty());
    }
}
