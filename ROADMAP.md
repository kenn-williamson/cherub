# Cherub — MVP Roadmap

The goal is to prove the core thesis as fast as possible: **a Rust binary can enforce deterministic capability tiers on AI agent tool calls, and the model cannot bypass the enforcement layer.**

Everything else — connectors, credential brokering, audit logging, IPC plugins — is elaboration. The thesis comes first.

---

## Milestone 0: Skeleton

**Goal:** Cargo project compiles. Directory structure exists. You can `cargo build` and `cargo test` with no errors.

- [x] `cargo init` with workspace structure
- [x] Create module stubs: `runtime`, `enforcement`, `tools`, `providers`
- [x] Define core traits: `Tool`, `Provider`, `ToolProposal`, `CapabilityToken`
- [x] `CapabilityToken` with private constructor — verify it cannot be constructed outside `enforcement` module
- [x] Define `Tier` enum: `Observe`, `Act`, `Commit`
- [x] Compiles, tests pass (even if tests are trivial)

**This milestone is about the type system.** The capability token's private constructor is the first load-bearing security guarantee. If this is wrong, everything built on top is theater.

---

## Milestone 1: Enforcement Layer

**Goal:** The enforcement layer can evaluate a tool proposal against a TOML policy and return allow/reject/escalate. No model involved yet — pure unit tests.

- [x] Policy file parser (TOML → internal policy struct)
- [x] Default policy: deny everything not explicitly permitted
- [x] Pattern matching engine for bash commands against policy rules
- [x] `evaluate(proposal, policy) → Decision` function (the core of the project)
- [x] Decision types: `Allow(CapabilityToken)`, `Reject`, `Escalate`
- [x] `Allow` returns a `CapabilityToken` — the only way to obtain one
- [x] Tests:
  - Permitted observe command → Allow
  - Permitted act command → Allow
  - Commit-tier command → Escalate
  - Unknown command → Reject
  - Command matching no pattern → Reject (deny by default)
  - Attempt to construct CapabilityToken outside enforcement → compile error
  - Policy with no rules → deny everything
  - Empty command → Reject

**This milestone is about correctness.** The enforcement layer is the security boundary. Every edge case matters.

---

## Milestone 2: Agent Loop + CLI

**Goal:** You can type a message in the terminal, the Anthropic API generates a response, and if the model proposes a bash command, the enforcement layer evaluates it before execution.

- [x] Anthropic API provider: non-streaming `complete()` via reqwest (streaming deferred)
- [x] System prompt defining the agent's tool-calling interface
- [x] Agent loop: message → model → parse tool proposals → enforce → execute or reject → feed result back to model
- [x] CLI interface: rustyline REPL with history
- [x] Bash tool: `tokio::process::Command` with `kill_on_drop(true)`, 120s timeout, 256 KiB output truncation
  - Bash tool function signature requires `CapabilityToken` parameter — cannot be called without one
- [x] Wire types: serde structs for Anthropic API JSON, consecutive ToolResult merging
- [x] ToolRegistry with enum dispatch (`ToolImpl::Bash`), `ToolDefinition` schemas
- [x] Escalate treated as reject for M2 (approval gates are M3)
- [x] 37 unit tests + compile-fail test passing

**This milestone is the thesis proven.** A human can sit at the terminal, interact with the agent, and observe that:
1. Read-only commands execute freely
2. Write commands execute if the policy allows Act tier
3. Destructive commands are blocked or escalated
4. The model cannot talk its way past the enforcement layer

---

## Milestone 3: Approval Gates + Stateless Constraints

**Goal:** Commit-tier actions pause and ask the human for approval. Parameterized constraints enable fine-grained policy rules beyond pattern matching.

### Approval Gates
- [x] When enforcement returns `Escalate`, the CLI displays: what the model wants to do, why, and asks for y/n
- [x] Approval → enforcement issues `CapabilityToken` → tool executes
- [x] Denial → generic rejection sent to model
- [x] Timeout → denial (configurable, default 60s)
- [x] Model receives the same generic "action not permitted" for both denial and timeout — no information leakage

