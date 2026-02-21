use uuid::Uuid;

use crate::providers::Message;
#[cfg(feature = "sessions")]
use crate::storage::SessionStore;

/// In-memory conversation history with optional PostgreSQL persistence.
///
/// Session owns its store — AgentLoop stays at 3 type params forever.
/// Persistence is Session's concern, not AgentLoop's.
pub struct Session {
    pub(crate) id: Uuid,
    pub(crate) user_id: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) next_ordinal: i32,
    /// How many times this session has been compacted.
    pub(crate) compaction_count: u32,
    #[cfg(feature = "sessions")]
    store: Option<Box<dyn SessionStore>>,
}

impl Session {
    /// Create an ephemeral session (no persistence).
    // No Default impl — Default as constructor substitute is prohibited (see CLAUDE.md).
    pub fn new(user_id: &str) -> Self {
        Self {
            id: Uuid::now_v7(),
            user_id: user_id.to_owned(),
            messages: Vec::new(),
            next_ordinal: 0,
            compaction_count: 0,
            #[cfg(feature = "sessions")]
            store: None,
        }
    }

    /// Restore a session from persisted state with an attached store.
    #[cfg(feature = "sessions")]
    pub fn from_persisted(
        id: Uuid,
        messages: Vec<Message>,
        user_id: String,
        store: Box<dyn SessionStore>,
    ) -> Self {
        let next_ordinal = messages.len() as i32;
        Self {
            id,
            user_id,
            messages,
            next_ordinal,
            compaction_count: 0,
            store: Some(store),
        }
    }

    /// Append a message to the session and return its ordinal.
    pub fn push(&mut self, message: Message) -> i32 {
        let ordinal = self.next_ordinal;
        self.messages.push(message);
        self.next_ordinal += 1;
        ordinal
    }

    /// Persist the last pushed message to the store (non-fatal: logs warning on failure).
    /// Called by AgentLoop after each `push()`.
    #[cfg(feature = "sessions")]
    pub async fn persist_last(&self) {
        let Some(ref store) = self.store else {
            return;
        };
        let Some(msg) = self.messages.last() else {
            return;
        };
        let ordinal = self.next_ordinal - 1;
        if let Err(e) = store.push_message(self.id, ordinal, msg).await {
            tracing::warn!(
                error = %e,
                session_id = %self.id,
                ordinal,
                "failed to persist message (non-fatal)"
            );
        }
    }
    /// Read-only view of the session messages.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// How many times this session has been compacted.
    pub fn compaction_count(&self) -> u32 {
        self.compaction_count
    }

    /// Split messages for compaction, preserving the most recent `preserve_recent` messages.
    ///
    /// Finds a clean split boundary by walking backward from the split point to the
    /// nearest User message, avoiding breaks in tool_use→tool_result pairs.
    ///
    /// Returns `None` if there aren't enough messages to compact (need at least
    /// `preserve_recent + 2` to have something worth summarizing).
    pub fn split_for_compaction(
        &self,
        preserve_recent: usize,
    ) -> Option<(Vec<Message>, Vec<Message>)> {
        if self.messages.len() <= preserve_recent + 2 {
            return None;
        }

        // Start from the intended split point.
        let raw_split = self.messages.len().saturating_sub(preserve_recent);

        // Walk backward to find a User message boundary — this avoids splitting
        // in the middle of an Assistant→ToolResult sequence.
        let mut split_at = raw_split;
        while split_at > 0 {
            if matches!(self.messages[split_at], Message::User { .. }) {
                break;
            }
            split_at -= 1;
        }

        // If we couldn't find a clean boundary, or the "old" portion would be empty,
        // fall back to the raw split.
        if split_at == 0 {
            split_at = raw_split;
        }

        // Don't compact if the old portion is too small to be useful.
        if split_at < 2 {
            return None;
        }

        let old = self.messages[..split_at].to_vec();
        let recent = self.messages[split_at..].to_vec();
        Some((old, recent))
    }

    /// Replace session messages after compaction.
    ///
    /// The new message list is: [summary_user, summary_ack, ...recent].
    /// Resets ordinals to match the new array and increments the compaction counter.
    pub fn apply_compaction(
        &mut self,
        summary_user: Message,
        summary_ack: Message,
        recent: Vec<Message>,
    ) {
        let mut new_messages = Vec::with_capacity(2 + recent.len());
        new_messages.push(summary_user);
        new_messages.push(summary_ack);
        new_messages.extend(recent);
        self.messages = new_messages;
        self.next_ordinal = self.messages.len() as i32;
        self.compaction_count += 1;
    }

