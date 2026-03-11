# Cherub

Deterministic capability enforcement for AI agents.

## What Is This

Every existing AI agent framework places its trust boundary at the prompt level — asking the model nicely not to do dangerous things. Cherub takes a fundamentally different approach: **the agent runtime makes rule violation structurally impossible, regardless of what the model intends.**

Cherub is a Rust binary that owns the entire execution path from user message to tool execution. The model proposes actions as data. The runtime evaluates proposals against a policy. The model never touches tools directly.

The enforcement layer is deterministic, non-negotiable, and invisible to the agent.

## How It Works

```
User message
    ↓
LLM Provider (Anthropic, OpenAI, Ollama, ...)
    ↓
Model output (parsed as data, not code)
    ↓
Enforcement Layer
    ├── Extract action from tool call
    ├── Match against policy rules
    ├── Evaluate tier (observe / act / commit)
    │       ├── observe  → auto-allow (read-only, no risk)
    │       ├── act      → auto-allow (reversible changes)
    │       └── commit   → require human approval (irreversible)
    ├── Check constraints and budget
    └── Issue CapabilityToken (unforgeable, compiler-enforced)
    ↓
Tool Execution (only with valid token)
    ↓
Result returned to model
```

**Key guarantees:**

- **Deny by default.** If the policy doesn't explicitly permit an action, it is denied. Unknown tools, unknown actions, ambiguous matches — all denied.
- **Capability tokens have private constructors.** Only the enforcement layer can create them. No public `new()`, no `Default`, no `From`. The Rust compiler enforces this.
- **The agent never sees the policy.** Rejection returns a generic "action not permitted" — no rule names, no hints about what would be allowed.
- **No bypass path.** There is no code path from model output to tool execution that does not pass through enforcement.

## Features

### Core Runtime
- **Enforcement layer** — Deterministic policy evaluation with capability tokens
- **Three-tier model** — Observe (read-only), Act (reversible), Commit (requires approval)
- **Approval gates** — CLI interactive approval, Telegram inline keyboard approval
- **Lifecycle hooks** — Pre/post tool execution hooks with output stashing
- **Audit log** — Append-only event log of all enforcement decisions
- **Context compaction** — Automatic conversation summarization to manage context windows

### Providers
- **Anthropic** — Claude models with extended thinking support
- **OpenAI-compatible** — OpenAI, Ollama, vLLM, Groq, and any OpenAI-compatible API
- **Failover** — Ordered failover with circuit breaker across providers
- **Sub-agents** — Delegate tasks to cheaper models with bounded tool subsets
- **Multi-provider config** — Named providers and agent definitions in TOML

### Tools
- **Bash** — Shell execution with command-level policy enforcement
- **File operations** — Read, edit, glob, grep with workspace containment
- **Memory** — Store/recall/search with hybrid FTS + vector search, contradiction detection
- **HTTP** — GET/POST/PUT/PATCH/DELETE with credential broker injection and leak detection
- **WASM sandbox** — Run untrusted tools in Wasmtime with fuel/memory/timeout limits
- **Container sandbox** — Docker/Podman isolated execution with IPC protocol
- **MCP** — Model Context Protocol server integration with per-tool enforcement
- **Dev environment** — Build sandbox images with language toolchains (Rust, Node, Go)

### Connectors
- **CLI** — Interactive terminal with streaming output
- **Telegram** — Multi-chat bot with photo support, turn batching, session persistence

### Persistence
- **Session storage** — PostgreSQL-backed conversation persistence
- **Cost tracking** — Per-call token usage and cost with budget enforcement
- **Credential vault** — AES-256-GCM encrypted credentials, never exposed to the agent

## Quick Start

```bash
# Build
cargo build

# Run (ephemeral sessions, Anthropic provider)
ANTHROPIC_API_KEY=sk-... cargo run
```

That's it. The default policy (`config/default_policy.toml`) enforces tiered access on bash commands — read commands auto-allow, write commands auto-allow, destructive commands require your approval.

## Configuration

### Policy File

Policies are TOML files that define what the agent can do. Each tool has actions with tier-based enforcement and pattern matching.

```toml
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls ", "^cat ", "^grep ", "^find "]

[tools.bash.actions.write]
tier = "act"
patterns = ["^mkdir ", "^cp ", "^mv ", "^git "]

[tools.bash.actions.destructive]
tier = "commit"
patterns = ["^rm ", "^sudo ", "^chmod "]

[tools.file]
enabled = true
match_source = "structured"

[tools.file.actions.read]
tier = "observe"
patterns = ["^read$", "^glob$", "^grep$"]

[tools.file.actions.write]
tier = "act"
patterns = ["^edit$", "^write$"]

[tools.http]
enabled = true
match_source = "http_structured"

[tools.http.actions.public_apis]
tier = "act"
patterns = ["^GET:api\\.github\\.com$", "^GET:httpbin\\.org$"]

[budget]
session_limit_usd = 5.00
daily_limit_usd = 25.00
on_exceeded = "escalate"
```

Run with a custom policy:

```bash
cargo run -- --policy path/to/policy.toml
```

### Feature Flags

