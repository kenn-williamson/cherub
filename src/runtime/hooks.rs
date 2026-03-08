use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::warn;

use crate::providers::{ApiUsage, ContentBlock, Message, StopReason, UserContent};

/// Result type for hook methods. Errors are non-fatal — logged and skipped.
pub type HookResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Lifecycle hooks for the agent loop (M15a).
///
/// Each method has a default no-op implementation. Hooks are stored as
/// `Vec<Box<dyn Hook>>` — `dyn Hook` is a legitimate extension boundary
/// per project convention (`async_trait` + object safety).
///
/// Hook errors are non-fatal: dispatchers catch errors, log a warning,
/// and continue. Hooks must never block the agent loop.
#[async_trait]
pub trait Hook: Send + Sync {
    /// Fired before the user message is pushed to the session.
    /// Content is mutable — hooks can redact or transform input.
    async fn before_inbound(&self, _ctx: &mut InboundContext<'_>) -> HookResult {
        Ok(())
    }

    /// Fired before each `provider.complete()` call (once per iteration).
    async fn before_provider_call(&self, _ctx: &ProviderCallContext<'_>) -> HookResult {
        Ok(())
    }

    /// Fired after `provider.complete()` returns, before parsing content blocks.
    async fn after_provider_call(&self, _ctx: &ProviderResponseContext<'_>) -> HookResult {
        Ok(())
    }

    /// Fired after enforcement Allow/Approved, before tool execution.
    async fn before_tool_call(&self, _ctx: &BeforeToolCallContext<'_>) -> HookResult {
        Ok(())
    }

    /// Fired after tool execution, before the result is pushed to the session.
    /// Result is mutable — hooks can transform or stash output.
    async fn after_tool_call(&self, _ctx: &mut AfterToolCallContext<'_>) -> HookResult {
        Ok(())
    }

    /// Fired before context compaction, after the split point is determined.
    async fn before_compaction(&self, _ctx: &CompactionContext<'_>) -> HookResult {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Context structs
// ---------------------------------------------------------------------------

/// Context for `before_inbound`. Mutable content allows redaction/transformation.
pub struct InboundContext<'a> {
    pub content: &'a mut Vec<UserContent>,
    pub user_id: &'a str,
}

/// Context for `before_provider_call`. Read-only view of what will be sent.
pub struct ProviderCallContext<'a> {
    pub system_prompt: &'a str,
    pub messages: &'a [Message],
    pub iteration: usize,
}

/// Context for `after_provider_call`. Read-only view of the response.
pub struct ProviderResponseContext<'a> {
    pub content: &'a [ContentBlock],
    pub stop_reason: StopReason,
    pub usage: Option<ApiUsage>,
}

/// Context for `before_tool_call`. Read-only view of the tool invocation.
pub struct BeforeToolCallContext<'a> {
    pub tool: &'a str,
    pub action: &'a str,
    pub params: &'a serde_json::Value,
}

/// Context for `after_tool_call`. Mutable result for transformation/stashing.
pub struct AfterToolCallContext<'a> {
    pub tool: &'a str,
    pub action: &'a str,
    pub result: &'a mut String,
    pub is_error: bool,
}

/// Context for `before_compaction`. Read-only view of messages about to be compacted.
pub struct CompactionContext<'a> {
    pub messages_to_compact: &'a [Message],
    pub total_message_count: usize,
}

// ---------------------------------------------------------------------------
// Dispatch helpers
// ---------------------------------------------------------------------------

pub(crate) async fn dispatch_before_inbound(hooks: &[Box<dyn Hook>], ctx: &mut InboundContext<'_>) {
    for hook in hooks {
        if let Err(e) = hook.before_inbound(ctx).await {
            warn!(error = %e, "hook before_inbound failed (non-fatal)");
        }
    }
}

pub(crate) async fn dispatch_before_provider_call(
    hooks: &[Box<dyn Hook>],
    ctx: &ProviderCallContext<'_>,
) {
    for hook in hooks {
        if let Err(e) = hook.before_provider_call(ctx).await {
            warn!(error = %e, "hook before_provider_call failed (non-fatal)");
        }
    }
}

pub(crate) async fn dispatch_after_provider_call(
    hooks: &[Box<dyn Hook>],
    ctx: &ProviderResponseContext<'_>,
) {
    for hook in hooks {
        if let Err(e) = hook.after_provider_call(ctx).await {
            warn!(error = %e, "hook after_provider_call failed (non-fatal)");
        }
    }
}

pub(crate) async fn dispatch_before_tool_call(
    hooks: &[Box<dyn Hook>],
    ctx: &BeforeToolCallContext<'_>,
) {
    for hook in hooks {
        if let Err(e) = hook.before_tool_call(ctx).await {
            warn!(error = %e, "hook before_tool_call failed (non-fatal)");
        }
    }
}

pub(crate) async fn dispatch_after_tool_call(
    hooks: &[Box<dyn Hook>],
    ctx: &mut AfterToolCallContext<'_>,
) {
    for hook in hooks {
        if let Err(e) = hook.after_tool_call(ctx).await {
            warn!(error = %e, "hook after_tool_call failed (non-fatal)");
        }
    }
}