### Stateless Constraints (per-tool and per-action)
- [x] Constraint predicates in policy TOML: field comparisons (`lt`, `gt`, `eq`), containment (`contains_all`, `one_of`), string matching
- [x] Per-tool constraints: apply to every action (sandbox boundary, always hard reject on failure)
- [x] Per-action constraints: apply to a specific operation, with `on_constraint_failure` = `"reject"` or `"escalate"`
- [x] Constraint evaluation in `policy.rs` alongside existing regex matching — predicates over `params` JSON
- [x] Constraint failure can override tier upward (Act → Escalate) but never downward (Commit always escalates)
- [x] Tests: numeric bounds, containment checks, failure escalation, failure rejection, tool-level vs action-level precedence

**Approval gates and constraints are coupled:** `on_constraint_failure = "escalate"` is only useful when there's a mechanism to ask the human. This is why they share a milestone.

---

## Milestone 4: Hardening

**Goal:** The enforcement layer handles adversarial inputs correctly.

- [x] Command injection tests: can the model craft a command that looks like `ls` but executes `rm`? (pipes, semicolons, backticks, $(), &&, ||)
- [x] Policy bypass tests: edge cases in pattern matching (unicode, whitespace, null bytes)
- [x] Context window isolation: model output parsing handles malformed tool calls gracefully
- [x] The model proposes multiple tools in one response — each evaluated independently
- [x] Error handling: what happens when the Anthropic API is down? When bash hangs? When the policy file is malformed?
- [x] Structured logging with `tracing`: every proposal, every decision, every execution

---

## Milestone 5: Telegram Connector

**Goal:** The agent is reachable via Telegram. First real connector, first IPC boundary.

- [x] Telegram bot using `teloxide` crate
- [x] Connector runs as a separate process (or tokio task initially, extracted to process later)
- [x] Messages flow: Telegram → connector → runtime → enforcement → tool → runtime → connector → Telegram
- [x] Approval gates work via Telegram: bot sends "Agent wants to run `rm -rf /tmp/old`. Allow? (reply Y/N)"
- [x] Media handling: images, files (basic — forward to model as base64 or file reference)

---

## Milestone 6: Enforced Memory + Session Persistence

**Goal:** The agent has persistent, structured memory protected by the enforcement layer. Sessions survive restarts. Memory writes are policy-gated tool invocations.

### Database
- [x] PostgreSQL integration (deadpool-postgres connection pool, `postgres` feature)
- [x] Schema: memories table (content, structured JSONB, provenance, confidence, tier, embeddings, tsvector)
- [x] Schema: memory_chunks table (chunked documents for search)
- [x] Schema: sessions table, session_messages table
- [x] Migration framework (refinery, embedded migrations)

### Memory as an Enforced Tool (M6b complete)
- [x] `memory` tool: store, recall, search, update, forget operations
- [x] Memory writes pass through enforcement layer (same CapabilityToken requirement)
- [x] Policy controls: identity writes = Commit tier, preference writes = Act tier, reads = Observe
- [x] Memory tier system: explicit (1.0), confirmed (0.9), inferred (0.5-0.7)
- [x] Provenance tracking: source session, source turn, source type on every memory
- [x] `match_source = "structured"` enforcement strategy — generalizes enforcement beyond bash
- [x] Three memory scopes: agent (Commit to modify), user (Act to modify), working (Observe to modify)
- [x] Soft-delete via `superseded_by` self-pointer — audit trail preserved
- [x] User isolation: each user's memories are filtered by user_id

### Search (M6c complete)
- [x] Hybrid search: pgvector cosine similarity + tsvector FTS
- [x] Reciprocal Rank Fusion for combining results
- [x] Embedding provider abstraction (OpenAI text-embedding-3-small initially)
- [x] Confidence-weighted result ranking

