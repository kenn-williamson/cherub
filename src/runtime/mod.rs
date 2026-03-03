pub mod approval;
pub mod output;
pub mod prompt;
pub mod session;
pub mod tokens;

use std::time::Instant;

use tracing::{info, info_span, warn};

use crate::enforcement::policy::Policy;
use crate::enforcement::{self, Decision};
use crate::error::CherubError;
use crate::providers::{
    ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition, UserContent,
};
use crate::tools::{Proposed, ToolContext, ToolInvocation, ToolRegistry};

use approval::{ApprovalGate, ApprovalResult, EscalationContext};
use output::{OutputEvent, OutputSink};
use session::Session;

#[cfg(feature = "postgres")]
use crate::storage::{AuditDecision, AuditStore, NewAuditEvent};

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
/// Generic over provider, approval gate, and output sink for testability.
pub struct AgentLoop<P: Provider, A: ApprovalGate, O: OutputSink> {
    session: Session,
    policy: Policy,
    provider: P,
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
}

impl<P: Provider, A: ApprovalGate, O: OutputSink> AgentLoop<P, A, O> {
    pub fn new(
        policy: Policy,
        provider: P,
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

        let (response, usage) = self
            .provider
            .complete(
                "You are a concise summarizer.",
                &summary_messages,
                &[], // No tools for summarization
            )
            .await?;

        if usage.is_some() {
            // Don't store — summarization usage shouldn't influence compaction threshold.
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

        let (response, _) = match result {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "memory flush extraction failed (non-fatal)");
                return;
            }
        };

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
            let (evaluated, decision) = enforcement::evaluate(proposal, &self.policy);

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

        // Extract text for injection query BEFORE content is moved into the session.
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

            let (assistant_msg, usage) = self
                .provider
                .complete(
                    &effective_system,
                    &self.session.messages,
                    &self.tool_definitions,
                )
                .await?;

            if usage.is_some() {
                self.last_usage = usage;
            }

            let (content, stop_reason) = match assistant_msg {
                Message::Assistant {
                    content,
                    stop_reason,
                } => (content, stop_reason),
                _ => return Err(CherubError::Provider("unexpected message type".to_owned())),
            };

            // Emit text blocks and collect tool_use blocks
            let mut tool_uses = Vec::new();
            for block in &content {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.is_empty() {
                            self.output.emit(OutputEvent::Text(text)).await;
                        }
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_uses.push((id.clone(), name.clone(), input.clone()));
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
                let (mut evaluated, decision) = enforcement::evaluate(proposal, &self.policy);
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

                        let exec_start = Instant::now();
                        match evaluated.execute(token, &self.registry, &ctx).await {
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
                                if !result.output.is_empty() {
                                    self.output
                                        .emit(OutputEvent::ToolOutput(&result.output))
                                        .await;
                                }
                                self.session.push(Message::ToolResult {
                                    tool_use_id,
                                    content: result.output,
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
                                self.output.emit(OutputEvent::ToolError(&err_msg)).await;
                                self.session.push(Message::ToolResult {
                                    tool_use_id,
                                    content: err_msg,
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
                            params: &input,
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

                                let exec_start = Instant::now();
                                match evaluated.execute(token, &self.registry, &ctx).await {
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
                                        if !result.output.is_empty() {
                                            self.output
                                                .emit(OutputEvent::ToolOutput(&result.output))
                                                .await;
                                        }
                                        self.session.push(Message::ToolResult {
                                            tool_use_id,
                                            content: result.output,
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
                                        self.output.emit(OutputEvent::ToolError(&err_msg)).await;
                                        self.session.push(Message::ToolResult {
                                            tool_use_id,
                                            content: err_msg,
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
                                    is_error: true,
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
