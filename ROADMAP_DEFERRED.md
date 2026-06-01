# Deferred Items

Items explicitly deferred from active milestones. Each entry notes *why* it was deferred, so future-us can judge whether the original reason still holds.

---

## M16 — Smart Routing (deferred indefinitely)

Originally: automatically route simple queries to cheaper/faster models via a complexity scorer (0–100) and a `SmartRoutingProvider` decorator.

**Why deferred:** M13d sub-agents are the correct pattern — the frontier model reasons first, then delegates bounded subtasks to cheaper models as tool calls. Smart routing inverts this: a cheap routing decision precedes the frontier model's reasoning, which introduces anchoring risk (the weak model's framing influences the frontier model's output). The complexity-scorer approach is also a maintenance burden with configurable-weights TOML that adds complexity without a clear win. If cheap-model delegation is needed, wire it as a sub-agent tool.

---

## M17 — Response Caching (deferred indefinitely)

Originally: in-memory LRU/TTL cache for non-tool-calling LLM responses, keyed by SHA-256 of (model, messages, tool_definitions).

**Why deferred:** Most agent turns involve tool calls, which can't be cached. Anthropic prompt caching already handles API-level repetition at a lower level. The SHA-256 key includes tool definitions, so any tool registration change invalidates the entire cache. The complexity cost (TTL management, LRU eviction, cache invalidation) doesn't pay off for agentic workloads. Revisit only if a chatbot-style use case (repeated identical non-tool queries) emerges.

---

## M18b — *User-facing* SSE Streaming (deferred indefinitely)

Originally: SSE streaming for CLI so text appears incrementally; `complete_streaming()` on the `Provider` trait; Anthropic + OpenAI SSE parsers.

**Why deferred:** Messaging connectors (Telegram, Discord) never benefit from streaming — messages are batched per-turn anyway. There is no web UI and no heavy CLI focus. The *incremental-display* complexity (surfacing partial tokens, backpressure) isn't justified. Revisit only when a web UI is built.

**Update — wire-level streaming shipped:** The Anthropic provider now uses `stream: true` on the wire and consumes the SSE internally (a self-contained parser over `reqwest::Response::bytes_stream()` — no `reqwest-eventsource`), reassembling a complete `Message`. This was done for *reliability*, not UX: a per-chunk idle timeout replaces the total request timeout, so long-but-healthy generations (e.g. extended thinking) no longer fail. The `Provider` trait is unchanged (still returns a complete `Message`). So the SSE parser already exists; only the user-facing incremental display + the `complete_streaming()` trait method + OpenAI's parser remain deferred.

---

## Items Deferred from Milestone 2

Items discovered or explicitly deferred during M2 implementation.

## Streaming Responses (M2-era) — partially resolved

M2 used the non-streaming API (`"stream": false`). The wire-level half of this is now **done**: the Anthropic provider streams (`stream: true`) and parses SSE with a self-contained parser over `reqwest::Response::bytes_stream()` (no `reqwest-eventsource`, as advised here). What remains is the *CLI UX* part — text appearing incrementally — which is deferred (see M18b above).

## Extended Thinking Support — implemented

No longer deferred. Anthropic extended thinking is configurable per provider
(`thinking_budget` in providers config → `AnthropicProvider::with_thinking_budget`),
`thinking`/`redacted_thinking` blocks are parsed (both the streaming and
non-streaming paths) and handled in `runtime/prompt.rs`. Kept here as a record;
remove once confirmed stable.

## `expose_secret()` Migration

API key `expose_secret()` is in the provider module. Must move to credential broker in M6.

## Dynamic Dispatch for Provider/Tool

Using concrete types and enum dispatch. Switch to `dyn Trait` (via `async-trait` or `Pin<Box<dyn Future>>`) in M7 when plugin IPC requires it.

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

## Memory Reconciliation / Admin Panel

Contradiction detection (M6) surfaces similar memories to the agent during writes. What's missing is a broader admin panel for memory management:

- Bulk memory reconciliation: scan all memories for a user and surface clusters of potentially contradictory entries
- Memory timeline view: show the `superseded_by` chain for a given memory path
- Memory merge: combine two memories into one (with provenance from both)
- `op_update()` contradiction check: currently deferred because it needs `get_by_id()` on `MemoryStore` to load scope/user_id from the existing memory
- Admin CLI: `cherub memory list/search/reconcile` subcommands for operator use

## Image / Screenshot Token Normalization

Browser screenshots are the most token-expensive thing a turn can carry, and the runtime currently neither bounds nor accurately accounts for them. Anthropic bills images at roughly `(width × height) / 750` tokens — a 1920×1080 screenshot is ~2,765 tokens — but `tokens::estimate_tokens` counts a flat 1,000 per image (`runtime/tokens.rs`). Two problems compound:

1. **Undercount → late compaction.** The estimator thinks a screenshot-heavy context is ~2.7× smaller than it is, so compaction fires late and the context runs bigger (and more expensive) than intended.
2. **Unbounded persistence.** Each screenshot stays in message history for the rest of the session and is re-sent every turn (at the cache-read rate if the prefix is intact, full rate if not). A 10-screenshot browse session can carry ~27K tokens of images on every subsequent turn.

Levers to weigh (this is why it's deferred — it needs design, not just a patch):
- **Downscale before encoding** — cap the long edge (e.g. 1024–1280px) and/or switch PNG→JPEG in `tools/container/browser` before the image crosses IPC. Trades visual fidelity for tokens; needs a quality threshold the model can still read.
- **Evict stale screenshots from history** — replace all-but-the-most-recent-N images with a text stub ("[screenshot from step 3 elided]"). Overlaps with the broader tool-result/context-editing lever; Anthropic's native context-editing may cover this.
- **Accurate estimation** — compute the image token cost from actual dimensions instead of the flat 1,000, so compaction triggers on time.

Open questions: where downscaling belongs (browser container vs. runtime ingest), whether eviction should be image-specific or part of a general tool-result eviction pass, and how to keep the most-decision-relevant screenshot when evicting. Pairs naturally with the prompt-cache work (`feat/prompt-cache-stability`) — both are about keeping the re-sent prefix cheap.
