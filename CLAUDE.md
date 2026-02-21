# Cherub

Secure agent runtime. Deterministic capability enforcement for AI agents.

## What This Is

A Rust binary that owns the entire execution path from user message to tool execution. The model proposes actions as data. The runtime evaluates proposals against a policy. The model never touches tools directly. See DESIGN.md for the full architecture.

## Project Structure

```
cherub/
├── Cargo.toml
├── docker-compose.yml        # PostgreSQL + pgvector on port 5480 (dev)
├── src/
│   ├── main.rs              # Entry point, CLI interface
│   ├── lib.rs               # Library entry point
│   ├── error.rs             # Error types
│   ├── bin/
│   │   └── telegram.rs       # Telegram bot entry point (feature-gated)
│   ├── runtime/
│   │   ├── mod.rs            # AgentLoop<P, A, O> + run_turn() (generic over Provider/ApprovalGate/OutputSink)
│   │   ├── approval.rs       # ApprovalGate trait, CliApprovalGate, EscalationContext
│   │   ├── output.rs         # OutputSink trait, StdoutSink, NullSink
│   │   ├── session.rs        # Conversation state, message history, optional persistence
│   │   ├── prompt.rs         # System prompt builder
│   │   └── tokens.rs         # Token estimation for context compaction
│   ├── enforcement/
│   │   ├── mod.rs            # Enforcement layer entry point
│   │   ├── capability.rs     # Capability tokens (private constructors)
│   │   ├── extraction.rs     # MatchSource enum (Command/Structured) — action extractor strategies
│   │   ├── policy.rs         # Policy loading and evaluation (Clone for multi-session sharing)
│   │   ├── shell.rs          # Shell command parser (quote-aware splitting)
│   │   └── tier.rs           # Observe/Act/Commit tier definitions
│   ├── tools/
│   │   ├── mod.rs            # Tool trait, ToolRegistry, ToolImpl enum dispatch, ToolContext
│   │   ├── bash.rs           # Bash execution tool (tokio::process::Command)
│   │   ├── memory.rs         # Memory tool: store/recall/search/update/forget (feature = "memory")
│   │   ├── http.rs           # HTTP tool: GET/POST/PUT/PATCH/DELETE with broker injection (feature = "credentials")
│   │   ├── credential_broker.rs  # CredentialBroker: name → inject into reqwest::RequestBuilder (feature = "credentials")
│   │   ├── leak_detector.rs  # Per-request secret scanner: redacts values from response bodies (feature = "credentials")
│   │   ├── wasm/             # Feature-gated: #[cfg(feature = "wasm")]
│   │   │   ├── mod.rs        # Module declarations, 7-layer defense-in-depth doc
│   │   │   ├── capabilities.rs  # Capabilities struct: workspace/http/secrets, parsed from TOML sidecar
│   │   │   ├── limits.rs     # ResourceLimits (fuel/memory/timeout), WasmResourceLimiter impl
│   │   │   ├── runtime.rs    # WasmToolRuntime: Engine config, epoch ticker, PreparedModule
│   │   │   ├── host.rs       # HostState: log/now_millis/workspace_read/check_http_request host fns
│   │   │   ├── wrapper.rs    # bindgen! bindings, StoreData, WasmTool, execute_in_sandbox
│   │   │   └── loader.rs     # load_from_dir/load_one: directory scan, BLAKE3 hash, compilation
│   │   └── container/        # Feature-gated: #[cfg(feature = "container")]
│   │       ├── mod.rs        # Module declarations, 7-layer defense-in-depth doc
│   │       ├── capabilities.rs  # ContainerCapabilities: workspace/http/secrets, parsed from TOML sidecar
│   │       ├── ipc.rs        # Wire format (length-prefixed JSON), IpcTransport, RuntimeMessage/ToolMessage
│   │       ├── runtime.rs    # ContainerRuntime trait + BollardRuntime (Docker/Podman via bollard)
│   │       ├── host.rs       # ContainerHostState: async host function proxy (workspace/http/secrets/log)
│   │       ├── wrapper.rs    # ContainerTool: lifecycle management, IPC execute loop, respawn-on-crash
│   │       └── loader.rs     # load_from_dir/load_one: scan tool.toml + capabilities.toml per subdirectory
│   ├── providers/
│   │   ├── mod.rs            # Provider trait, Message/UserContent/ContentBlock types (serde + Clone)
│   │   ├── anthropic.rs      # Anthropic API provider (non-streaming)
│   │   └── wire.rs           # Serde structs for Anthropic API JSON (private, supports images)
│   ├── storage/              # Feature-gated: #[cfg(feature = "postgres")]
│   │   ├── mod.rs            # SessionStore + MemoryStore + CredentialStore + AuditStore traits, connect(), migration runner
│   │   ├── embedding.rs      # EmbeddingProvider trait + OpenAiEmbeddingProvider (M6c)
│   │   ├── search.rs         # Reciprocal Rank Fusion algorithm (M6c, pure/no-DB)
│   │   ├── pg_session_store.rs  # PgSessionStore: PostgreSQL SessionStore impl
│   │   ├── pg_memory_store.rs   # PgMemoryStore: PostgreSQL MemoryStore impl (feature = "memory")
│   │   ├── crypto.rs         # AES-256-GCM + HKDF-SHA256 per-secret encryption (feature = "credentials")
│   │   ├── credential_types.rs  # Credential/CredentialRef/DecryptedCredential/CredentialLocation (feature = "credentials")
│   │   ├── pg_credential_store.rs  # PgCredentialStore: PostgreSQL CredentialStore impl (feature = "credentials")
│   │   ├── pg_audit_store.rs    # PgAuditStore: PostgreSQL AuditStore impl, append-only event log (M10)
│   │   └── migrations/
│   │       ├── V1__initial_schema.sql  # Sessions + messages + memory schema (UUIDv7, scope column)
│   │       ├── V2__vector_indexes.sql  # HNSW indexes for embedding columns (M6c)
│   │       ├── V3__credentials.sql     # Encrypted credential vault table (M7a)
│   │       └── V4__audit_log.sql       # Audit event log table (M10)
│   └── telegram/             # Feature-gated: #[cfg(feature = "telegram")]
│       ├── mod.rs             # Module declarations
│       ├── approval.rs        # TelegramApprovalGate (inline keyboard + oneshot channels)
│       ├── connector.rs       # Message/callback routing, photo download + base64
│       ├── output.rs          # TelegramSink (OutputSink for Telegram chats)
│       └── session.rs         # Per-chat session manager (channel-based, no Arc<Mutex>)
├── tests/
│   ├── adversarial.rs        # Mock-provider adversarial integration tests (27 tests)
│   ├── compile_tests.rs      # Compile-time invariant tests (trybuild)
│   ├── embedding_live.rs     # Live OpenAI embedding tests (#[ignore], requires OPENAI_API_KEY)
│   ├── fixtures/
│   │   └── mod.rs            # Shared test fixtures: TestContainer + MockEmbeddingProvider (M6c)
│   ├── memory_enforcement.rs # Memory tool enforcement tests, no DB needed (feature = "memory")
│   ├── container_lifecycle.rs  # Container IPC interop tests (M9, Python subprocess mock + #[ignore] Docker)
│   ├── memory_injection.rs   # Proactive injection integration tests (M6d, no DB needed)
│   ├── memory_store.rs       # PgMemoryStore integration tests (M6b + M6c hybrid search)
│   ├── redteam.rs            # Live model adversarial tests (#[ignore], requires API key)
│   ├── compaction.rs         # Context compaction integration tests (mock provider, no API key)
│   ├── session_persistence.rs  # Session persistence integration tests (feature = "sessions", auto-starts DB)
│   ├── telegram_approval.rs  # Telegram approval flow tests (feature-gated)
│   └── ui/
│       ├── capability_token_private.rs      # Proves CapabilityToken can't be constructed outside enforcement
│       └── capability_token_private.stderr  # Expected compiler error output
├── .config/
│   └── nextest.toml          # cargo-nextest config: 4 slots, retries, slow-test detection
├── config/
│   └── default_policy.toml   # Example policy file
├── DESIGN.md
├── ROADMAP.md
├── ROADMAP_DEFERRED.md
└── LICENSE
```

