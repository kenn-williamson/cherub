pub mod approval;
pub mod hooks;
pub mod output;
pub mod prompt;
#[cfg(feature = "schedule")]
pub mod schedule;
pub mod session;
pub mod tokens;

use std::future::Future;
use std::time::{Duration, Instant};

use tracing::{info, info_span, warn};

use crate::enforcement::policy::Policy;
use crate::enforcement::{self, Decision};
use crate::error::CherubError;
use crate::providers::{
    ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition, ToolResultImage,
    UserContent,
};
use crate::tools::{Proposed, ToolContext, ToolInvocation, ToolRegistry};

use approval::{ApprovalGate, ApprovalResult, EscalationContext};
use output::{OutputEvent, OutputSink};
use session::Session;

#[cfg(feature = "postgres")]
use crate::storage::{
    AuditDecision, AuditStore, CallType, CostStore, NewAuditEvent, NewTokenUsage,
};

const MAX_ITERATIONS: usize = 25;

/// Compact when estimated tokens exceed this fraction of the context window.
const COMPACTION_THRESHOLD_RATIO: f32 = 0.75;

/// Hard-stop safety net: force compaction before provider.complete() if estimated
/// tokens exceed this fraction of the window. Catches mid-turn growth from large
/// tool results that push past the normal 75% pre-turn compaction.
const HARD_STOP_RATIO: f32 = 0.95;

/// Number of recent messages to preserve across compaction (3 turn pairs).
const COMPACTION_PRESERVE_RECENT: usize = 6;

/// Minimum message count before compaction is even considered.
const COMPACTION_MIN_MESSAGES: usize = 10;

/// Maximum number of memories injected into the system prompt per turn.
#[cfg(feature = "memory")]
const INJECTION_MAX_MEMORIES: i64 = 5;

/// Minimum query length to trigger injection search. Skip for very short messages.
#[cfg(feature = "memory")]
const INJECTION_MIN_QUERY_LEN: usize = 3;

