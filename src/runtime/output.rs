use std::future::Future;

/// Events emitted by the agent loop during execution.
pub enum OutputEvent<'a> {
    /// Text content from the model's response.
    Text(&'a str),
    /// Recapitulation: first text block before any tool use (M14b).
    Recapitulation(&'a str),
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
    /// Extended thinking output from the model (M14a).
    Thinking(&'a str),
    /// Progress indicator during tool execution (M14c).
    Progress { tool: &'a str, status: &'a str },
    /// Clear the progress indicator (M14c).
    ProgressClear,
}

/// Abstraction over output delivery. Generic parameter on `AgentLoop`,
/// following the same pattern as `Provider` and `ApprovalGate`.
pub trait OutputSink: Send + Sync {
    fn emit(&self, event: OutputEvent<'_>) -> impl Future<Output = ()> + Send;

    /// Called at the start of a turn. Sinks can use this to initialize batching (M14c/d).
    fn turn_start(&self) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Called at the end of a turn. Sinks can use this to flush batched output (M14c/d).
    fn turn_end(&self) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// Replicates the original `println!()` behavior for CLI use.
pub struct StdoutSink;

impl OutputSink for StdoutSink {
    async fn emit(&self, event: OutputEvent<'_>) {
        match event {
            OutputEvent::Text(text) => println!("{text}"),
            OutputEvent::Recapitulation(text) => {
                // Dim text (ANSI SGR 2) for recapitulation.
                println!("\x1b[2m{text}\x1b[0m");
            }
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
            OutputEvent::Thinking(text) => println!("[THINKING] {text}"),
            OutputEvent::Progress { tool, status } => {
                eprint!("\r\x1b[K[working] {tool}: {status}");
            }
            OutputEvent::ProgressClear => {
                eprint!("\r\x1b[K");
            }
        }
    }
}

/// Discards all output. Used in tests where output is irrelevant.
pub struct NullSink;

impl OutputSink for NullSink {
    async fn emit(&self, _event: OutputEvent<'_>) {}
}
