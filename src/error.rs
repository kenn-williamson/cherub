use thiserror::Error;

#[derive(Debug, Error)]
pub enum CherubError {
    #[error("action not permitted")]
    NotPermitted,

    #[error("tool execution failed: {0}")]
    ToolExecution(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("invalid tool invocation: {0}")]
    InvalidInvocation(String),
}
