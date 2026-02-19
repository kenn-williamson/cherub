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
        }
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

        self.session.push(Message::User { content });
        #[cfg(feature = "sessions")]
        self.session.persist_last().await;

        for iteration in 0..MAX_ITERATIONS {
            let _iter_span = info_span!("iteration", n = iteration);

            let assistant_msg = self
                .provider
                .complete(
                    &self.system_prompt,
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