### Proactive Memory Injection (M6d complete)
- [x] Before each turn, runtime embeds user message and queries relevant memories
- [x] Top memories injected into system prompt with confidence labels
- [x] Agent cannot suppress injection — runtime controls context

### Session Persistence
- [x] Sessions stored in PostgreSQL (messages, tool calls, results) — `sessions` feature
- [x] Session restore on restart — CLI resumes last session, Telegram resumes per-chat
- [x] Context compaction: token estimation, LLM summarization of old turns
- [x] Pre-compaction memory flush: extract important information before discarding context

### Contradiction Detection
- [ ] On memory write, query semantically similar existing memories
- [ ] Surface conflicts to user via existing escalation mechanism
- [ ] `superseded_by` chain for memory history (no silent overwrites)

---

## Milestone 7: Credential Broker ✓

**Goal:** The agent can reference credentials by name. Actual values are injected at execution time, outside the agent's context.

### M7a: Credential Vault (complete)
- [x] Encrypted credential storage in PostgreSQL (AES-256-GCM + HKDF-SHA256 per-secret key derivation)
- [x] `CredentialStore` trait with `PgCredentialStore` implementation
- [x] Master key from `CHERUB_MASTER_KEY` env var (32+ bytes, hex-encoded)
- [x] CLI: `cherub credential store/list/delete` subcommands
- [x] Unit tests: encrypt/decrypt roundtrip, salt uniqueness, tamper detection, key validation

### M7b: HTTP Tool + Credential Injection (complete)
- [x] `HttpTool` with configured timeouts (connect 10s, read 30s, total 120s)
- [x] `CredentialBroker`: resolves name → validates host+capability → decrypts → injects
- [x] `HttpStructured` match source: extracts `"{method}:{host}"` from params
- [x] Policy gating: `http_structured` match source in policy; by default HTTP is disabled
- [x] Defense in depth: policy gates the action; broker re-validates host + capability scope
- [x] Agent sees credential name only — value is injected at the execution boundary

### M7c: Leak Prevention (complete)
- [x] `LeakDetector`: per-request scanner, registered with decrypted values
- [x] Response body scanned before being returned to session history
- [x] Error messages scanned before being logged
- [x] `DecryptedCredential`: no Clone, no Display; `expose()` is `pub(crate)` only
- [x] `expose_secret()` appears exactly once in the broker (the 4th call site in the codebase)

### M7d: Polish + Documentation (complete)
- [x] `config/default_policy.toml`: HTTP tool section with examples and comments
- [x] `ROADMAP.md`: M7 documented
- [x] `CLAUDE.md`: credential isolation rules updated for 4th expose_secret() site
- [x] `tests/adversarial.rs`: HTTP enforcement adversarial tests

---

## Milestone 8: WASM Sandbox + Untrusted Tool Execution

**Goal:** Untrusted tools run in WASM sandboxes with host-mediated I/O. The enforcement layer gates entry; the sandbox constrains execution.

- [x] Wasmtime integration: compile, instantiate, and execute WASM modules
- [x] Host functions: `workspace_read`, `http_request`, `secret_exists`, `log`, `now_millis`
- [x] Capability declaration: per-tool manifest (allowlisted endpoints, path prefixes, credential names)
- [x] Credential injection at host boundary (tool never sees secret values)
- [x] Resource limits: fuel metering (CPU), memory cap, execution timeout
- [x] Leak detection: scan HTTP responses and tool output for secret exfiltration
- [x] Fresh WASM instance per execution (no shared state between invocations)
- [x] Tool loader: discover WASM tools from a configured directory, validate with BLAKE3 hash
- [x] Write a WASM tool (HTTP/API tool) to validate the sandbox works end-to-end
- [x] Enforcement layer gates WASM tools identically to in-process tools (same CapabilityToken requirement)

---

