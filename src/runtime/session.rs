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
    pub(crate) messages: Vec<Message>,
    pub(crate) next_ordinal: i32,
    #[cfg(feature = "sessions")]
    store: Option<Box<dyn SessionStore>>,
}

impl Session {
    /// Create an ephemeral session (no persistence).
    // No Default impl — Default as constructor substitute is prohibited (see CLAUDE.md).
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            id: Uuid::now_v7(),
            messages: Vec::new(),
            next_ordinal: 0,
            #[cfg(feature = "sessions")]
            store: None,
        }
    }

    /// Restore a session from persisted state with an attached store.
    #[cfg(feature = "sessions")]
    pub fn from_persisted(id: Uuid, messages: Vec<Message>, store: Box<dyn SessionStore>) -> Self {
        let next_ordinal = messages.len() as i32;
        Self {
            id,
            messages,
            next_ordinal,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_retrieve() {
        let mut session = Session::new();
        session.push(Message::user_text("hello"));
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn push_returns_ordinal() {
        let mut session = Session::new();
        let ord0 = session.push(Message::user_text("first"));
        let ord1 = session.push(Message::user_text("second"));
        assert_eq!(ord0, 0);
        assert_eq!(ord1, 1);
        assert_eq!(session.next_ordinal, 2);
    }

    #[test]
    fn session_id_is_v7() {
        let s = Session::new();
        // UUID v7 has version bits set to 7
        assert_eq!(s.id.get_version_num(), 7);
    }

    #[test]
    fn message_serde_round_trip() {
        use crate::providers::{ContentBlock, StopReason, UserContent};

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