## Key Invariants

These must never be violated. Every PR, every refactor, every feature addition must preserve these:

1. **The enforcement layer is the only path to tool execution.** There is no code path from model output to tool execution that does not pass through `enforcement::evaluate()`. If you find yourself writing a shortcut, stop.

2. **Capability tokens have private constructors.** `CapabilityToken` can only be created by the enforcement layer after policy evaluation. No public `new()`, no `Default`, no `From` impl that could forge one.

3. **Model output is data, not code.** The model's response is parsed as a string into typed Rust structs. The model never influences control flow directly — it proposes, the runtime decides.

4. **The agent never sees the policy.** Policy rejection returns a generic "action not permitted" message. No rule names, no explanations, no hints about what would be allowed instead.

5. **Deny by default.** If the policy doesn't explicitly permit an action, it is denied. Unknown tools, unknown actions, ambiguous matches — all denied.

## Rust Conventions

- **Edition:** 2024
- **Async runtime:** tokio (multi-threaded)
- **Error handling:** `thiserror` for library errors, `anyhow` for application-level. Use `?` propagation, not `.unwrap()` in non-test code.
- **Serialization:** `serde` + `serde_json` for API communication, `toml` for config/policy files.
- **HTTP client:** `reqwest` for API calls.
- **Logging:** `tracing` crate. Structured logging from day one.
- **No `unsafe` blocks** unless absolutely necessary and documented with a safety comment.
- **Tests:** Unit tests in-module (`#[cfg(test)] mod tests`). Integration tests in `tests/` directory. The enforcement layer must have thorough tests — it is the security boundary.

