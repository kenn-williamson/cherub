# Cherub

Secure agent runtime. Deterministic capability enforcement for AI agents.

## What This Is

A Rust binary that owns the entire execution path from user message to tool execution. The model proposes actions as data. The runtime evaluates proposals against a policy. The model never touches tools directly. See DESIGN.md for the full architecture.

## Project Structure

```
cherub/
├── Cargo.toml
├── src/
│   ├── main.rs              # Entry point, CLI interface
│   ├── lib.rs               # Library entry point
│   ├── error.rs             # Error types
│   ├── runtime/
│   │   ├── mod.rs            # Agent loop
│   │   └── session.rs        # Conversation state, message history
│   ├── enforcement/
│   │   ├── mod.rs            # Enforcement layer entry point
│   │   ├── capability.rs     # Capability tokens (private constructors)
│   │   ├── policy.rs         # Policy loading and evaluation
│   │   └── tier.rs           # Observe/Act/Commit tier definitions
│   ├── tools/
│   │   ├── mod.rs            # Tool trait, tool registry
│   │   └── bash.rs           # Bash execution tool
│   └── providers/
│       ├── mod.rs            # Provider trait
│       └── anthropic.rs      # Anthropic API provider
├── tests/
│   ├── compile_tests.rs      # Compile-time invariant tests (trybuild)
│   └── ui/
│       ├── capability_token_private.rs      # Proves CapabilityToken can't be constructed outside enforcement
│       └── capability_token_private.stderr  # Expected compiler error output
├── config/
│   └── default_policy.toml   # Example policy file
├── DESIGN.md
├── ROADMAP.md
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

Use `enum` when variants are known at compile time. Use `dyn Trait` only at true extension boundaries (plugins loaded at runtime). In this project, the only legitimate `dyn Trait` boundaries are: `Provider` (multiple LLM backends) and `Tool` (plugin tools over IPC). Everything else should be an enum.

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
- **Credential isolation** — `secrecy::SecretString` for all credential values. `grep expose_secret` must only appear in the credential broker module. If it appears elsewhere, it's a bug.
- **No `unsafe`** — Zero `unsafe` blocks unless documented with a `// SAFETY:` comment explaining why it's necessary and what invariant the developer is upholding.

### Idiomatic Rust Rules (LLM Anti-Pattern Watchlist)

Specific patterns to catch and correct — based on what LLMs commonly get wrong when writing Rust.

- **Enum over trait objects** — If variants are known at compile time, use `enum` + `match`. The only `dyn Trait` in this project: `Provider` and `Tool` (plugin boundaries).
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
- **`secrecy`** — Wrap all credential values in `SecretString`. The `Debug` impl auto-redacts. `expose_secret()` only in the credential broker.
- **`toml`** — Enforce file size limit before parsing. Strongly typed deserialization into Rust structs with `#[serde(deny_unknown_fields)]`.

## Build and Run

```bash
# Build
cargo build

# Run (requires ANTHROPIC_API_KEY env var)
ANTHROPIC_API_KEY=sk-... cargo run

# Run with custom policy
ANTHROPIC_API_KEY=sk-... cargo run -- --policy path/to/policy.toml

# Test
cargo test

# Test enforcement layer specifically
cargo test enforcement
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
