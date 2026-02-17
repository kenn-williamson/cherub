use crate::providers::Message;

/// In-memory conversation history. No persistence, no pruning (deferred).
pub struct Session {
    messages: Vec<Message>,
}

impl Session {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_retrieve() {
        let mut session = Session::new();
        session.push(Message::user_text("hello"));
        assert_eq!(session.messages().len(), 1);
    }
}
