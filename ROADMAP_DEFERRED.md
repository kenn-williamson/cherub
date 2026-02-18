# Deferred Items from Milestone 2

Items discovered or explicitly deferred during M2 implementation that are **not already tracked** in ROADMAP.md milestones (M3–M7 and Beyond MVP).

## Streaming Responses

M2 uses non-streaming API (`"stream": false`). Add SSE streaming for CLI UX so text appears incrementally. Write a minimal SSE parser (~50 lines) over `reqwest::Response::bytes_stream()` — avoid `reqwest-eventsource` (unmaintained, pins old reqwest).

## Extended Thinking Support

Anthropic `thinking` content blocks are ignored. Add when needed for complex reasoning tasks.

## Context Window Management

Session grows unbounded in memory. Need pruning/summarization strategy before sessions get long.

## Parallel Tool Execution

When the model returns multiple `tool_use` blocks, they're executed sequentially. Consider parallel execution for independent commands.

## `expose_secret()` Migration

API key `expose_secret()` is in the provider module. Must move to credential broker in M6.

## Dynamic Dispatch for Provider/Tool

Using concrete types and enum dispatch. Switch to `dyn Trait` (via `async-trait` or `Pin<Box<dyn Future>>`) in M7 when plugin IPC requires it.

## API Error Retry

Network/rate-limit failures surface to the user. No automatic retry with backoff.

## Token Usage Tracking

Anthropic response contains `usage` data; currently ignored. Track for cost awareness.

## Per-Session Working Directory

Bash commands run in the binary's CWD. No `cd` tracking or per-session isolation.

## Streaming Cancellation

No way to interrupt a streaming response mid-turn (e.g., Ctrl-C during model output).

## Output Formatting

Raw text output. No markdown rendering, no syntax highlighting, no colored diffs.

## Stateful Constraints

Stateless constraints (field comparisons, containment checks) are planned for M3. Stateful constraints require an `EnforcementState` struct that tracks cumulative behavior across invocations:

- **Daily/hourly sum tracking** — "no more than $100 in transfers today." Requires time-windowed accumulators per action, per field.
- **Action rate limiting** — "no more than 10 buy orders per hour." Requires counters with time decay.
- **Monotonic budget tracking** — "total spend across all actions must not exceed $500 for this task." Requires per-task running totals.

`evaluate()` signature changes from `(proposal, &policy) -> decision` to `(proposal, &policy, &mut state) -> decision`. The state struct needs persistence strategy (in-memory for single-session, serialized for multi-session).

## Telegram Output Verbosity Modes

The Telegram sink currently emits every `OutputEvent` as a separate message (tool allowed/rejected, tool output, errors, etc.), giving a play-by-play of agent execution. This is useful for debugging but noisy for end users. Add configurable verbosity modes:

1. **Summary mode** (default for users) — Buffer all events during a turn, send only the final `Text` response. Tool calls are invisible to the user.
2. **Progress mode** — Send a "typing..." indicator or single status message while the agent works, then replace/follow with the final answer.
3. **Collapsible detail mode** — Send the final answer as the main message, with an inline keyboard "Show details" button that reveals tool calls, outputs, and enforcement decisions.

Current behavior becomes **Debug mode** — preserved as-is for development and troubleshooting. Mode selection could be per-chat (via `/verbose`, `/quiet` commands) or per-policy config.

## Per-Task Dynamic Constraints

Stateless per-tool and per-action constraints come from the policy file (static, operator-set). Per-task constraints are dynamic and session-scoped — they come from the conversation between the user and agent.

Flow: user describes task in natural language → agent extracts structured constraints → connector renders them in medium-appropriate format (Telegram message, Discord embed, CLI table) → user confirms → constraints locked into enforcement layer for the session.

Key design concerns:
- **Trust gap** — The agent interprets natural language into structured constraints. It could misinterpret or weaken them. The confirmation gate is the trust anchor: the user sees structured predicates, not the agent's paraphrase.
- **Constraint modification** — Once confirmed, constraints are immutable for the session. The user can request a new constraint set (re-confirmation required), but the agent cannot unilaterally modify them.
- **Connector-agnostic representation** — The enforcement layer sees `Constraint { field, op, value, on_failure }` regardless of whether confirmation happened via Telegram, Discord, or CLI. Presentation is the connector's responsibility.
- **Interaction with policy constraints** — Task constraints are additive. They can further restrict what the policy allows but cannot relax policy constraints. A policy that says "max $500 per buy" cannot be overridden by a task constraint of "max $1000 per buy."
