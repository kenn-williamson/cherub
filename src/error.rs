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

    #[error("policy error: {0}")]
    PolicyLoad(String),

    #[error("invalid policy: {0}")]
    PolicyValidation(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[cfg(feature = "postgres")]
    #[error("storage error: {0}")]
    Storage(String),

    /// Credential vault errors (M7a). Phrasing is always generic — never exposes
    /// key material, pattern details, or anything that could help an attacker.
    #[cfg(feature = "credentials")]
    #[error("credential error: {0}")]
    Credential(String),

    /// HTTP tool errors (M7b). Request-level failures that don't involve credentials.
    #[cfg(feature = "credentials")]
    #[error("http tool error: {0}")]
    Http(String),

    /// WASM sandbox errors (M8). Compilation, instantiation, or execution failures.
    /// Never contains credential values or policy details.
    #[cfg(feature = "wasm")]
    #[error("wasm error: {0}")]
    Wasm(String),

    /// Container sandbox errors (M9). Lifecycle, IPC, or execution failures.
    /// Never contains credential values or policy details.
    #[cfg(feature = "container")]
    #[error("container error: {0}")]
    Container(String),
}
