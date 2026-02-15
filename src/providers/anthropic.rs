use crate::error::CherubError;

use super::{Message, Provider};

/// Anthropic API provider stub. Milestone 2 adds reqwest + streaming.
pub struct AnthropicProvider;

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn complete(&self, _messages: &[Message]) -> Result<Message, CherubError> {
        // Stub: Milestone 2 implements actual API communication.
        Err(CherubError::Provider(
            "not implemented".to_owned(),
        ))
    }
}