## Idiomatic Rust — Avoid OOP Patterns

LLMs (including the one writing this code) drift toward Java/C#-style OOP when writing Rust. This section exists to catch that drift. **Read this before writing any new struct or trait.**

### Use enums for variants, not trait objects

```rust
// WRONG: OOP-style trait hierarchy
trait Message { fn content(&self) -> &str; }
struct UserMessage { content: String }
struct AssistantMessage { content: String }
impl Message for UserMessage { ... }
impl Message for AssistantMessage { ... }
fn process(msg: &dyn Message) { ... }

// RIGHT: Algebraic data types
enum Message {
    User { content: String },
    Assistant { content: String, tool_calls: Vec<ToolCall> },
}
fn process(msg: &Message) {
    match msg {
        Message::User { content } => { ... }
        Message::Assistant { content, tool_calls } => { ... }
    }
}
```

Use `enum` when variants are known at compile time. Use `dyn Trait` only at true extension boundaries (plugins loaded at runtime). In this project, the legitimate `dyn Trait` boundaries are: `Provider` (multiple LLM backends), `Tool` (plugin tools over IPC), `SessionStore`, `MemoryStore`, and `EmbeddingProvider` (all storage/embedding backends selected at runtime). Everything else should be an enum.

### Use the typestate pattern for capability tokens

```rust
// The CapabilityToken isn't just a struct — it's a state machine enforced by the type system.
// A proposal must go through evaluation before it becomes executable.

struct Proposed;    // Tool call parsed from model output
struct Evaluated;   // Enforcement layer has decided

struct ToolInvocation<State> {
    tool: String,
    action: String,
    params: serde_json::Value,
    _state: PhantomData<State>,
}

// Only Evaluated invocations can be executed
impl ToolInvocation<Evaluated> {
    fn execute(self, token: CapabilityToken) -> Result<ToolResult> { ... }
}

// You literally cannot call execute() on a Proposed invocation. The compiler rejects it.
```

### Prefer functions and iterators over methods and mutation

```rust
// WRONG: Mutable accumulator pattern
let mut results = Vec::new();
for proposal in proposals {
    let decision = evaluate(&proposal, &policy);
    results.push(decision);
}

// RIGHT: Iterator chain
let results: Vec<Decision> = proposals
    .iter()
    .map(|p| evaluate(p, &policy))
    .collect();
```

### Use builders for complex construction, not constructors with many args

```rust
// WRONG: Telescoping constructor
let policy = Policy::new(path, true, false, None, Some(60));

// RIGHT: Builder pattern
let policy = Policy::builder()
    .path(path)
    .deny_by_default(true)
    .hot_reload(false)
    .timeout(Duration::from_secs(60))
    .build()?;
```

### Use `impl Trait` over `Box<dyn Trait>` when you can