## Milestone 9: Container Sandbox + IPC Protocol

**Goal:** Heavy/polyglot tools run in Docker/Podman containers. Language-agnostic plugin ecosystem.

- [x] Define IPC protocol: length-prefixed JSON over Unix domain sockets
- [x] Plugin registration handshake: type, identity, capability declarations
- [x] Container lifecycle management: spawn, connect, health check, restart, kill on timeout
- [x] Network isolation by default (no network access; IPC socket is the only channel)
- [x] Workspace mounting: read-only directory mounts for tools that need file access
- [x] Resource limits via cgroups (CPU, memory)
- [x] A crashing container tool does not affect the runtime
- [x] Write a container tool plugin (e.g., Python-based) to validate language-agnostic IPC works

---

## Milestone 10: Security Hardening + Audit Log ✓

**Goal:** Close the remaining security gaps vs. IronClaw. Add operational observability via an append-only audit event log.

### HTTP tool hardening (complete)
- [x] DNS rebinding defense: resolve hostname before sending; reject if any resolved IP is in a private/loopback/link-local range (127/8, 10/8, 172.16/12, 192.168/16, 169.254/16, ::1, fc00::/7, fe80::/10)
- [x] Disable HTTP redirects (`redirect::Policy::none()`): prevents credential exfiltration via injected redirect after credential injection
- [x] Document that LeakDetector scans all response bodies regardless of HTTP status (2xx and error)
- [x] Document that in-process bash is trusted/dev context only; production should use container-sandboxed bash

### Audit log (complete)
- [x] V4 migration: `audit_events` table (append-only; rows are never updated or deleted)
- [x] `AuditDecision` enum: allow, reject, escalate, approve, deny
- [x] `AuditStore` trait with `append()` and `list()` operations
- [x] `PgAuditStore` implementation with parameterized dynamic WHERE builder
- [x] `AgentLoop::with_audit_log()`: optional audit store, non-fatal on append failure
- [x] Audit events emitted for every enforcement decision (with tier, duration_ms, is_error)
- [x] `cherub audit list` CLI subcommand with `--tool`, `--decision`, `--user`, `--session`, `--limit` filters
- [x] Audit store auto-attached when DATABASE_URL is set

---

## Milestone 11: MCP Server Support ✓

**Goal:** Support MCP (Model Context Protocol) servers as a tool source alongside WASM/container plugins. Any MCP-compliant server becomes available to the agent, with all calls routed through the enforcement layer.

### MCP integration (complete)
- [x] `MatchSource::McpStructured` — extracts `"{server}:{tool}"` from `__mcp_server`/`__mcp_tool` params for enforcement pattern matching
- [x] `McpClient` — wraps `rmcp::RunningService`, handles spawn/init/discovery/call/shutdown over stdio transport
- [x] `McpToolProxy` — one per discovered tool, fields: server_name, tool_name, composite_name (`"{server}__{tool}"`), description, input_schema, client
- [x] `ToolImpl::Mcp(McpToolProxy)` variant with full enum dispatch (name/execute/definition)
- [x] `ToolRegistry::enforcement_name()` — maps composite name to server name for policy lookup
- [x] `ToolRegistry::enrich_params()` — injects `__mcp_server`/`__mcp_tool` metadata (always overwrites to prevent adversarial injection)
- [x] `loader::load_from_config()` — reads TOML config, spawns servers, discovers tools, returns `McpLoadResult`
- [x] `credential_env` support — decrypt credentials from vault at spawn time (feature-gated on `credentials`)
- [x] `--mcp-config <path>` CLI flag wired into `run_agent()`
- [x] `CherubError::Mcp(String)` error variant (feature-gated on `mcp`)
- [x] Internal `__mcp_*` keys stripped before forwarding to MCP server
- [x] Integration tests: mock MCP server binary (echo + add tools), 12 tests covering discovery, execution, enforcement, adversarial override prevention, error handling

