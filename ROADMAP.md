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

## Beyond MVP

These are real goals but not blocking the thesis proof:

- Discord connector
- Slack connector
- Policy hot-reload (file watch + re-evaluate)
- Multi-provider failover (try primary → fallback on failure; Anthropic + OpenAI + Ollama)
- Session persistence (JSONL)
- Multi-agent routing (different policies per channel)
- Per-task dynamic constraints (session-scoped, user-confirmed via approval gate — see DESIGN.md Section 3.5)
- Stateful constraints (cumulative tracking: daily spend limits, action rate limits, time-windowed budgets — first application: LLM cost budget enforcement)
- Policy generation tooling (analyze a tool's actions, suggest tier classifications)
- LLM cost tracking + budget enforcement: track token usage (model, input/output tokens, per-model cost rates) in PostgreSQL (V5 migration). Wire cost data into stateful constraints — per-session and per-day spend budgets. Budget exceeded → escalate or reject depending on policy. Subsumes "Token Usage Tracking" from ROADMAP_DEFERRED.md.
- Multi-provider failover implementation: `FailoverProvider` wraps `Vec<Box<dyn Provider>>`, tries each in order. Start with Anthropic + OpenAI. Log which provider succeeded. Retry/fallback logic with structured tracing.
- OpenTelemetry export: `tracing-opentelemetry` as optional subscriber. Feature-gated (`otel`). `OTEL_EXPORTER_OTLP_ENDPOINT` env var enables. Cherub already emits structured spans — OTEL export makes them visible in Grafana/Datadog/etc.
- MCP tool protocol support: support MCP servers as a tool source alongside WASM/container plugins. Spawn server process, discover tools via `tools/list`, route calls through enforcement layer. Feature-gated (`mcp`). `tools/mcp/` module, MCP client (stdio transport), `MatchSource::McpStructured`, capability sidecar TOML.
- Schedule triggers (cron/interval): `tokio-cron-scheduler` injects "scheduled wake" messages into agent loop at configured intervals. Enables periodic autonomous work within policy bounds. Feature-gated (`schedule`). CLI flag `--schedule`.

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