```rust
// WRONG: Unnecessary heap allocation
fn get_provider() -> Box<dyn Provider> { ... }

// RIGHT: Static dispatch when the concrete type is known
fn get_provider() -> impl Provider { ... }

// OK: Dynamic dispatch when genuinely needed (e.g., runtime plugin selection)
fn load_plugin(name: &str) -> Box<dyn Tool> { ... }
```

### Ownership as architecture, not `Arc<Mutex<T>>` everywhere

```rust
// WRONG: Shared mutable state via interior mutability
struct Runtime {
    policy: Arc<Mutex<Policy>>,
    tools: Arc<Mutex<HashMap<String, Box<dyn Tool>>>>,
}

// RIGHT: Ownership flows through the system
struct Runtime {
    policy: Policy,           // Runtime owns the policy
    tools: ToolRegistry,      // Runtime owns the registry
}
// Pass &self for reads, &mut self for mutations.
// If concurrent access is needed, use channels (mpsc) not shared locks.
```

### Specific anti-patterns to watch for

- **No getter/setter methods.** Make fields `pub` or `pub(crate)` and access them directly. Methods should do work, not just return fields.
- **No `impl Default` as a constructor substitute.** Use `new()` with required args or a builder.
- **No deep trait hierarchies.** Rust traits are not Java interfaces. Prefer composition (struct fields) over trait inheritance.
- **No `clone()` to dodge the borrow checker.** If you're cloning to satisfy borrows, restructure ownership instead.
- **No `String` where `&str` suffices.** Take `&str` in function parameters, return `String` when you own it.
- **No `Option<Box<dyn Error>>` for errors.** Use `thiserror` enums with specific variants.

## Rust Coder Rules

Hard constraints enforced during development. Every PR, commit, and code review must pass these checks.

### Security Invariant Rules

These enforce Cherub-specific guarantees the compiler alone can't catch.

- **CapabilityToken audit rule** — Before any PR/commit, `grep` for `CapabilityToken` and verify: no `pub fn new`, no `Default`, no `From`, no `Clone`, no `Copy`. Only `enforcement/` creates tokens.
- **Single enforcement path** — Every tool's `execute()` function signature must require a `CapabilityToken` parameter. If a tool function compiles without one, it's a bug.
- **Policy opacity** — No enforcement error message may contain: rule names, pattern text, tier names, or any string from the policy file. Rejection is always `"action not permitted"`.
- **Credential isolation** — `secrecy::SecretString` for all credential values. `grep expose_secret` must only appear at these six call sites: (1) DB URL in `storage/mod.rs`, (2) API key in `providers/anthropic.rs`, (3) embedding key in `storage/embedding.rs`, (4) agent credential injection in `storage/credential_types.rs::DecryptedCredential::expose()` (called only from `tools/credential_broker.rs`), (5) master key hex-validation in `storage/crypto.rs::CredentialCrypto::new()`, (6) master key HKDF input in `storage/crypto.rs::CredentialCrypto::derive_key()`. If it appears anywhere else, it's a bug.
- **No `unsafe`** — Zero `unsafe` blocks unless documented with a `// SAFETY:` comment explaining why it's necessary and what invariant the developer is upholding.

### Idiomatic Rust Rules (LLM Anti-Pattern Watchlist)

Specific patterns to catch and correct — based on what LLMs commonly get wrong when writing Rust.