### Policy example
```toml
[tools.google-workspace]
enabled = true
match_source = "mcp_structured"

[tools.google-workspace.actions.read]
tier = "observe"
patterns = ["^google-workspace:list_events$", "^google-workspace:search_emails$"]

[tools.google-workspace.actions.send]
tier = "commit"
patterns = ["^google-workspace:send_email$"]
```

---

## Milestone 12: Cost Tracking + Budget Enforcement ✓

**Goal:** No autonomous work without a budget ceiling. Track every token, enforce spend limits through the existing constraint system. Safety net before the agent operates independently.

### M12a: Token Usage Tracking (complete)
- [x] V5 migration: `token_usage` table (user_id, session_id, model_name, input_tokens, output_tokens, cost_usd, call_type, timestamp)
- [x] `CostStore` trait: `record()`, `session_cost()`, `period_cost(user_id, since)`, `daily_costs(user_id, days)`
- [x] `PgCostStore` implementation (append-only, same pattern as `PgAuditStore`)
- [x] Wire into agent loop: record `ApiUsage` after every `provider.complete()` call (inference, summarization, extraction)

### M12b: Model Pricing Configuration (complete)
- [x] `ModelPricing { input_per_mtok: f64, output_per_mtok: f64, cache_write_per_mtok, cache_read_per_mtok }` keyed by model name prefix
- [x] DB-backed pricing: `model_pricing` table (V6 migration), `PricingStore` trait + `PgPricingStore`
- [x] `PricingTable` (in-memory `HashMap`) loaded from DB at startup, empty = $0.00
- [x] `lookup_pricing(table, model_name)` with longest-prefix matching
- [x] `compute_cost(usage: &ApiUsage, pricing: &ModelPricing) -> f64` pure function
- [x] `cherub pricing list/set/delete` CLI subcommands
- [x] No hardcoded pricing — users populate via CLI; no stale rates shipped

### M12c: Budget Constraints in Enforcement (complete)
- [x] `BudgetContext` struct: session cost, daily cost
- [x] Extend `evaluate()` signature: `evaluate(proposal, policy, budget: Option<&BudgetContext>)` — backward compatible
- [x] Budget exceeded → `Decision::Escalate` or `Decision::Reject` (configurable via `on_exceeded`)
- [x] Policy TOML `[budget]` section: `session_limit_usd`, `daily_limit_usd`, `on_exceeded = "escalate" | "reject"`
- [x] Budget context loaded from `CostStore` before tool evaluation each turn

### M12d: CLI Cost Visibility (complete)
- [x] `cherub cost summary` — current session, today, this month
- [x] `cherub cost history --days 7` — daily breakdown
- [x] Reuse existing CLI subcommand pattern (`cherub credential`, `cherub audit`)

---

## Milestone 13: Multi-Provider Architecture

**Goal:** Multiple LLM providers for failover, cost optimization, and task-appropriate model routing. Frontier model as orchestrator, cheaper/local models as delegated tool calls.

Research findings:
- **Anchoring bias is real and unfixable** in draft+review patterns. GPT-4 anchoring index ~0.45 (Ling et al. 2024). Standard mitigations (CoT, reflection, "ignore the anchor") do not reduce anchoring (tested across all GPT models).
- **Cascade/draft-review is the wrong pattern.** Presenting a frontier model with a weaker draft anchors its output toward the draft's approach. Stronger models are *more* consistently biased than weaker ones.
- **Frontier orchestrator + local model as tool is the right pattern.** The frontier model reasons first, delegates bounded subtasks (summarization, extraction, formatting) to cheaper models. This structurally avoids anchoring because frontier reasoning is upstream of the cheap model's output.
- **Evidence:** Anthropic multi-agent (Opus lead + Sonnet workers) outperformed single Opus by 90.2%. NVIDIA Orchestrator-8B achieved 70% cost savings with better quality than GPT-5 solo. Cross-vendor DMF: 79% cost savings, +5% code correctness.
- **Local models** are good for bounded tasks but not autonomous multi-step reasoning.

