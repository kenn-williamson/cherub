pub mod approval;
pub mod prompt;
pub mod session;

use std::time::Instant;

use tracing::{info, info_span, warn};

use crate::enforcement::policy::Policy;
use crate::enforcement::{self, Decision};
use crate::error::CherubError;
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::{ContentBlock, Message, StopReason, ToolDefinition};
use crate::tools::{Proposed, ToolInvocation, ToolRegistry};

use approval::{ApprovalResult, CliApprovalGate, EscalationContext};
use session::Session;

const MAX_ITERATIONS: usize = 25;

/// The agent loop. Owns session state and orchestrates model ↔ tool interaction.
pub struct AgentLoop {
    session: Session,
    policy: Policy,
    provider: AnthropicProvider,
    registry: ToolRegistry,
    system_prompt: String,
    tool_definitions: Vec<ToolDefinition>,
    approval_gate: CliApprovalGate,
}

impl AgentLoop {
    pub fn new(
        policy: Policy,
        provider: AnthropicProvider,
        registry: ToolRegistry,
        system_prompt: String,
        approval_gate: CliApprovalGate,
    ) -> Self {
        let tool_definitions = registry.definitions();
        Self {
            session: Session::new(),
            policy,
            provider,
            registry,
            system_prompt,
            tool_definitions,
            approval_gate,
        }
    }

    /// Run one user turn: push user message, call model, handle tool calls in a loop.
    pub async fn run_turn(&mut self, user_input: &str) -> Result<(), CherubError> {
        let _span = info_span!("turn").entered();

        self.session.push(Message::User {
            content: user_input.to_owned(),
        });

        for iteration in 0..MAX_ITERATIONS {
            let _iter_span = info_span!("iteration", n = iteration).entered();

            let assistant_msg = self
                .provider
                .complete(
                    &self.system_prompt,
                    self.session.messages(),
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

            // Print text blocks and collect tool_use blocks
            let mut tool_uses = Vec::new();
            for block in &content {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.is_empty() {
                            println!("{text}");
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

            if stop_reason != StopReason::ToolUse || tool_uses.is_empty() {
                return Ok(());
            }

            // Process tool calls through enforcement
            for (tool_use_id, name, input) in tool_uses {
                let command_str = input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<no command>");

                let proposal = ToolInvocation::<Proposed>::new(&name, "execute", input.clone());
                let (evaluated, decision) = enforcement::evaluate(proposal, &self.policy);

                match decision {
                    Decision::Allow(token) => {
                        let _exec_span =
                            info_span!("tool_exec", tool = %name, command = %command_str).entered();
                        info!(decision = "ALLOWED", tool = %name, command = %command_str);
                        println!("[ALLOWED] {name}: {command_str}");

                        let exec_start = Instant::now();
                        match evaluated.execute(token, &self.registry).await {
                            Ok(result) => {
                                info!(duration_ms = %exec_start.elapsed().as_millis(), "tool execution complete");
                                if !result.output.is_empty() {
                                    println!("{}", result.output);
                                }
                                self.session.push(Message::ToolResult {
                                    tool_use_id,
                                    content: result.output,
                                    is_error: false,
                                });
                            }
                            Err(e) => {
                                let err_msg = e.to_string();
                                warn!(duration_ms = %exec_start.elapsed().as_millis(), error = %err_msg, "tool execution failed");
                                println!("[ERROR] {err_msg}");
                                self.session.push(Message::ToolResult {
                                    tool_use_id,
                                    content: err_msg,
                                    is_error: true,
                                });
                            }
                        }
                    }
                    Decision::Reject => {
                        info!(decision = "REJECTED", tool = %name, command = %command_str);
                        println!("[REJECTED] {name}: {command_str}");
                        self.session.push(Message::ToolResult {
                            tool_use_id,
                            content: "action not permitted".to_owned(),
                            is_error: true,
                        });
                    }
                    Decision::Escalate { tier } => {
                        info!(decision = "ESCALATED", tool = %name, command = %command_str);

                        let context = EscalationContext {
                            tool: &name,
                            command: command_str,
                            params: &input,
                        };
                        match self.approval_gate.request_approval(&context).await {
                            ApprovalResult::Approved => {
                                let token = enforcement::approve_escalation(tier);
                                let _exec_span =
                                    info_span!("tool_exec", tool = %name, command = %command_str)
                                        .entered();
                                info!(decision = "APPROVED", tool = %name, command = %command_str);
                                println!("[APPROVED] {name}: {command_str}");

                                let exec_start = Instant::now();
                                match evaluated.execute(token, &self.registry).await {
                                    Ok(result) => {
                                        info!(duration_ms = %exec_start.elapsed().as_millis(), "tool execution complete");
                                        if !result.output.is_empty() {
                                            println!("{}", result.output);
                                        }
                                        self.session.push(Message::ToolResult {
                                            tool_use_id,
                                            content: result.output,
                                            is_error: false,
                                        });
                                    }
                                    Err(e) => {
                                        let err_msg = e.to_string();
                                        warn!(duration_ms = %exec_start.elapsed().as_millis(), error = %err_msg, "tool execution failed");
                                        println!("[ERROR] {err_msg}");
                                        self.session.push(Message::ToolResult {
                                            tool_use_id,
                                            content: err_msg,
                                            is_error: true,
                                        });
                                    }
                                }
                            }
                            ApprovalResult::Denied => {
                                info!(decision = "DENIED", tool = %name, command = %command_str);
                                println!("[DENIED] {name}: {command_str}");
                                // Policy opacity: identical message to Reject
                                self.session.push(Message::ToolResult {
                                    tool_use_id,
                                    content: "action not permitted".to_owned(),
                                    is_error: true,
                                });
                            }
                        }
                    }
                }
            }

            if iteration == MAX_ITERATIONS - 1 {
                warn!("reached max iterations ({MAX_ITERATIONS}), stopping turn");
                println!("[WARNING] Reached maximum iterations, stopping.");
            }
        }

        Ok(())
    }
}