- **Enum over trait objects** — If variants are known at compile time, use `enum` + `match`. Legitimate `dyn Trait` boundaries in this project: `Provider` (LLM backends), `Tool` (plugin boundaries), `SessionStore`, `MemoryStore`, `EmbeddingProvider` (all storage backends selected at runtime).
- **`&str` in, `String` out** — Function parameters take `&str` or `impl AsRef<str>`. Return `String` only when transferring ownership. Never `fn foo(s: String)` when `fn foo(s: &str)` works.
- **Iterator chains over mutation** — Prefer `.iter().map().collect()` over `let mut v = Vec::new(); for x in ... { v.push(...) }`.
- **`?` propagation, never `.unwrap()`** — `.unwrap()` only in tests and code paths that are provably infallible (with a comment explaining why). Use `thiserror` enums in the enforcement layer, `anyhow` at the application/CLI boundary.
- **No `clone()` to dodge borrows** — If cloning to satisfy the borrow checker, restructure ownership. Ask: who should own this data?
- **No getter/setter methods** — Fields are `pub(crate)` and accessed directly. Methods do work, not field access.
- **No `Arc<Mutex<T>>`** — Use ownership or channels (`tokio::sync::mpsc`). The only exception: truly shared concurrent state that can't be restructured (document why).
- **Builders for 3+ parameters** — Hand-written builders (not derive_builder). Consuming self pattern: `fn path(mut self, p: &Path) -> Self`.
- **`impl Trait` over `Box<dyn Trait>`** — Use `impl Provider` when the concrete type is known. `Box<dyn Tool>` only at the plugin IPC boundary where types are genuinely dynamic.
- **No `Default` as constructor** — Use `new()` with required args or builder. `Default` only for types where every field has a meaningful zero value.

### Crate-Specific Rules

Best practices for the specific crates in use.

