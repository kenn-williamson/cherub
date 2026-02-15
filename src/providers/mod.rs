pub mod anthropic;

use crate::error::CherubError;

/// Messages exchanged between the runtime and LLM providers.
/// Enum — variants are known at compile time.
pub enum Message {
    User { content: String },
    Assistant { content: String },
}

/// Extension point for LLM inference backends. One of only two `dyn Trait`
/// boundaries in the project (with `Tool`).
///
/// Synchronous for Milestone 0. Becomes async in Milestone 2 when tokio is added.
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    fn complete(&self, messages: &[Message]) -> Result<Message, CherubError>;
}