pub(crate) async fn dispatch_before_compaction(
    hooks: &[Box<dyn Hook>],
    ctx: &CompactionContext<'_>,
) {
    for hook in hooks {
        if let Err(e) = hook.before_compaction(ctx).await {
            warn!(error = %e, "hook before_compaction failed (non-fatal)");
        }
    }
}

// ---------------------------------------------------------------------------
// M15b: OutputStashingHook
// ---------------------------------------------------------------------------

/// Stashes large tool outputs to files, replacing them with a truncated
/// preview + file reference. Prevents context window overflow from large
/// tool results (e.g. `cat` on a big file).
pub struct OutputStashingHook {
    threshold: usize,
    workspace_root: PathBuf,
}

/// Default stash threshold: 256 KiB.
const DEFAULT_STASH_THRESHOLD: usize = 256 * 1024;

/// Preview size: first 1 KiB of the original output.
const PREVIEW_SIZE: usize = 1024;

impl OutputStashingHook {
    pub fn new(workspace_root: &Path) -> Self {
        Self {
            threshold: DEFAULT_STASH_THRESHOLD,
            workspace_root: workspace_root.to_owned(),
        }
    }

    pub fn with_threshold(mut self, threshold: usize) -> Self {
        self.threshold = threshold;
        self
    }
}

#[async_trait]
impl Hook for OutputStashingHook {
    async fn after_tool_call(&self, ctx: &mut AfterToolCallContext<'_>) -> HookResult {
        if ctx.is_error || ctx.result.len() <= self.threshold {
            return Ok(());
        }

        let stash_dir = self.workspace_root.join(".cherub").join("stash");
        tokio::fs::create_dir_all(&stash_dir).await?;

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        // Sanitize tool name for filename (replace non-alphanumeric with underscore).
        let sanitized_tool: String = ctx
            .tool
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();

        let filename = format!("{timestamp}_{sanitized_tool}.txt");
        let file_path = stash_dir.join(&filename);

        tokio::fs::write(&file_path, ctx.result.as_bytes()).await?;

        let original_len = ctx.result.len();
        let preview = if ctx.result.len() > PREVIEW_SIZE {
            &ctx.result[..PREVIEW_SIZE]
        } else {
            ctx.result.as_str()
        };
        let relative_path = format!(".cherub/stash/{filename}");

        *ctx.result = format!(
            "[Output stashed: {original_len} bytes → {relative_path}]\n\n\
             Preview (first {PREVIEW_SIZE} bytes):\n{preview}"
        );

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hook(dir: &Path) -> OutputStashingHook {
        OutputStashingHook::new(dir).with_threshold(100)
    }

    #[tokio::test]
    async fn stash_ignores_small_output() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook(dir.path());
        let mut output = "small output".to_owned();
        let mut ctx = AfterToolCallContext {
            tool: "bash",
            action: "execute",
            result: &mut output,
            is_error: false,
        };
        hook.after_tool_call(&mut ctx).await.unwrap();
        assert_eq!(output, "small output");
    }

    #[tokio::test]
    async fn stash_ignores_errors() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook(dir.path());
        let mut output = "x".repeat(200);
        let original = output.clone();
        let mut ctx = AfterToolCallContext {
            tool: "bash",
            action: "execute",
            result: &mut output,
            is_error: true,
        };
        hook.after_tool_call(&mut ctx).await.unwrap();
        assert_eq!(output, original);
    }

    #[tokio::test]
    async fn stash_replaces_large_output() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook(dir.path());
        let mut output = "x".repeat(200);
        let mut ctx = AfterToolCallContext {
            tool: "bash",
            action: "execute",
            result: &mut output,
            is_error: false,
        };
        hook.after_tool_call(&mut ctx).await.unwrap();
        assert!(output.contains("[Output stashed:"));
        assert!(output.contains(".cherub/stash/"));
        assert!(output.contains("Preview"));
    }

    #[tokio::test]
    async fn stash_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook(dir.path());
        let mut output = "x".repeat(200);
        let mut ctx = AfterToolCallContext {
            tool: "bash",
            action: "execute",
            result: &mut output,
            is_error: false,
        };
        hook.after_tool_call(&mut ctx).await.unwrap();
        assert!(dir.path().join(".cherub/stash").exists());
    }

    #[tokio::test]
    async fn stash_file_contains_original() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook(dir.path());
        let original = "y".repeat(200);
        let mut output = original.clone();
        let mut ctx = AfterToolCallContext {
            tool: "bash",
            action: "execute",
            result: &mut output,
            is_error: false,
        };
        hook.after_tool_call(&mut ctx).await.unwrap();

        // Find the stashed file and verify contents.
        let stash_dir = dir.path().join(".cherub/stash");
        let mut entries = std::fs::read_dir(&stash_dir).unwrap();
        let entry = entries.next().unwrap().unwrap();
        let contents = std::fs::read_to_string(entry.path()).unwrap();
        assert_eq!(contents, original);
    }
}
