use teloxide::prelude::*;
use teloxide::types::ParseMode;

use crate::runtime::output::{OutputEvent, OutputSink};

/// Maximum Telegram message length (API limit).
const MAX_MESSAGE_LEN: usize = 4096;

/// Output sink that sends messages to a Telegram chat.
pub struct TelegramSink {
    bot: Bot,
    chat_id: ChatId,
}

impl TelegramSink {
    pub fn new(bot: Bot, chat_id: ChatId) -> Self {
        Self { bot, chat_id }
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
}

impl OutputSink for TelegramSink {
    async fn emit(&self, event: OutputEvent<'_>) {
        match event {
            OutputEvent::Text(text) => self.send_plain(text).await,
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
        }
    }
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
}
