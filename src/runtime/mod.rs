pub mod approval;
pub mod output;
pub mod prompt;
pub mod session;

use std::time::Instant;

use tracing::{info, info_span, warn};

use crate::enforcement::policy::Policy;
use crate::enforcement::{self, Decision};
use crate::error::CherubError;
use crate::providers::{ContentBlock, Message, Provider, StopReason, ToolDefinition, UserContent};
use crate::tools::{Proposed, ToolContext, ToolInvocation, ToolRegistry};

use approval::{ApprovalGate, ApprovalResult, EscalationContext};
use output::{OutputEvent, OutputSink};
use session::Session;

const MAX_ITERATIONS: usize = 25;

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
    /// Optional shared memory store for proactive injection (M6d).
    /// When set, the runtime queries memories before each turn and injects
    /// the top results into the system prompt. The agent cannot suppress this.
    #[cfg(feature = "memory")]
    memory_store: Option<std::sync::Arc<dyn crate::storage::MemoryStore>>,
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
            #[cfg(feature = "memory")]
            memory_store: None,
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
        let effective_system: std::borrow::Cow<str> = {
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
                            std::borrow::Cow::Owned(format!("{}{}", self.system_prompt, injection))
                        }
                        Ok(_) => std::borrow::Cow::Borrowed(&self.system_prompt),
                        Err(e) => {
                            warn!(
                                error = %e,
                                "memory injection search failed, proceeding without injection"
                            );
                            std::borrow::Cow::Borrowed(&self.system_prompt)
                        }
                    }
                } else {
                    std::borrow::Cow::Borrowed(&self.system_prompt)
                }
            } else {
                std::borrow::Cow::Borrowed(&self.system_prompt)
            }
        };
        #[cfg(not(feature = "memory"))]
        let effective_system: &str = &self.system_prompt;

        for iteration in 0..MAX_ITERATIONS {
            let _iter_span = info_span!("iteration", n = iteration);

            let assistant_msg = self
                .provider
                .complete(
                    &effective_system,
                    &self.session.messages,
                    &self.tool_definitions,
                )
                .await?;

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
                // Build a display string that works for any tool type.
                let display_str = input
                    .get("command")
                    .or_else(|| input.get("action"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("<no action>");

                let proposal = ToolInvocation::<Proposed>::new(&name, "execute", input.clone());
                let (evaluated, decision) = enforcement::evaluate(proposal, &self.policy);

                match decision {
                    Decision::Allow(token) => {
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
                                info!(duration_ms = %exec_start.elapsed().as_millis(), "tool execution complete");
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
                                let err_msg = e.to_string();
                                warn!(duration_ms = %exec_start.elapsed().as_millis(), error = %err_msg, "tool execution failed");
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
                        info!(decision = "ESCALATED", tool = %name, action = %display_str);

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
                                        info!(duration_ms = %exec_start.elapsed().as_millis(), "tool execution complete");
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
                                        let err_msg = e.to_string();
                                        warn!(duration_ms = %exec_start.elapsed().as_millis(), error = %err_msg, "tool execution failed");
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
