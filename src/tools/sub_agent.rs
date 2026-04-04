//! Sub-agent tool: bounded inner loop for delegating work to cheaper models.
//!
//! Each configured sub-agent becomes a tool the orchestrator can invoke. The
//! orchestrator sees the `description` and decides when to delegate. This enables
//! the "frontier orchestrator + local model as tool" pattern, avoiding anchoring
//! bias (proven unfixable in draft/review patterns).
//!
//! Key design properties:
//! - Owns its own `Box<dyn Provider>` and `ToolRegistry`
//! - Registry contains only base tools (never other sub-agents — prevents recursion)
//! - Escalations auto-rejected (sub-agents cannot escalate to human)
//! - Bounded by `max_turns` and `timeout`
//! - Cost attribution via `ToolResult.sub_agent_usage`

use std::time::Duration;

use serde_json::json;
use tracing::{info, info_span, warn};

use crate::enforcement::policy::Policy;
use crate::enforcement::{self, Decision};
use crate::error::CherubError;
use crate::providers::{
    ApiUsage, ContentBlock, Message, Provider, StopReason, ToolDefinition, UserContent,
};
use crate::tools::{Proposed, ToolContext, ToolInvocation, ToolRegistry, ToolResult};

/// A sub-agent tool that delegates work to a cheaper/local model.
pub struct SubAgentTool {
    pub name: String,
    pub description: String,
    pub provider: Box<dyn Provider>,
    pub system_prompt: String,
    pub max_turns: u32,
    pub timeout: Duration,
    pub registry: ToolRegistry,
    pub policy: Policy,
}

impl SubAgentTool {
    /// Execute the sub-agent with the given input.
    pub async fn execute(
        &self,
        params: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, CherubError> {
        let input = params
            .get("input")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CherubError::InvalidInvocation("sub-agent requires 'input' parameter".to_owned())
            })?;

        let _span = info_span!("sub_agent", name = %self.name);
        info!(
            input_len = input.len(),
            max_turns = self.max_turns,
            "sub-agent invoked"
        );

        let tool_defs = self.registry.definitions();
        let mut messages: Vec<Message> = vec![Message::User {
            content: vec![UserContent::Text(input.to_owned())],
        }];
        let mut cumulative_usage = ApiUsage::new(0, 0);
        let mut collected_text: Vec<String> = Vec::new();

        let result = tokio::time::timeout(self.timeout, async {
            for turn in 0..self.max_turns {
                let _turn_span = info_span!("sub_agent_turn", turn);

                let (response, usage) = self
                    .provider
                    .complete(&self.system_prompt, &messages, &tool_defs)
                    .await?;

                // Accumulate usage.
                if let Some(u) = usage {
                    cumulative_usage.input_tokens += u.input_tokens;
                    cumulative_usage.output_tokens += u.output_tokens;
                    cumulative_usage.cache_creation_tokens += u.cache_creation_tokens;
                    cumulative_usage.cache_read_tokens += u.cache_read_tokens;
                }

                let (content, stop_reason) = match response {
                    Message::Assistant {
                        content,
                        stop_reason,
                    } => (content, stop_reason),
                    _ => {
                        return Err(CherubError::Provider(
                            "sub-agent: unexpected message type".to_owned(),
                        ));
                    }
                };

                // Collect text blocks.
                let mut tool_uses = Vec::new();
                for block in &content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            collected_text.push(text.clone());
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_uses.push((id.clone(), name.clone(), input.clone()));
                        }
                        _ => {}
                    }
                }

                // Push the assistant message into conversation.
                messages.push(Message::Assistant {
                    content,
                    stop_reason,
                });

                if stop_reason != StopReason::ToolUse || tool_uses.is_empty() {
                    break;
                }

                // Process tool calls through enforcement.
                for (tool_use_id, tool_name, tool_input) in tool_uses {
                    let enforcement_name = self.registry.enforcement_name(&tool_name);
                    let enriched = self.registry.enrich_params(&tool_name, &tool_input);

                    let proposal =
                        ToolInvocation::<Proposed>::new(enforcement_name, "execute", enriched);
                    // No budget context for inner calls.
                    let (mut evaluated, decision) =
                        enforcement::evaluate(proposal, &self.policy, None);
                    // Restore original name for registry lookup.
                    evaluated.tool = tool_name.clone();

                    match decision {
                        Decision::Allow(token) => {
                            match evaluated.execute(token, &self.registry, ctx).await {
                                Ok(result) => {
                                    messages.push(Message::ToolResult {
                                        tool_use_id,
                                        content: result.output,
                                        images: vec![],
                                        is_error: false,
                                    });
                                }
                                Err(e) => {
                                    warn!(
                                        tool = %tool_name,
                                        error = %e,
                                        "sub-agent tool execution failed"
                                    );
                                    messages.push(Message::ToolResult {
                                        tool_use_id,
                                        content: e.to_string(),
                                        images: vec![],
                                        is_error: true,
                                    });
                                }
                            }
                        }
                        Decision::Reject | Decision::Escalate { .. } => {
                            // Sub-agents cannot escalate to human.
                            info!(
                                tool = %tool_name,
                                "sub-agent tool call rejected (escalations auto-rejected)"
                            );
                            messages.push(Message::ToolResult {
                                tool_use_id,
                                content: "action not permitted".to_owned(),
                                images: vec![],
                                is_error: true,
                            });
                        }
                    }
                }
            }
            Ok(())
        })
        .await;

        match result {
            Err(_elapsed) => {
                warn!(timeout_secs = self.timeout.as_secs(), "sub-agent timed out");
                collected_text.push("[sub-agent timed out]".to_owned());
            }
            Ok(Err(e)) => return Err(e),
            Ok(Ok(())) => {}
        }

        let output = collected_text.join("\n");
        let model_name = self.provider.model_name().to_owned();

        Ok(ToolResult {
            output,
            images: vec![],
            sub_agent_usage: Some((model_name, cumulative_usage)),
        })
    }

    /// Build a tool definition for the LLM to see.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "The task or query to send to this sub-agent"
                    }
                },
                "required": ["input"]
            }),
        }
    }
}