| Feature | What it enables | Implies |
|---|---|---|
| `postgres` | Database infrastructure (connection pool, migrations) | — |
| `sessions` | Session persistence across restarts | `postgres` |
| `memory` | Memory tool (store/recall/search) | `postgres` |
| `credentials` | Encrypted credential vault + HTTP tool | `postgres` |
| `wasm` | WASM sandbox for untrusted tools | — |
| `container` | Docker/Podman container sandbox | — |
| `mcp` | MCP server support | — |
| `telegram` | Telegram bot connector | — |

Default build (no features): CLI with in-process bash, single provider, ephemeral sessions.

### Provider Configuration

For multi-provider setups with failover and sub-agents, use a providers config file:

```bash
cargo run -- --providers config/example_providers.toml
```

See `config/example_providers.toml` for the full format.

## Build & Run

```bash
# ── Providers ─────────────────────────────────────────────────────────

# Anthropic (default)
ANTHROPIC_API_KEY=sk-... cargo run

# OpenAI
OPENAI_API_KEY=sk-... cargo run -- --provider openai

# OpenAI with specific model
OPENAI_API_KEY=sk-... cargo run -- --provider openai --model gpt-4o-mini

# Local Ollama (no API key needed)
cargo run -- --provider openai --base-url http://localhost:11434/v1 --model llama3

# Extended thinking (Anthropic only)
ANTHROPIC_API_KEY=sk-... cargo run -- --thinking-budget 8000 --show-thinking

# ── Persistence ───────────────────────────────────────────────────────

# Start development database
docker compose up -d

# With session persistence
DATABASE_URL=postgres://cherub:cherub_dev@localhost:5480/cherub \
  ANTHROPIC_API_KEY=sk-... cargo run --features sessions

# With memory tool (FTS-only)
DATABASE_URL=postgres://cherub:cherub_dev@localhost:5480/cherub \
  ANTHROPIC_API_KEY=sk-... cargo run --features memory

# With memory tool + hybrid vector search
DATABASE_URL=postgres://cherub:cherub_dev@localhost:5480/cherub \
  ANTHROPIC_API_KEY=sk-... OPENAI_API_KEY=sk-... cargo run --features memory

# ── Sandboxing ────────────────────────────────────────────────────────

# Container-sandboxed bash (requires Docker + built image)
# Build image first:
docker build -t cherub-sandbox-bash:latest tools/container/sandbox-bash/
# With language toolchains:
docker build --build-arg LANGUAGES="rust,node,go" -t cherub-sandbox-bash:latest tools/container/sandbox-bash/
# Run:
ANTHROPIC_API_KEY=sk-... cargo run --features container -- --sandbox-bash

# ── MCP ───────────────────────────────────────────────────────────────

# With MCP servers
ANTHROPIC_API_KEY=sk-... cargo run --features mcp -- --mcp-config config/mcp_servers.toml

# ── Telegram ──────────────────────────────────────────────────────────

# Telegram bot
TELEGRAM_BOT_TOKEN=... ANTHROPIC_API_KEY=sk-... TELEGRAM_ALLOWED_CHATS=123456 \
  cargo run --features telegram --bin cherub-telegram

# Telegram with all features
CHERUB_SANDBOX_BASH=1 DATABASE_URL=... TELEGRAM_BOT_TOKEN=... \
  ANTHROPIC_API_KEY=sk-... TELEGRAM_ALLOWED_CHATS=123456 \
  cargo run --features telegram,sessions,container --bin cherub-telegram

# ── Management Commands ───────────────────────────────────────────────

# Credential vault (requires --features credentials)
cargo run --features credentials -- credential store my-api-key
cargo run --features credentials -- credential list

# Audit log (requires --features postgres)
cargo run --features postgres -- audit list --tool bash --limit 50

# Cost tracking (requires --features postgres)
cargo run --features postgres -- cost summary
cargo run --features postgres -- cost history --days 7
```

## Testing

```bash
# Base tests (no features, no external dependencies)
cargo test

# Full test suite with nextest (recommended — serializes DB tests correctly)
cargo nextest run --features memory

# Feature-specific tests
cargo nextest run --features sessions --test session_persistence
cargo nextest run --features container --test container_bash
cargo build --example mock_mcp_server --features mcp && \
  cargo nextest run --features mcp --test mcp_integration

# Enforcement layer tests
cargo test enforcement

# Live tests (require API keys, skipped by default)
ANTHROPIC_API_KEY=sk-... cargo test --test redteam -- --ignored
OPENAI_API_KEY=sk-... cargo nextest run --features memory --test embedding_live -- --ignored
```

Database integration tests use [testcontainers](https://docs.rs/testcontainers) to automatically start PostgreSQL — no manual `docker compose up` needed for tests. Use `cargo nextest` (not `cargo test`) for database tests to avoid TRUNCATE race conditions.

## Architecture

```
src/
├── main.rs              # CLI entry point
├── runtime/             # Agent loop, approval gates, hooks, sessions
├── enforcement/         # Policy evaluation, capability tokens, tier system
├── tools/               # Bash, file, memory, HTTP, WASM, container, MCP
├── providers/           # Anthropic, OpenAI, failover, sub-agents, pricing
├── storage/             # PostgreSQL: sessions, memory, credentials, audit, cost
└── telegram/            # Telegram bot connector
```

See [DESIGN.md](DESIGN.md) for the full architectural design, threat model, and design rationale.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