### M13-prep: Provider Trait Migration (complete)
- [x] `async_trait` becomes non-optional dependency (was gated behind `postgres`/`container`)
- [x] `Provider` trait gets `#[async_trait]` for object safety
- [x] `AgentLoop` drops generic `P: Provider`, stores `Box<dyn Provider>` instead
- [x] `ApiUsage` extended with `cache_creation_tokens` and `cache_read_tokens` (Anthropic cache pricing)
- [x] `ModelPricing` extended with `cache_write_per_mtok` and `cache_read_per_mtok`
- [x] `WireUsage` parses `cache_creation_input_tokens` / `cache_read_input_tokens` from Anthropic responses
- [x] `Provider::pricing()` removed — pricing decoupled from providers, now DB-backed (M12b)

### M13a: OpenAI-Compatible Provider (complete)
- [x] `OpenAiProvider` implementing `Provider` trait — covers OpenAI, Azure OpenAI, Gemini (via compatible endpoint), Ollama, vLLM, LM Studio, Groq
- [x] Constructor takes `base_url` parameter (defaults to OpenAI, configurable for local/alternative)
- [x] Own wire types in `openai_wire.rs` (private, like `wire.rs` for Anthropic)
- [x] Same retry logic pattern as `AnthropicProvider` (provider-local `RetryConfig`)

### M13b: Provider Configuration (complete)
- [x] TOML config for named provider definitions (`[providers.<name>]` sections)
- [x] `ProvidersConfig` + `ProviderDef` + `SubAgentDef` structs with `#[serde(deny_unknown_fields)]`
- [x] `ProviderType` enum: `anthropic`, `openai`, `failover` (M13c placeholder)
- [x] `instantiate_provider()` reusable library function in `providers/config.rs`
- [x] `--providers` CLI flag; backward compatible with existing `--provider`/`--base-url`/`--model`
- [x] `CHERUB_PROVIDERS_CONFIG` env var for Telegram bot
- [x] Cross-field validation: failover requires provider list, agents reference existing providers
- [x] 14 unit tests: parsing, unknown field rejection, validation, instantiation

### M13c: Failover Provider
- [ ] `FailoverProvider` wraps `Vec<Box<dyn Provider>>` — legitimate `dyn Provider` boundary
- [ ] `complete()` tries providers in order; on transient `CherubError::Provider`, tries next
- [ ] Circuit breaker per provider: N consecutive failures → skip for cooldown period, auto-recover
- [ ] Structured tracing: which provider tried, succeeded, failed and why
- [ ] Cost tracking integration: record correct model name for each provider's `ApiUsage`
- [ ] Configuration in providers TOML: `type = "failover"` with `providers = ["name1", "name2"]`

### M13d: Sub-Agent Tools
- [ ] `SubAgentTool` struct: owns `Box<dyn Provider>`, system prompt, description, resource limits
- [ ] `ToolImpl::SubAgent` variant with full enum dispatch (name/execute/definition)
- [ ] Each sub-agent is registered as a tool — orchestrator sees description, decides when to delegate
- [ ] Bounded inner execution loop (not full AgentLoop): max_turns, timeout, no approval gate
- [ ] Sub-agent escalations auto-rejected (`AutoRejectGate`) — only orchestrator can escalate to human
- [ ] Policy gates sub-agent invocation via `MatchSource::Structured` (`action = "invoke"`)
- [ ] Sub-agents can optionally have their own tool set (subset declared in config, e.g., `tools = ["bash", "file"]`)
- [ ] Cost tracking correctly attributes sub-agent token usage to sub-agent's model name
- [ ] TOML config: `[agents.<name>]` sections in the providers config file
- [ ] No new feature flag (always available when `--providers` config defines agents)

---

## Milestone 14: Output Patterns + Extended Thinking