/// Extract plain text from user content, joining multiple text blocks with a space.
/// Image blocks are silently skipped — only text contributes to the injection query.
#[cfg(feature = "memory")]
fn extract_user_text(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(|c| {
            if let UserContent::Text(text) = c {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The agent loop. Owns session state and orchestrates model <-> tool interaction.
/// Generic over approval gate and output sink for testability. Provider is
/// `Box<dyn Provider>` — object-safe via `async_trait` (M13-prep).
pub struct AgentLoop<A: ApprovalGate, O: OutputSink> {
    session: Session,
    policy: Policy,
    provider: Box<dyn Provider>,
    registry: ToolRegistry,
    system_prompt: String,
    tool_definitions: Vec<ToolDefinition>,
    approval_gate: A,
    output: O,
    /// Last API-reported input token count, used for smarter compaction triggering.
    last_usage: Option<ApiUsage>,
    /// Optional shared memory store for proactive injection (M6d).
    /// When set, the runtime queries memories before each turn and injects
    /// the top results into the system prompt. The agent cannot suppress this.
    #[cfg(feature = "memory")]
    memory_store: Option<std::sync::Arc<dyn crate::storage::MemoryStore>>,
    /// Optional audit log store (M10).
    /// When set, every enforcement decision and execution outcome is appended.
    /// Failures are non-fatal — logged and skipped; they never block tool execution.
    #[cfg(feature = "postgres")]
    audit_store: Option<std::sync::Arc<dyn AuditStore>>,
    /// Optional cost tracking store (M12).
    /// When set, every LLM API call is recorded with token counts and cost.
    /// Failures are non-fatal — logged and skipped; they never block inference.
    #[cfg(feature = "postgres")]
    cost_store: Option<std::sync::Arc<dyn CostStore>>,
    /// In-memory pricing table loaded from DB at startup.
    /// Used by `record_cost()` to look up per-model rates.
    /// Empty map = all costs recorded as $0.00 (no DB or no pricing configured).
    #[cfg(feature = "postgres")]
    pricing_table: crate::providers::pricing::PricingTable,
    /// Whether to emit thinking blocks to the output sink (M14a).
    show_thinking: bool,
    /// Lifecycle hooks (M15a). Dispatched at 6 points in the agent loop.
    /// Errors are non-fatal — logged and skipped, never blocking execution.
    hooks: Vec<Box<dyn hooks::Hook>>,
    /// Optional task queue store for async approval.
    /// When set with `autonomous_mode`, commit-tier actions are queued instead
    /// of blocking the turn waiting for human input.
    #[cfg(feature = "postgres")]
    task_store: Option<std::sync::Arc<dyn crate::storage::TaskStore>>,
    /// When true, escalated actions are queued to `task_store` instead of blocking.
    /// Set by the caller for cron/autonomous turns; false for interactive turns.
    #[cfg(feature = "postgres")]
    autonomous_mode: bool,
}

impl<A: ApprovalGate, O: OutputSink> AgentLoop<A, O> {
    pub fn new(
        policy: Policy,
        provider: Box<dyn Provider>,
        registry: ToolRegistry,
        system_prompt: String,
        approval_gate: A,
        output: O,
        user_id: &str,
    ) -> Self {
        let tool_definitions = registry.definitions();
        Self {
            session: Session::new(user_id),
            policy,
            provider,
            registry,
            system_prompt,
            tool_definitions,
            approval_gate,
            output,
            last_usage: None,
            #[cfg(feature = "memory")]
            memory_store: None,
            #[cfg(feature = "postgres")]
            audit_store: None,
            #[cfg(feature = "postgres")]
            cost_store: None,
            #[cfg(feature = "postgres")]
            pricing_table: std::collections::HashMap::new(),
            show_thinking: false,
            hooks: Vec::new(),
            #[cfg(feature = "postgres")]
            task_store: None,
            #[cfg(feature = "postgres")]
            autonomous_mode: false,
        }
    }

    /// Attach a memory store for proactive injection.
    ///
    /// When attached, the runtime embeds the user message and queries for relevant
    /// memories before each turn, injecting results into the system prompt. The agent
    /// cannot suppress injection — context is controlled entirely by the runtime.
    ///
    /// Call this once after `new()` and before the first `run_turn()`.
    #[cfg(feature = "memory")]
    pub fn with_memory_injection(
        &mut self,
        store: std::sync::Arc<dyn crate::storage::MemoryStore>,
    ) {
        self.memory_store = Some(store);
    }

    /// Attach an audit log store (M10).
    ///
    /// When attached, every enforcement decision (allow/reject/escalate/approve/deny)
    /// and execution outcome is appended to the store. Append failures are non-fatal —
    /// they are logged and execution continues normally.
    ///
    /// Call this once after `new()` and before the first `run_turn()`.
    #[cfg(feature = "postgres")]
    pub fn with_audit_log(&mut self, store: std::sync::Arc<dyn AuditStore>) {
        self.audit_store = Some(store);
    }

    /// Attach a cost tracking store (M12).
    ///
    /// When attached, every LLM API call (inference, summarization, extraction) is
    /// recorded with token counts and computed cost. Failures are non-fatal —
    /// they are logged and execution continues normally.
    ///
    /// Call this once after `new()` and before the first `run_turn()`.
    #[cfg(feature = "postgres")]
    pub fn with_cost_tracking(&mut self, store: std::sync::Arc<dyn CostStore>) {
        self.cost_store = Some(store);
    }

    /// Set the in-memory pricing table for cost computation.
    ///
    /// The table is loaded from `model_pricing` at startup. If empty (no DB or
    /// no pricing rows), all costs are recorded as $0.00.
    #[cfg(feature = "postgres")]
    pub fn with_pricing_table(&mut self, table: crate::providers::pricing::PricingTable) {
        self.pricing_table = table;
    }

    /// Attach a PostgreSQL session store. Resumes the previous session for the given
    /// connector channel, or creates a new one.
    ///
    /// Call this once after `new()` and before the first `run_turn()`.
    #[cfg(feature = "sessions")]
    pub async fn with_persistence(
        &mut self,
        store: Box<dyn crate::storage::SessionStore>, // storage module gated by postgres
        connector: &str,
        connector_id: &str,
    ) -> Result<(), CherubError> {
        let (session_id, messages) = store.get_or_create_session(connector, connector_id).await?;
        let msg_count = messages.len();
        let user_id = self.session.user_id.clone();
        self.session = Session::from_persisted(session_id, messages, user_id, store);
        tracing::info!(
            session_id = %session_id,
            message_count = msg_count,
            connector,
            connector_id,
            "session attached"
        );
        Ok(())
    }

    /// Enable or disable emitting thinking blocks to the output sink (M14a).
    pub fn with_show_thinking(&mut self, show: bool) {
        self.show_thinking = show;
    }

    /// Register a lifecycle hook (M15a).
    ///
    /// Hooks are dispatched in registration order at 6 points in the agent loop.
    /// Hook errors are non-fatal — logged and skipped, never blocking execution.
    pub fn with_hook(&mut self, hook: Box<dyn hooks::Hook>) {
        self.hooks.push(hook);
    }

    /// Attach a task queue store for async approval.
    ///
    /// When attached and `with_autonomous_mode()` is set, commit-tier actions during
    /// the turn are queued to the store instead of blocking for user input.
    #[cfg(feature = "postgres")]
    pub fn with_task_store(&mut self, store: std::sync::Arc<dyn crate::storage::TaskStore>) {
        self.task_store = Some(store);
    }

    /// Enable autonomous mode for the next turn.
    ///
    /// In autonomous mode, escalated (commit-tier) actions are queued to `task_store`
    /// instead of blocking. The user is notified via the approval gate. The turn
    /// continues with remaining work rather than waiting.
    ///
    /// Call this before each cron-triggered turn; the flag is reset to `false` after
    /// each `run_turn()` completes (interactive turns are always non-autonomous).
    #[cfg(feature = "postgres")]
    pub fn set_autonomous_mode(&mut self, enabled: bool) {
        self.autonomous_mode = enabled;
    }

    /// Execute all tasks in the queue that have been approved by the user.
    ///
    /// Called after an approval callback is received (immediate) or at the start of
    /// a cron turn (catches any approvals that arrived between cycles). Each approved
    /// task is re-evaluated through the enforcement layer (policy may have changed),
    /// executed with the human-approval token, and marked done or failed.
    ///
    /// Returns the number of tasks that were executed (successfully or not).
    #[cfg(feature = "postgres")]
    pub async fn drain_approved_tasks(&mut self) -> usize {
        use crate::enforcement;
        use crate::tools::{Proposed, ToolContext, ToolInvocation};

        let store = match &self.task_store {
            Some(s) => std::sync::Arc::clone(s),
            None => return 0,
        };

        let tasks = match store.list_approved(&self.session.user_id).await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "drain_approved_tasks: failed to list approved tasks");
                return 0;
            }
        };

        let mut executed = 0usize;
        for task in tasks {
            match store.mark_running(task.id).await {
                Ok(true) => {} // claimed — proceed
                Ok(false) => {
                    info!(task_id = %task.id, "drain: lost race, task already claimed by another drainer");
                    continue;
                }
                Err(e) => {
                    warn!(task_id = %task.id, error = %e, "drain: failed to mark running");
                    continue;
                }
            }

            let action_str = task.action.as_deref().unwrap_or("");
            let proposal =
                ToolInvocation::<Proposed>::new(&task.tool, action_str, task.params.clone());
            let (evaluated, decision) = enforcement::evaluate(proposal, &self.policy, None);

            let ctx = ToolContext {
                user_id: self.session.user_id.clone(),
                session_id: self.session.id,
                turn_number: self.session.next_ordinal,
            };

            match decision {
                crate::enforcement::Decision::Escalate { tier } => {
                    let token = enforcement::approve_escalation(tier);
                    match evaluated.execute(token, &self.registry, &ctx).await {
                        Ok(result) => {
                            info!(task_id = %task.id, tool = %task.tool, "approved task executed");
                            executed += 1;
                            let output = result.output;
                            self.output
                                .emit(output::OutputEvent::Text(&format!(
                                    "✓ Completed: {}\n\n{}",
                                    task.description, output
                                )))
                                .await;
                            if let Err(e) = store.mark_done(task.id, &output).await {
                                warn!(task_id = %task.id, error = %e, "drain: failed to mark done");
                            }
                        }
                        Err(e) => {
                            warn!(task_id = %task.id, error = %e, "approved task execution failed");
                            let err_msg = e.to_string();
                            self.output
                                .emit(output::OutputEvent::Text(&format!(
                                    "✗ Failed: {}\n\n{}",
                                    task.description, err_msg
                                )))
                                .await;
                            if let Err(e2) = store.mark_failed(task.id, &err_msg).await {
                                warn!(task_id = %task.id, error = %e2, "drain: failed to mark failed");
                            }
                        }
                    }
                }
                _ => {
                    // Policy changed since the task was queued — action is no longer commit-tier.
                    let msg = "policy changed: action no longer requires approval (tier changed)";
                    warn!(task_id = %task.id, "drain: {}", msg);
                    if let Err(e) = store.mark_failed(task.id, msg).await {
                        warn!(task_id = %task.id, error = %e, "drain: failed to mark failed");
                    }
                }
            }
        }

        executed
    }

    /// Read-only view of the conversation history.
    pub fn session_messages(&self) -> &[Message] {
        &self.session.messages
    }

    /// The session ID (UUID v7, time-sortable).
    pub fn session_id(&self) -> uuid::Uuid {
        self.session.id
    }

    /// Append an audit event non-fatally. Logs a warning on failure; never panics.
    /// Audit failures must never block tool execution — the runtime continues regardless.
    #[cfg(feature = "postgres")]
    async fn audit(&self, event: NewAuditEvent) {
        if let Some(ref store) = self.audit_store
            && let Err(e) = store.append(event).await
        {
            warn!(error = %e, "audit log append failed (non-fatal)");
        }
    }

    /// Record a cost event non-fatally. Logs a warning on failure; never panics.
    /// Cost recording failures must never block inference — the runtime continues regardless.
    #[cfg(feature = "postgres")]
    async fn record_cost(&self, usage: ApiUsage, call_type: CallType) {
        use crate::providers::pricing;

        if let Some(ref store) = self.cost_store {
            let cost_usd = pricing::lookup_pricing(&self.pricing_table, self.provider.model_name())
                .map_or(0.0, |p| pricing::compute_cost(&usage, &p));
            if let Err(e) = store
                .record(NewTokenUsage {
                    session_id: Some(self.session.id),
                    user_id: self.session.user_id.clone(),
                    turn_number: Some(self.session.next_ordinal),
                    model_name: self.provider.model_name().to_owned(),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cost_usd,
                    call_type,
                })
                .await
            {
                warn!(error = %e, "cost recording failed (non-fatal)");
            }
        }
    }

    /// Check whether the session exceeds the context window threshold and compact if so.
    ///
    /// Called once per turn, after pushing the user message and building the effective
    /// system prompt, but **before** the iteration loop. Mid-turn compaction would
    /// break tool_use/tool_result pairing.
    async fn maybe_compact(&mut self, effective_system: &str) -> Result<(), CherubError> {
        if self.session.messages.len() < COMPACTION_MIN_MESSAGES {
            return Ok(());
        }

        // Use API-reported usage if available, otherwise estimate.
        let input_tokens = self.last_usage.map(|u| u.input_tokens).unwrap_or_else(|| {
            tokens::estimate_tokens(
                effective_system,
                &self.session.messages,
                &self.tool_definitions,
            )
        });

        let window = tokens::context_window_size(self.provider.model_name());
        let threshold = (window as f32 * COMPACTION_THRESHOLD_RATIO) as u32;

        if input_tokens < threshold {
            return Ok(());
        }

        info!(
            input_tokens,
            threshold,
            message_count = self.session.messages.len(),
            "context window threshold exceeded, compacting"
        );

        let Some((old, recent)) = self
            .session
            .split_for_compaction(COMPACTION_PRESERVE_RECENT)
        else {
            warn!("compaction split failed — not enough messages at clean boundary");
            return Ok(());
        };

        // Hook: before_compaction.
        hooks::dispatch_before_compaction(
            &self.hooks,
            &hooks::CompactionContext {
                messages_to_compact: &old,
                total_message_count: self.session.messages.len(),
            },
        )
        .await;

        // Pre-compaction memory flush (feature-gated, non-fatal).
        #[cfg(feature = "memory")]
        self.flush_to_memory(&old, effective_system).await;

        // Summarize the old messages.
        let summary = self.summarize(&old, effective_system).await?;

        let compaction_number = self.session.compaction_count + 1;
        let summary_user = Message::user_text(&format!(
            "[Context Summary — Compaction #{compaction_number}]\n\n{summary}"
        ));
        let summary_ack = Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "Understood. I have the context from our earlier conversation.".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
        };

        self.session
            .apply_compaction(summary_user, summary_ack, recent);

        #[cfg(feature = "sessions")]
        self.session.persist_compacted().await;

        // Clear last usage since the message list changed dramatically.
        self.last_usage = None;

        info!(
            compaction_number,
            new_message_count = self.session.messages.len(),
            "compaction complete"
        );

        self.output
            .emit(OutputEvent::Warning(
                "Context compacted — older conversation has been summarized.",
            ))
            .await;

        Ok(())
    }

    /// Call the provider to summarize a block of messages for compaction.
    ///
    /// Uses a summarization-only prompt (no tools, no enforcement) — this is a
    /// runtime operation, not an agent tool call.
    async fn summarize(
        &self,
        messages: &[Message],
        _effective_system: &str,
    ) -> Result<String, CherubError> {
        let conversation_text = prompt::serialize_messages_for_prompt(messages);

        let summarize_prompt = format!(
            "You are a conversation summarizer. Below is a conversation between a user and an \
             AI assistant. Produce a concise summary that preserves:\n\
             - Key decisions and conclusions\n\
             - Important facts, file paths, and code references\n\
             - User preferences and instructions\n\
             - Current state of any in-progress tasks\n\n\
             Omit routine back-and-forth. Focus on information the assistant would need \
             to continue the conversation coherently.\n\n\
             --- Conversation ---\n\
             {conversation_text}\n\
             --- End of conversation ---\n\n\
             Provide only the summary, no preamble."
        );

        let summary_messages = vec![Message::user_text(&summarize_prompt)];

        let (response, _usage) = self
            .provider
            .complete(
                "You are a concise summarizer.",
                &summary_messages,
                &[], // No tools for summarization
            )
            .await?;

        #[cfg(feature = "postgres")]
        if let Some(u) = _usage {
            self.record_cost(u, CallType::Summarization).await;
        }

        // Extract text from the response.
        match response {
            Message::Assistant { content, .. } => {
                let text: String = content
                    .iter()
                    .filter_map(|block| {
                        if let ContentBlock::Text { text } = block {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(text)
            }
            _ => Err(CherubError::Provider(
                "unexpected response type from summarization".to_owned(),
            )),
        }
    }

    /// Flush important facts from old messages to memory before compaction.
    ///
    /// Two-pass design:
    /// - **Pass 1 (Working scope):** All extracted facts are written directly to
    ///   `MemoryScope::Working` via `store.store()`. Working is Observe tier — no
    ///   enforcement needed for a runtime operation at this level.
    /// - **Pass 2 (User scope promotion):** High-importance facts (preferences,
    ///   facts, instructions) are promoted to `MemoryScope::User` through the
    ///   enforcement pipeline. If enforcement escalates (no user available during
    ///   compaction) or rejects, the fact stays in Working scope.
    ///
    /// Non-fatal throughout: any failure at any step logs a warning and proceeds.
    /// Compaction must succeed regardless of memory flush outcomes.
    #[cfg(feature = "memory")]
    async fn flush_to_memory(&self, messages: &[Message], effective_system: &str) {
        let Some(ref store) = self.memory_store else {
            return;
        };

        let conversation_text = prompt::serialize_messages_for_prompt(messages);
        let extraction_prompt = format!(
            "Extract important facts, preferences, and decisions from this conversation. \
             Return a JSON array of objects, each with:\n\
             - \"content\": the fact or preference (one sentence)\n\
             - \"category\": one of \"preference\", \"fact\", \"instruction\", \"observation\"\n\
             - \"importance\": \"high\" for explicit preferences, instructions, and \
               identity-relevant facts; \"low\" for transient observations and routine details\n\n\
             Only include information worth remembering across sessions. \
             Omit routine tool outputs and transient details.\n\n\
             --- Conversation ---\n\
             {conversation_text}\n\
             --- End ---\n\n\
             Return ONLY the JSON array, no other text."
        );

        let extraction_messages = vec![Message::user_text(&extraction_prompt)];
        let result = self
            .provider
            .complete(effective_system, &extraction_messages, &[])
            .await;

        let (response, extraction_usage) = match result {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "memory flush extraction failed (non-fatal)");
                return;
            }
        };

        #[cfg(feature = "postgres")]
        if let Some(u) = extraction_usage {
            self.record_cost(u, CallType::Extraction).await;
        }

        // Parse the response as a JSON array of facts.
        let text = match response {
            Message::Assistant { ref content, .. } => content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(""),
            _ => return,
        };

        // Try to parse JSON array from the response text.
        // Strip markdown code fences if present.
        let json_text = text
            .trim()
            .strip_prefix("```json")
            .or_else(|| text.trim().strip_prefix("```"))
            .and_then(|s| s.strip_suffix("```"))
            .unwrap_or(text.trim());

        let facts: Vec<serde_json::Value> = match serde_json::from_str(json_text) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "memory flush JSON parse failed (non-fatal)");
                return;
            }
        };

        let mut working_count = 0u32;
        let mut promoted_count = 0u32;

        for fact in &facts {
            let Some(content) = fact.get("content").and_then(|v| v.as_str()) else {
                continue;
            };
            let category_str = fact
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("observation");
            let category = category_str
                .parse::<crate::storage::MemoryCategory>()
                .unwrap_or(crate::storage::MemoryCategory::Observation);
            let importance = fact
                .get("importance")
                .and_then(|v| v.as_str())
                .unwrap_or("low");

            // Pass 1: Write to Working scope (Observe tier, always succeeds).
            let working_memory = crate::storage::NewMemory {
                user_id: self.session.user_id.clone(),
                scope: crate::storage::MemoryScope::Working,
                category,
                path: format!("working/compaction/{category_str}"),
                content: content.to_owned(),
                structured: None,
                source_session_id: Some(self.session.id),
                source_turn_number: None,
                source_type: crate::storage::SourceType::Inferred,
                confidence: 0.8,
            };
            match store.store(working_memory).await {
                Ok(_) => working_count += 1,
                Err(e) => {
                    warn!(error = %e, "memory flush working store failed (non-fatal)");
                }
            }

            // Pass 2: Attempt User scope promotion for high-importance facts.
            // Only preferences, facts, and instructions are candidates.
            // Observations stay in Working scope regardless.
            if importance != "high" {
                continue;
            }

            let user_path = match category_str {
                "preference" => "preferences/compaction",
                "fact" => "facts/compaction",
                "instruction" => "instructions/compaction",
                _ => continue,
            };

            let params = serde_json::json!({
                "action": "store",
                "scope": "user",
                "path": user_path,
                "content": content,
                "category": category_str,
                "source_type": "inferred",
                "confidence": 0.8
            });

            let proposal = ToolInvocation::<Proposed>::new("memory", "execute", params);
            let (evaluated, decision) = enforcement::evaluate(proposal, &self.policy, None);

            match decision {
                Decision::Allow(token) => {
                    let ctx = ToolContext {
                        user_id: self.session.user_id.clone(),
                        session_id: self.session.id,
                        turn_number: self.session.next_ordinal,
                    };
                    match evaluated.execute(token, &self.registry, &ctx).await {
                        Ok(_) => promoted_count += 1,
                        Err(e) => {
                            warn!(error = %e, "memory flush user promotion failed (non-fatal)");
                        }
                    }
                }
                Decision::Escalate { .. } => {
                    // User not available during compaction — fact stays in Working scope.
                }
                Decision::Reject => {
                    // Policy doesn't allow this write — fact stays in Working scope.
                }
            }
        }

        if working_count > 0 || promoted_count > 0 {
            info!(
                working_count,
                promoted_count, "pre-compaction memory flush complete"
            );
        }
    }

    /// Convenience wrapper: run a text-only user turn.
    pub async fn run_turn_text(&mut self, text: &str) -> Result<(), CherubError> {
        self.run_turn(vec![UserContent::Text(text.to_owned())])
            .await
    }

    /// Run one user turn: push user message, call model, handle tool calls in a loop.
    pub async fn run_turn(&mut self, content: Vec<UserContent>) -> Result<(), CherubError> {
        // Note: we don't use entered() spans because EnteredSpan is !Send,
        // which prevents this future from being spawned on tokio. Structured
        // fields on info!() calls carry the same context.
        let _span = info_span!("turn");

        self.output.turn_start().await;
        let result = self.run_turn_inner(content).await;
        // Always clear autonomous_mode — even if the turn errored — so a failed
        // autonomous turn cannot contaminate the next user-driven interactive turn.
        #[cfg(feature = "postgres")]
        {
            self.autonomous_mode = false;
        }
        self.output.turn_end().await;
        result
    }

    /// Inner implementation of `run_turn`, separated so lifecycle calls always fire.
    async fn run_turn_inner(&mut self, mut content: Vec<UserContent>) -> Result<(), CherubError> {
        // Hook: before_inbound — hooks can redact/transform user input.
        hooks::dispatch_before_inbound(
            &self.hooks,
            &mut hooks::InboundContext {
                content: &mut content,
                user_id: &self.session.user_id,
            },
        )
        .await;

        // Extract text for injection query AFTER hooks have processed content.
        #[cfg(feature = "memory")]
        let user_query = extract_user_text(&content);

        self.session.push(Message::User { content });
        #[cfg(feature = "sessions")]
        self.session.persist_last().await;

        // Build effective system prompt — may include injected memories (M6d).
        // Computed once per turn, used for every provider.complete() call in this turn.
        // The agent cannot suppress injection: this is runtime-controlled context.
        #[cfg(feature = "memory")]
        let effective_system: String = {
            if user_query.len() >= INJECTION_MIN_QUERY_LEN {
                if let Some(ref store) = self.memory_store {
                    match store
                        .search(
                            &user_query,
                            None,
                            Some(&self.session.user_id),
                            INJECTION_MAX_MEMORIES,
                        )
                        .await
                    {
                        Ok(memories) if !memories.is_empty() => {
                            // Touch each injected memory (fire-and-forget, non-fatal).
                            for m in &memories {
                                let id = m.id;
                                let store_clone = std::sync::Arc::clone(store);
                                tokio::spawn(async move {
                                    let _ = store_clone.touch(id).await;
                                });
                            }
                            let injection = prompt::format_memory_injection(&memories);
                            info!(
                                memory_count = memories.len(),
                                "memory injection: surfaced relevant memories"
                            );
                            format!("{}{}", self.system_prompt, injection)
                        }
                        Ok(_) => self.system_prompt.clone(),
                        Err(e) => {
                            warn!(
                                error = %e,
                                "memory injection search failed, proceeding without injection"
                            );
                            self.system_prompt.clone()
                        }
                    }
                } else {
                    self.system_prompt.clone()
                }
            } else {
                self.system_prompt.clone()
            }
        };
        #[cfg(not(feature = "memory"))]
        let effective_system: String = self.system_prompt.clone();

        // Context compaction: summarize older messages if the context window is filling up.
        // Runs before the iteration loop — mid-turn compaction would break tool_use/tool_result.
        self.maybe_compact(&effective_system).await?;

        for iteration in 0..MAX_ITERATIONS {
            let _iter_span = info_span!("iteration", n = iteration);

            // Hard-stop safety net: if mid-turn tool results pushed us past 95%
            // of the context window, force compaction before the next API call.
            if self.session.messages.len() >= COMPACTION_MIN_MESSAGES {
                let input_tokens = self.last_usage.map(|u| u.input_tokens).unwrap_or_else(|| {
                    tokens::estimate_tokens(
                        &effective_system,
                        &self.session.messages,
                        &self.tool_definitions,
                    )
                });
                let window = tokens::context_window_size(self.provider.model_name());
                let hard_stop = (window as f32 * HARD_STOP_RATIO) as u32;
                if input_tokens > hard_stop {
                    warn!(
                        input_tokens,
                        hard_stop,
                        iteration,
                        "hard-stop: context window near capacity, compacting mid-turn"
                    );
                    self.maybe_compact(&effective_system).await?;
                }
            }

            // Hook: before_provider_call.
            hooks::dispatch_before_provider_call(
                &self.hooks,
                &hooks::ProviderCallContext {
                    system_prompt: &effective_system,
                    messages: &self.session.messages,
                    iteration,
                },
            )
            .await;

            let (assistant_msg, usage) = self
                .provider
                .complete(
                    &effective_system,
                    &self.session.messages,
                    &self.tool_definitions,
                )
                .await?;

            if let Some(u) = usage {
                self.last_usage = Some(u);
                #[cfg(feature = "postgres")]
                self.record_cost(u, CallType::Inference).await;
            }

            let (content, stop_reason) = match assistant_msg {
                Message::Assistant {
                    content,
                    stop_reason,
                } => (content, stop_reason),
                _ => return Err(CherubError::Provider("unexpected message type".to_owned())),
            };

            // Hook: after_provider_call.
            hooks::dispatch_after_provider_call(
                &self.hooks,
                &hooks::ProviderResponseContext {
                    content: &content,
                    stop_reason,
                    usage,
                },
            )
            .await;

            // Emit text blocks and collect tool_use blocks.
            // The first non-empty text block before any ToolUse is emitted as
            // Recapitulation (M14b) so sinks can style it distinctly.
            let mut tool_uses = Vec::new();
            let mut first_text_emitted = false;
            let mut tool_use_seen = false;
            for block in &content {
                match block {
                    ContentBlock::Text { text } if !text.is_empty() => {
                        if !first_text_emitted && !tool_use_seen {
                            self.output.emit(OutputEvent::Recapitulation(text)).await;
                            first_text_emitted = true;
                        } else {
                            self.output.emit(OutputEvent::Text(text)).await;
                        }
                    }
                    ContentBlock::Text { .. } => {}
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_use_seen = true;
                        tool_uses.push((id.clone(), name.clone(), input.clone()));
                    }
                    ContentBlock::Thinking { thinking } => {
                        tracing::debug!(len = thinking.len(), "thinking block");
                        if self.show_thinking {
                            self.output.emit(OutputEvent::Thinking(thinking)).await;
                        }
                    }
                    ContentBlock::RedactedThinking { .. } => {
                        tracing::debug!("redacted thinking block");
                    }
                }
            }

            // Warn the user if the model's response was truncated.
            if stop_reason == StopReason::MaxTokens {
                warn!(iteration, "model response truncated (max_tokens reached)");
                self.output
                    .emit(OutputEvent::Warning(
                        "Model response was truncated — output may be incomplete.",
                    ))
                    .await;
            }

            self.session.push(Message::Assistant {
                content,
                stop_reason,
            });
            #[cfg(feature = "sessions")]
            self.session.persist_last().await;

            if stop_reason != StopReason::ToolUse || tool_uses.is_empty() {
                return Ok(());
            }

            // Construct context for this tool execution cycle.
            let ctx = ToolContext {
                user_id: self.session.user_id.clone(),
                session_id: self.session.id,
                turn_number: self.session.next_ordinal,
            };

            // Build budget context for enforcement (M12).
            // Only queries costs that are actually configured in the budget.
            #[cfg(feature = "postgres")]
            let budget_ctx = {
                use crate::enforcement::BudgetContext;

                if let (Some(store), Some(budget)) = (&self.cost_store, &self.policy.budget) {
                    let session_cost = if budget.session_limit_usd.is_some() {
                        store
                            .session_cost(self.session.id)
                            .await
                            .map(|s| s.total_cost_usd)
                            .unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    let daily_cost = if budget.daily_limit_usd.is_some() {
                        let today = chrono::Utc::now()
                            .date_naive()
                            .and_hms_opt(0, 0, 0)
                            .unwrap()
                            .and_utc();
                        store
                            .period_cost(&ctx.user_id, today)
                            .await
                            .map(|s| s.total_cost_usd)
                            .unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    Some(BudgetContext {
                        session_cost_usd: session_cost,
                        daily_cost_usd: daily_cost,
                    })
                } else {
                    None
                }
            };
            #[cfg(not(feature = "postgres"))]
            let budget_ctx: Option<crate::enforcement::BudgetContext> = None;

            // Process tool calls through enforcement
            for (tool_use_id, name, input) in tool_uses {
                // Map composite tool name → enforcement policy name (MCP: server name).
                let enforcement_name = self.registry.enforcement_name(&name);
                // Enrich params with MCP metadata for McpStructured extraction.
                let enriched = self.registry.enrich_params(&name, &input);

                // Build a display string that works for any tool type.
                let display_str = enriched
                    .get("command")
                    .or_else(|| enriched.get("action"))
                    .or_else(|| enriched.get("__mcp_tool"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("<no action>")
                    .to_owned();
                let display_str = display_str.as_str();

                let proposal =
                    ToolInvocation::<Proposed>::new(enforcement_name, "execute", enriched);
                let (mut evaluated, decision) =
                    enforcement::evaluate(proposal, &self.policy, budget_ctx.as_ref());
                // Restore original composite name for registry lookup.
                evaluated.tool = name.clone();

                match decision {
                    Decision::Allow(token) => {
                        #[cfg(feature = "postgres")]
                        let tier_str = token.tier.as_str().to_owned();
                        info!(decision = "ALLOWED", tool = %name, action = %display_str);
                        self.output
                            .emit(OutputEvent::ToolAllowed {
                                tool: &name,
                                command: display_str,
                            })
                            .await;

                        // Hook: before_tool_call (fires only for allowed tools).
                        hooks::dispatch_before_tool_call(
                            &self.hooks,
                            &hooks::BeforeToolCallContext {
                                tool: &name,
                                action: display_str,
                                params: &evaluated.params,
                            },
                        )
                        .await;

                        let exec_start = Instant::now();
                        match self
                            .execute_with_progress(
                                &name,
                                display_str,
                                evaluated.execute(token, &self.registry, &ctx),
                            )
                            .await
                        {
                            Ok(result) => {
                                let duration_ms = exec_start.elapsed().as_millis() as i64;
                                info!(duration_ms = %duration_ms, "tool execution complete");
                                #[cfg(feature = "postgres")]
                                self.audit(NewAuditEvent {
                                    session_id: Some(ctx.session_id),
                                    user_id: ctx.user_id.clone(),
                                    turn_number: Some(ctx.turn_number),
                                    tool: name.clone(),
                                    action: Some(display_str.to_owned()),
                                    decision: AuditDecision::Allow,
                                    tier: Some(tier_str),
                                    duration_ms: Some(duration_ms),
                                    is_error: Some(false),
                                })
                                .await;
                                // Record sub-agent cost under the sub-agent's model name (M13d).
                                #[cfg(feature = "postgres")]
                                if let Some((ref model_name, ref usage)) = result.sub_agent_usage
                                    && let Some(ref store) = self.cost_store
                                {
                                    use crate::providers::pricing;
                                    let cost_usd = pricing::lookup_pricing(
                                        &self.pricing_table,
                                        model_name,
                                    )
                                    .map_or(0.0, |p| pricing::compute_cost(usage, &p));
                                    if let Err(e) = store
                                        .record(NewTokenUsage {
                                            session_id: Some(self.session.id),
                                            user_id: self.session.user_id.clone(),
                                            turn_number: Some(self.session.next_ordinal),
                                            model_name: model_name.clone(),
                                            input_tokens: usage.input_tokens,
                                            output_tokens: usage.output_tokens,
                                            cost_usd,
                                            call_type: CallType::Inference,
                                        })
                                        .await
                                    {
                                        warn!(
                                            error = %e,
                                            "sub-agent cost recording failed (non-fatal)"
                                        );
                                    }
                                }
                                // Hook: after_tool_call (success path).
                                let result_images = result.images;
                                let mut output = result.output;
                                hooks::dispatch_after_tool_call(
                                    &self.hooks,
                                    &mut hooks::AfterToolCallContext {
                                        tool: &name,
                                        action: display_str,
                                        result: &mut output,
                                        is_error: false,
                                    },
                                )
                                .await;
                                if !output.is_empty() {
                                    self.output.emit(OutputEvent::ToolOutput(&output)).await;
                                }
                                let images = result_images
                                    .into_iter()
                                    .map(|img| ToolResultImage {
                                        media_type: img.media_type,
                                        data: img.data,
                                    })
                                    .collect();
                                self.session.push(Message::ToolResult {
                                    tool_use_id,
                                    content: output,
                                    images,
                                    is_error: false,
                                });
                                #[cfg(feature = "sessions")]
                                self.session.persist_last().await;
                            }
                            Err(e) => {
                                let duration_ms = exec_start.elapsed().as_millis() as i64;
                                let err_msg = e.to_string();
                                warn!(duration_ms = %duration_ms, error = %err_msg, "tool execution failed");
                                #[cfg(feature = "postgres")]
                                self.audit(NewAuditEvent {
                                    session_id: Some(ctx.session_id),
                                    user_id: ctx.user_id.clone(),
                                    turn_number: Some(ctx.turn_number),
                                    tool: name.clone(),
                                    action: Some(display_str.to_owned()),
                                    decision: AuditDecision::Allow,
                                    tier: Some(tier_str),
                                    duration_ms: Some(duration_ms),
                                    is_error: Some(true),
                                })
                                .await;
                                // Hook: after_tool_call (error path).
                                let mut err_output = err_msg;
                                hooks::dispatch_after_tool_call(
                                    &self.hooks,
                                    &mut hooks::AfterToolCallContext {
                                        tool: &name,
                                        action: display_str,
                                        result: &mut err_output,
                                        is_error: true,
                                    },
                                )
                                .await;
                                self.output.emit(OutputEvent::ToolError(&err_output)).await;
                                self.session.push(Message::ToolResult {
                                    tool_use_id,
                                    content: err_output,
                                    images: vec![],
                                    is_error: true,
                                });
                                #[cfg(feature = "sessions")]
                                self.session.persist_last().await;
                            }
                        }
                    }
                    Decision::Reject => {
                        info!(decision = "REJECTED", tool = %name, action = %display_str);
                        #[cfg(feature = "postgres")]
                        self.audit(NewAuditEvent {
                            session_id: Some(ctx.session_id),
                            user_id: ctx.user_id.clone(),
                            turn_number: Some(ctx.turn_number),
                            tool: name.clone(),
                            action: Some(display_str.to_owned()),
                            decision: AuditDecision::Reject,
                            tier: None,
                            duration_ms: None,
                            is_error: None,
                        })
                        .await;
                        self.output
                            .emit(OutputEvent::ToolRejected {
                                tool: &name,
                                command: display_str,
                            })
                            .await;
                        self.session.push(Message::ToolResult {
                            tool_use_id,
                            content: "action not permitted".to_owned(),
                            images: vec![],
                            is_error: true,
                        });
                        #[cfg(feature = "sessions")]
                        self.session.persist_last().await;
                    }
                    Decision::Escalate { tier } => {
                        #[cfg(feature = "postgres")]
                        let tier_str = tier.as_str().to_owned();
                        info!(decision = "ESCALATED", tool = %name, action = %display_str);
                        #[cfg(feature = "postgres")]
                        self.audit(NewAuditEvent {
                            session_id: Some(ctx.session_id),
                            user_id: ctx.user_id.clone(),
                            turn_number: Some(ctx.turn_number),
                            tool: name.clone(),
                            action: Some(display_str.to_owned()),
                            decision: AuditDecision::Escalate,
                            tier: Some(tier_str.clone()),
                            duration_ms: None,
                            is_error: None,
                        })
                        .await;

                        let context = EscalationContext {
                            tool: &name,
                            command: display_str,
                            // Use enriched params (MCP metadata injected) so that
                            // drain_approved_tasks() re-evaluation succeeds on MCP tools.
                            params: &evaluated.params,
                            #[cfg(feature = "postgres")]
                            autonomous: self.autonomous_mode,
                            #[cfg(not(feature = "postgres"))]
                            autonomous: false,
                        };
                        match self.approval_gate.request_approval(&context).await {
                            ApprovalResult::Approved => {
                                let token = enforcement::approve_escalation(tier);
                                info!(decision = "APPROVED", tool = %name, action = %display_str);
                                self.output
                                    .emit(OutputEvent::ToolApproved {
                                        tool: &name,
                                        command: display_str,
                                    })
                                    .await;

                                // Hook: before_tool_call (fires only for approved tools).
                                hooks::dispatch_before_tool_call(
                                    &self.hooks,
                                    &hooks::BeforeToolCallContext {
                                        tool: &name,
                                        action: display_str,
                                        params: &evaluated.params,
                                    },
                                )
                                .await;

                                let exec_start = Instant::now();
                                match self
                                    .execute_with_progress(
                                        &name,
                                        display_str,
                                        evaluated.execute(token, &self.registry, &ctx),
                                    )
                                    .await
                                {
                                    Ok(result) => {
                                        let duration_ms = exec_start.elapsed().as_millis() as i64;
                                        info!(duration_ms = %duration_ms, "tool execution complete");
                                        #[cfg(feature = "postgres")]
                                        self.audit(NewAuditEvent {
                                            session_id: Some(ctx.session_id),
                                            user_id: ctx.user_id.clone(),
                                            turn_number: Some(ctx.turn_number),
                                            tool: name.clone(),
                                            action: Some(display_str.to_owned()),
                                            decision: AuditDecision::Approve,
                                            tier: Some(tier_str.clone()),
                                            duration_ms: Some(duration_ms),
                                            is_error: Some(false),
                                        })
                                        .await;
                                        // Hook: after_tool_call (approved success path).
                                        let result_images = result.images;
                                        let mut output = result.output;
                                        hooks::dispatch_after_tool_call(
                                            &self.hooks,
                                            &mut hooks::AfterToolCallContext {
                                                tool: &name,
                                                action: display_str,
                                                result: &mut output,
                                                is_error: false,
                                            },
                                        )
                                        .await;
                                        if !output.is_empty() {
                                            self.output
                                                .emit(OutputEvent::ToolOutput(&output))
                                                .await;
                                        }
                                        let images = result_images
                                            .into_iter()
                                            .map(|img| ToolResultImage {
                                                media_type: img.media_type,
                                                data: img.data,
                                            })
                                            .collect();
                                        self.session.push(Message::ToolResult {
                                            tool_use_id,
                                            content: output,
                                            images,
                                            is_error: false,
                                        });
                                        #[cfg(feature = "sessions")]
                                        self.session.persist_last().await;
                                    }
                                    Err(e) => {
                                        let duration_ms = exec_start.elapsed().as_millis() as i64;
                                        let err_msg = e.to_string();
                                        warn!(duration_ms = %duration_ms, error = %err_msg, "tool execution failed");
                                        #[cfg(feature = "postgres")]
                                        self.audit(NewAuditEvent {
                                            session_id: Some(ctx.session_id),
                                            user_id: ctx.user_id.clone(),
                                            turn_number: Some(ctx.turn_number),
                                            tool: name.clone(),
                                            action: Some(display_str.to_owned()),
                                            decision: AuditDecision::Approve,
                                            tier: Some(tier_str.clone()),
                                            duration_ms: Some(duration_ms),
                                            is_error: Some(true),
                                        })
                                        .await;
                                        // Hook: after_tool_call (approved error path).
                                        let mut err_output = err_msg;
                                        hooks::dispatch_after_tool_call(
                                            &self.hooks,
                                            &mut hooks::AfterToolCallContext {
                                                tool: &name,
                                                action: display_str,
                                                result: &mut err_output,
                                                is_error: true,
                                            },
                                        )
                                        .await;
                                        self.output.emit(OutputEvent::ToolError(&err_output)).await;
                                        self.session.push(Message::ToolResult {
                                            tool_use_id,
                                            content: err_output,
                                            images: vec![],
                                            is_error: true,
                                        });
                                        #[cfg(feature = "sessions")]
                                        self.session.persist_last().await;
                                    }
                                }
                            }
                            ApprovalResult::Denied => {
                                info!(decision = "DENIED", tool = %name, action = %display_str);
                                #[cfg(feature = "postgres")]
                                self.audit(NewAuditEvent {
                                    session_id: Some(ctx.session_id),
                                    user_id: ctx.user_id.clone(),
                                    turn_number: Some(ctx.turn_number),
                                    tool: name.clone(),
                                    action: Some(display_str.to_owned()),
                                    decision: AuditDecision::Deny,
                                    tier: Some(tier_str),
                                    duration_ms: None,
                                    is_error: None,
                                })
                                .await;
                                self.output
                                    .emit(OutputEvent::ToolDenied {
                                        tool: &name,
                                        command: display_str,
                                    })
                                    .await;
                                // Policy opacity: identical message to Reject
                                self.session.push(Message::ToolResult {
                                    tool_use_id,
                                    content: "action not permitted".to_owned(),
                                    images: vec![],
                                    is_error: true,
                                });
                                #[cfg(feature = "sessions")]
                                self.session.persist_last().await;
                            }
                            ApprovalResult::Queued(task_id) => {
                                info!(
                                    decision = "QUEUED",
                                    tool = %name,
                                    action = %display_str,
                                    %task_id,
                                    "action queued for async approval"
                                );
                                #[cfg(feature = "postgres")]
                                self.audit(NewAuditEvent {
                                    session_id: Some(ctx.session_id),
                                    user_id: ctx.user_id.clone(),
                                    turn_number: Some(ctx.turn_number),
                                    tool: name.clone(),
                                    action: Some(display_str.to_owned()),
                                    decision: AuditDecision::Escalate,
                                    tier: Some(tier_str),
                                    duration_ms: None,
                                    is_error: None,
                                })
                                .await;
                                // Inform the model that the action has been queued.
                                // The model can continue with other work items.
                                self.session.push(Message::ToolResult {
                                    tool_use_id,
                                    content: format!(
                                        "Action queued for your approval (task id: {task_id}). \
                                         I'll execute it once you approve the Telegram notification. \
                                         Continuing with other work."
                                    ),
                                    images: vec![],
                                    is_error: false,
                                });
                                #[cfg(feature = "sessions")]
                                self.session.persist_last().await;
                            }
                        }
                    }
                }
            }

            if iteration == MAX_ITERATIONS - 1 {
                warn!(
                    max_iterations = MAX_ITERATIONS,
                    "reached max iterations, stopping turn"
                );
                self.output
                    .emit(OutputEvent::Warning(
                        "Reached maximum iterations, stopping.",
                    ))
                    .await;
            }
        }

        Ok(())
    }

    /// Execute a tool future with periodic progress updates (M14c).
    async fn execute_with_progress(
        &self,
        tool_name: &str,
        display_str: &str,
        fut: impl Future<Output = Result<crate::tools::ToolResult, CherubError>>,
    ) -> Result<crate::tools::ToolResult, CherubError> {
        self.output
            .emit(OutputEvent::Progress {
                tool: tool_name,
                status: display_str,
            })
            .await;
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.tick().await; // skip immediate tick
        tokio::pin!(fut);
        let mut elapsed = 0u64;
        loop {
            tokio::select! {
                result = &mut fut => {
                    self.output.emit(OutputEvent::ProgressClear).await;
                    return result;
                }
                _ = interval.tick() => {
                    elapsed += 5;
                    let status = format!("{display_str} ({elapsed}s)");
                    self.output.emit(OutputEvent::Progress { tool: tool_name, status: &status }).await;
                }
            }
        }
    }
}

#[cfg(all(test, feature = "memory"))]
mod tests {
    use super::*;

    #[test]
    fn extract_user_text_single_text() {
        let content = vec![UserContent::Text("hello world".to_owned())];
        assert_eq!(extract_user_text(&content), "hello world");
    }

    #[test]
    fn extract_user_text_multiple_text_joined() {
        let content = vec![
            UserContent::Text("hello".to_owned()),
            UserContent::Text("world".to_owned()),
        ];
        assert_eq!(extract_user_text(&content), "hello world");
    }

    #[test]
    fn extract_user_text_skips_images() {
        let content = vec![
            UserContent::Text("describe this".to_owned()),
            UserContent::Image {
                media_type: "image/png".to_owned(),
                data: "base64data".to_owned(),
            },
        ];
        assert_eq!(extract_user_text(&content), "describe this");
    }

    #[test]
    fn extract_user_text_empty_content() {
        assert_eq!(extract_user_text(&[]), "");
    }

    #[test]
    fn extract_user_text_image_only() {
        let content = vec![UserContent::Image {
            media_type: "image/jpeg".to_owned(),
            data: "abc".to_owned(),
        }];
        assert_eq!(extract_user_text(&content), "");
    }
}