    /// Persist the full compacted message list via `replace_messages`.
    /// Non-fatal: logs warning on failure.
    #[cfg(feature = "sessions")]
    pub async fn persist_compacted(&self) {
        let Some(ref store) = self.store else {
            return;
        };
        if let Err(e) = store.replace_messages(self.id, &self.messages).await {
            tracing::warn!(
                error = %e,
                session_id = %self.id,
                "failed to persist compacted session (non-fatal)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ContentBlock, StopReason};

    #[test]
    fn push_and_retrieve() {
        let mut session = Session::new("test");
        session.push(Message::user_text("hello"));
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn push_returns_ordinal() {
        let mut session = Session::new("test");
        let ord0 = session.push(Message::user_text("first"));
        let ord1 = session.push(Message::user_text("second"));
        assert_eq!(ord0, 0);
        assert_eq!(ord1, 1);
        assert_eq!(session.next_ordinal, 2);
    }

    #[test]
    fn session_id_is_v7() {
        let s = Session::new("test");
        // UUID v7 has version bits set to 7
        assert_eq!(s.id.get_version_num(), 7);
    }

    #[test]
    fn user_id_stored() {
        let s = Session::new("alice");
        assert_eq!(s.user_id, "alice");
    }

    #[test]
    fn compaction_count_starts_at_zero() {
        let s = Session::new("test");
        assert_eq!(s.compaction_count, 0);
    }

    #[test]
    fn split_for_compaction_too_few_messages_returns_none() {
        let mut session = Session::new("test");
        session.push(Message::user_text("hello"));
        session.push(Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "hi".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        });
        // Only 2 messages, preserve_recent=6 → not enough to compact
        assert!(session.split_for_compaction(6).is_none());
    }

    #[test]
    fn split_for_compaction_finds_user_boundary() {
        let mut session = Session::new("test");
        // Build a conversation with 10 messages: 5 user/assistant pairs
        for i in 0..5 {
            session.push(Message::user_text(&format!("msg {i}")));
            session.push(Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: format!("reply {i}"),
                }],
                stop_reason: StopReason::EndTurn,
            });
        }
        assert_eq!(session.messages.len(), 10);

        let (old, recent) = session.split_for_compaction(4).unwrap();
        // Split should be at a User message boundary
        assert!(matches!(recent[0], Message::User { .. }));
        // old + recent = all original messages
        assert_eq!(old.len() + recent.len(), 10);
        assert!(recent.len() >= 4);
    }

    #[test]
    fn split_for_compaction_preserves_tool_result_pairs() {
        let mut session = Session::new("test");
        // User, Assistant(tool_use), ToolResult, Assistant(end), User, Assistant(end)
        session.push(Message::user_text("first"));
        session.push(Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t1".to_owned(),
                name: "bash".to_owned(),
                input: serde_json::json!({"command": "ls"}),
            }],
            stop_reason: StopReason::ToolUse,
        });
        session.push(Message::ToolResult {
            tool_use_id: "t1".to_owned(),
            content: "file.txt".to_owned(),
            is_error: false,
        });
        session.push(Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "done".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        });
        session.push(Message::user_text("second"));
        session.push(Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "reply".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        });
        assert_eq!(session.messages.len(), 6);

        let (old, recent) = session.split_for_compaction(2).unwrap();
        // The split should not break in the middle of tool_use→tool_result
        assert!(matches!(recent[0], Message::User { .. }));
        assert_eq!(old.len() + recent.len(), 6);
    }

    #[test]
    fn apply_compaction_replaces_messages() {
        let mut session = Session::new("test");
        for i in 0..10 {
            session.push(Message::user_text(&format!("msg {i}")));
        }
        assert_eq!(session.messages.len(), 10);
        assert_eq!(session.compaction_count, 0);

        let summary_user = Message::user_text("[Context Summary]");
        let summary_ack = Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "Understood.".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        };
        let recent = vec![Message::user_text("recent msg")];

        session.apply_compaction(summary_user, summary_ack, recent);
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.compaction_count, 1);
        assert_eq!(session.next_ordinal, 3);
        // First message is the summary
        assert!(matches!(session.messages[0], Message::User { .. }));
        // Second is the ack
        assert!(matches!(session.messages[1], Message::Assistant { .. }));
    }

    #[test]
    fn message_serde_round_trip() {
        use crate::providers::UserContent;

        let messages = vec![
            Message::user_text("hello"),
            Message::User {
                content: vec![
                    UserContent::Text("text part".to_owned()),
                    UserContent::Image {
                        media_type: "image/png".to_owned(),
                        data: "base64data==".to_owned(),
                    },
                ],
            },
            Message::Assistant {
                content: vec![
                    ContentBlock::Text {
                        text: "sure".to_owned(),
                    },
                    ContentBlock::ToolUse {
                        id: "tool_abc".to_owned(),
                        name: "bash".to_owned(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                ],
                stop_reason: StopReason::ToolUse,
            },
            Message::ToolResult {
                tool_use_id: "tool_abc".to_owned(),
                content: "file.txt".to_owned(),
                is_error: false,
            },
        ];

        for msg in &messages {
            let json = serde_json::to_string(msg).expect("serialize");
            let restored: Message = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(msg, &restored);
        }
    }
}