**Goal:** The agent communicates clearly during autonomous work: recapitulates understanding, shows heartbeat during execution, presents clean results. Extended thinking enables complex code reasoning.

### M14a: Extended Thinking Support
- [ ] Add `Thinking { thinking: String }` to wire `ResponseContentBlock` (currently only `Text` and `ToolUse`)
- [ ] Add `ContentBlock::Thinking` to internal types
- [ ] Enable extended thinking in Anthropic API request when configured (`anthropic-beta` header)
- [ ] Thinking blocks logged via tracing but NOT emitted to OutputSink by default — available in debug/verbose mode
- [ ] Feature-gated or config-gated (not all providers support thinking blocks)

### M14b: Recapitulation Pattern
- [ ] System prompt instruction: model begins each response by briefly restating its understanding of the task (1-2 sentences), then proceeds with execution
- [ ] `OutputEvent::Recapitulation(&'a str)` for sinks that want to style it differently
- [ ] Prompt-level pattern, not structural — model naturally produces recapitulation before tool calls

### M14c: Heartbeat / Progress Indicator
- [ ] `OutputEvent::Progress { tool: &'a str, status: &'a str }` — emitted when a tool starts executing
- [ ] CLI sink: spinner line (`[working] running tests...`) overwritten by next event
- [ ] Telegram sink: edit last message to show current status (avoids message spam)
- [ ] Periodic `Progress` events during long tool executions

### M14d: Turn-Level Output Batching (Telegram)
- [ ] `TelegramSink` collects events during a turn instead of sending each immediately
- [ ] At turn end: single message with recapitulation at top, tool summary (collapsed), final result
- [ ] Uses `edit_message` to update a single status message during the turn
- [ ] Falls back to current behavior in debug/verbose mode
- [ ] Subsumes "Telegram output verbosity modes" from ROADMAP_DEFERRED.md

---

## Milestone Dependencies

```
M12a (token tracking) ──┐
M12b (pricing config) ──┼── M12c (budget constraints) ── M12d (CLI)
                         │
                         └── M13a (OpenAI provider) ── M13b (provider config) ──┐
                                                                                 ├── M13c (failover)
                                                                                 └── M13d (sub-agents)

M14a-c are independent — can parallel with M12/M13
M14d depends on M14c
```

---

## Unfinished from Earlier Milestones

### M6: Contradiction Detection
- [ ] On memory write, query semantically similar existing memories
- [ ] Surface conflicts to user via existing escalation mechanism
- [ ] `superseded_by` chain for memory history (no silent overwrites)

---

## Beyond M14

These are real goals but not yet planned into milestones:

- Schedule triggers (cron/interval): `tokio-cron-scheduler` injects "scheduled wake" messages into agent loop. Feature-gated (`schedule`). CLI flag `--schedule`.
- Policy hot-reload (file watch + re-evaluate). Design exists in DESIGN.md Section 9.4.
- Multi-agent routing (different policies per channel)
- Per-task dynamic constraints (session-scoped, user-confirmed via approval gate — see DESIGN.md Section 3.5)
- OpenTelemetry export: `tracing-opentelemetry` as optional subscriber. Feature-gated (`otel`).
- MCP dynamic tool changes: handle `tools/list_changed` notifications at runtime
- Policy generation tooling (analyze a tool's actions, suggest tier classifications)
- Discord connector
- Slack connector

---

## Definition of Done (MVP)

The MVP is **Milestone 4 complete**. At that point:

- A Rust binary enforces deterministic capability tiers on AI agent tool calls
- The enforcement layer is the only path to tool execution
- Capability tokens are compiler-enforced and unforgeable
- The policy is agent-opaque
- Deny-by-default is proven
- Adversarial inputs are handled
- A human can use it as a daily CLI agent and trust that destructive commands require approval

Everything after Milestone 4 is expansion. The thesis is proven at Milestone 4.