- **`regex`** — Compile patterns at policy load time, not per-evaluation. Set `size_limit(1 << 20)` and `nest_limit(50)`. Use `unicode(false)` for command matching. Never use `fancy-regex` for policy patterns.
- **`serde`** — Use `#[serde(deny_unknown_fields)]` on all policy/config structs. Validate semantics after deserialization (regex compilation, tier validity, no duplicate rules).
- **`tracing`** — Use structured fields (`tracing::info!(tool = %name, decision = %result)`), not string interpolation. Every enforcement decision gets a span. Every tool execution gets a span.
- **`reqwest`** — Always set `connect_timeout(10s)`, `read_timeout(30s)`, `timeout(120s)`. Use `reqwest-eventsource` for SSE streaming from LLM providers.
- **`tokio`** — Use `tokio::process::Command` with `.kill_on_drop(true)`. Wrap all child process execution in `tokio::time::timeout()`. Use `.arg()` arrays, never shell string concatenation (even though we're executing bash — the command string goes as a single arg to `bash -c`).
- **`secrecy`** — Wrap all credential values in `SecretString`. The `Debug` impl auto-redacts. `expose_secret()` only at the six documented call sites: DB URL, API key, embedding key, credential broker, and the two crypto.rs master-key sites (hex validation + HKDF IKM). Not in general-purpose code.
- **`toml`** — Enforce file size limit before parsing. Strongly typed deserialization into Rust structs with `#[serde(deny_unknown_fields)]`.

## Build and Run

```bash
# Build (CLI only, ephemeral sessions)
cargo build

# Build with session persistence (requires PostgreSQL)
cargo build --features sessions

# Build with memory tool (requires PostgreSQL; implies postgres)
cargo build --features memory

# Build with Telegram connector
cargo build --features telegram

# Build with Telegram + session persistence
cargo build --features telegram,sessions

# Run CLI (requires ANTHROPIC_API_KEY env var)
ANTHROPIC_API_KEY=sk-... cargo run

# Run with session persistence (start PostgreSQL first: docker compose up -d)
DATABASE_URL=postgres://cherub:cherub_dev@localhost:5480/cherub \
  ANTHROPIC_API_KEY=sk-... cargo run --features sessions

# Run with memory tool (FTS-only search)
DATABASE_URL=postgres://cherub:cherub_dev@localhost:5480/cherub \
  ANTHROPIC_API_KEY=sk-... cargo run --features memory

# Run with memory tool + hybrid search (requires OPENAI_API_KEY for embeddings)
DATABASE_URL=postgres://cherub:cherub_dev@localhost:5480/cherub \
  ANTHROPIC_API_KEY=sk-... \
  OPENAI_API_KEY=sk-... \
  cargo run --features memory

# Run with custom policy
ANTHROPIC_API_KEY=sk-... cargo run -- --policy path/to/policy.toml

# Run Telegram bot (TELEGRAM_ALLOWED_CHATS is required)
TELEGRAM_BOT_TOKEN=... ANTHROPIC_API_KEY=sk-... TELEGRAM_ALLOWED_CHATS=123456,789012 cargo run --features telegram --bin cherub-telegram

# Telegram bot open to all users (not recommended)
TELEGRAM_BOT_TOKEN=... ANTHROPIC_API_KEY=sk-... TELEGRAM_ALLOWED_CHATS='*' cargo run --features telegram --bin cherub-telegram

# Start development database
docker compose up -d

# Test (no features — existing tests must pass unchanged)
cargo test

# Test with Telegram-specific tests
cargo test --features telegram

# Test enforcement layer specifically
cargo test enforcement

# ── cargo nextest — preferred test runner ─────────────────────────────────────
# DB integration tests (memory_store, session_persistence) TRUNCATE tables before
# each test. nextest serializes tests within a slot (one test per container at a
# time). `cargo test` runs parallel threads sharing the same container, causing
# TRUNCATE races — use nextest for all memory/sessions tests.

# Full memory test suite (non-DB + DB, auto-starts PostgreSQL via testcontainers)
cargo nextest run --features memory

# Test PgMemoryStore only (M6b + M6c hybrid search)
cargo nextest run --features memory --test memory_store

# Test with session persistence only
cargo nextest run --features sessions --test session_persistence

# Non-DB memory tests (enforcement + injection + RRF) — also work with cargo test
cargo nextest run --features memory --test memory_enforcement --test memory_injection

# Live embedding test (requires OPENAI_API_KEY, skipped by default)
OPENAI_API_KEY=sk-... cargo nextest run --features memory --test embedding_live -- --ignored
```

## Policy File Format (TOML)

```toml
[tools.bash]
enabled = true

[tools.bash.actions.read]
# Commands like ls, cat, find, grep, etc.
tier = "observe"
patterns = ["^ls ", "^cat ", "^find ", "^grep ", "^rg ", "^head ", "^tail ", "^wc ", "^file ", "^which ", "^echo ", "^pwd$", "^env$", "^whoami$"]

[tools.bash.actions.write]
# Commands like mkdir, cp, mv, touch, tee, etc.
tier = "act"
patterns = ["^mkdir ", "^cp ", "^mv ", "^touch ", "^tee ", "^git "]

[tools.bash.actions.destructive]
# Commands like rm, chmod, chown, kill, etc.
tier = "commit"
patterns = ["^rm ", "^chmod ", "^chown ", "^kill ", "^pkill ", "^sudo ", "^apt ", "^pip install", "^cargo install"]
```

The enforcement layer matches proposed commands against patterns in tier order: commit patterns are checked first (require approval), then act patterns (allowed if tier permits), then observe patterns (always allowed if tool is enabled). Anything that doesn't match any pattern is denied.

## Lessons from OpenClaw and Pi-Mono

Full source of both projects is available at `../openclaw/` and `../pi-mono/` for reference.

**From Pi (what to emulate):**
- Agent loop is ~418 lines. Keep Cherub's loop similarly small.
- Event-streaming architecture: `agent_start`, `turn_start`, `message_start/update/end`, `tool_execution_start/update/end`, `turn_end`, `agent_end`. Everything is an event. The audit log is the event stream.
- JSONL sessions with tree structure (id/parentId DAG). Append-only, never mutate entries.
- System prompt is ~160 lines. No behavioral guardrails in the prompt — the runtime handles safety.
- Tools are data (TypeBox schemas) with execute functions. Separate definition from implementation.
- Provider abstraction normalizes 20+ LLMs into one streaming interface.

**From OpenClaw (what to learn from and what to fix):**
- Channel plugin pattern: `probe()`, `connect()`, `send()`, `on_message()`, `tools()`. Solid interface for connectors.
- Plugin discovery via manifest files (`openclaw.plugin.json`) + filesystem scan.
- Tool policies exist but are allowlist-at-registration, not runtime enforcement. We enforce at runtime.
- Credentials stored in plaintext YAML config. We broker credentials — agent never sees values.
- Extensions run in-process with full permissions. We process-isolate plugins.
- Gateway is JSON-RPC over WebSocket. Good control plane pattern for later milestones.

## Working on This Project

- Read DESIGN.md before making architectural decisions. The design is deliberate.
- Read ROADMAP.md for current milestone and priorities.
- The enforcement layer is the most important code in the project. Changes to it require tests that prove the invariant holds.
- Prefer simple, obvious code over clever abstractions. This is a security project — reviewability matters more than elegance.
- Do not add dependencies without justification. Every dependency is attack surface.
