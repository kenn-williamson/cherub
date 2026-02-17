use std::future::Future;

/// Events emitted by the agent loop during execution.
pub enum OutputEvent<'a> {
    /// Text content from the model's response.
    Text(&'a str),
    /// A tool invocation was allowed by policy.
    ToolAllowed { tool: &'a str, command: &'a str },
    /// A tool invocation was rejected by policy.
    ToolRejected { tool: &'a str, command: &'a str },
    /// A tool invocation was approved via escalation.
    ToolApproved { tool: &'a str, command: &'a str },
    /// A tool invocation was denied via escalation.
    ToolDenied { tool: &'a str, command: &'a str },
    /// Successful output from tool execution.
    ToolOutput(&'a str),
    /// Error output from tool execution.
    ToolError(&'a str),
    /// Runtime warning (e.g., max iterations reached).
    Warning(&'a str),
}

/// Abstraction over output delivery. Generic parameter on `AgentLoop`,
/// following the same pattern as `Provider` and `ApprovalGate`.
pub trait OutputSink: Send + Sync {
    fn emit(&self, event: OutputEvent<'_>) -> impl Future<Output = ()> + Send;
}

/// Replicates the original `println!()` behavior for CLI use.
pub struct StdoutSink;

impl OutputSink for StdoutSink {
    async fn emit(&self, event: OutputEvent<'_>) {
        match event {
            OutputEvent::Text(text) => println!("{text}"),
            OutputEvent::ToolAllowed { tool, command } => {
                println!("[ALLOWED] {tool}: {command}");
            }
            OutputEvent::ToolRejected { tool, command } => {
                println!("[REJECTED] {tool}: {command}");
            }
            OutputEvent::ToolApproved { tool, command } => {
                println!("[APPROVED] {tool}: {command}");
            }
            OutputEvent::ToolDenied { tool, command } => {
                println!("[DENIED] {tool}: {command}");
            }
            OutputEvent::ToolOutput(output) => println!("{output}"),
            OutputEvent::ToolError(err) => println!("[ERROR] {err}"),
            OutputEvent::Warning(msg) => println!("[WARNING] {msg}"),
        }
    }
}

/// Discards all output. Used in tests where output is irrelevant.
pub struct NullSink;

impl OutputSink for NullSink {
    async fn emit(&self, _event: OutputEvent<'_>) {}
}
