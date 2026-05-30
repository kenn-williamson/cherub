# Plan: Shared Agent Builder (de-duplicate CLI vs Telegram construction)

**Status:** Implementation-ready. All previously-open decisions resolved 2026-05-30 (see §0).
**Correction 2026-05-30 (final review):** decision §0.2 was wrong — `impl Into<Arc<dyn Provider>>` does **not** accept the `Box::new(concrete_mock)` that ~15 test call sites pass (unsizing to `dyn Provider` never fires through a generic trait bound). The constructor now takes **explicit `Arc` params** and the call-site sweep is acknowledged as not churn-free. See §0.2, §6e, §10 step 1.
**Verified 2026-05-30** against current code — every claim below was re-checked by reading the actual files (signatures, the divergence table, the provider control flow, registry mutability, and tool-backend `Arc` ownership). Line numbers had drifted (project paused ~7 weeks) and have been refreshed, but treat them as approximate and re-grep before editing — the *signatures and shapes* matter more than the numbers.
**Owner decisions captured:** builder over positional fn; `gate`/`sink` are required (no typestate; skip the typestate builder); plus the 2026-05-30 resolutions in §0.

---

## 0. Resolved decisions (read this first)

These were open in the prior draft and are now settled. The rest of the doc reflects them; they are restated near the relevant section.

1. **Two lifetimes → two layers (the central decision).** The expensive, process/socket/DB-backed tool *backends* and the `ToolRegistry` are built **once at process startup** and **shared by `Arc`** across many agent loops. Each session/chat gets a cheap `AgentLoop` that clones the shared `Arc` handles and adds its own per-session state. This is required for **correctness**, not just efficiency: Telegram builds an agent *per chat*, and we must not spawn a fresh set of MCP server processes / Docker runtime / DB-backed credential broker per chat. See §6.
   - Unlocked by two verified facts (§14): (a) `ToolRegistry` is **immutable after construction** — every turn-path access is `&self`, `Tool::execute` is `&self`, and no per-session state lives in it; (b) the backends already hold their shared state behind `Arc` internally (`Arc<Mutex<McpClient>>`, `Arc<dyn ContainerRuntime>`, `Arc<PreparedModule>`, `Arc<CredentialBroker>`).
2. **`AgentLoop::new` takes explicit `Arc` params (single constructor; call sites are mechanically swept).** Its `provider`/`registry` params become **`Arc<dyn Provider>`** and **`Arc<ToolRegistry>`** — explicit, *not* `impl Into<…>`. This is **not** churn-free, and (per the owner) that's fine. The prior draft's "compiles unchanged via `.into()`" was **wrong**: a generic `impl Into<Arc<dyn Provider>>` param does **not** accept `Box<MockProvider>` (nor `Arc<MockProvider>`), because the unsizing coercion `Concrete → dyn Provider` never fires through a generic trait bound — it only fires when the destination type is a concrete trait-object type. Only the two **production** sites use a typed `Box<dyn Provider>` binding (`main.rs:969`, `session.rs:268`) and would survive; the ~15 **test** sites pass `Box::new(concrete_mock)` inline and would fail `E0277`. With an **explicit** `Arc<dyn Provider>` param, argument-position unsizing *does* fire, so the sweep is clean and cast-free: `Box::new(mock)` → `Arc::new(mock)`, `ToolRegistry::new()` (passed inline) → `Arc::new(ToolRegistry::new())`, and the two production sites pass `Arc::from(boxed_provider)`. One constructor = one audit path (better for a security review than adding a second `from_shared` ctor). Sharing the provider additionally makes `FailoverProvider`'s circuit breaker correctly **global** (one tripped breaker protects all chats). This reverses the prior draft's "no `AgentLoop` API change" line — the multi-session requirement justifies it.
3. **System prompt: the transport resolves a `String`; the builder does not build it.** Both transports already converge on the same default — `build_system_prompt(&cwd)` (`prompt.rs:106`). Only the *override source* differs (CLI: `CHERUB_SYSTEM_PROMPT_FILE` at `main.rs:1330`; Telegram: `SessionConfig::system_prompt_override` at `session.rs:436`). That divergence is inherently per-transport, so each transport resolves override-or-default and hands a finished `system_prompt: String` to `AgentConfig`. There is no duplication to centralize.
4. **`ProviderSpec` is an enum; do not normalize the flag path through `instantiate_provider`.** Capture both CLI paths as `ProviderSpec::{ Named(ProvidersConfig), Flags { provider_type, model, base_url, thinking_budget, max_tokens } }` and branch at construction exactly as `main.rs:969–1008` does today. Normalizing the flag path into a `ProviderDef` is deferred: `instantiate_provider` resolves the API key from an env-var *name* (`api_key_env`), whereas the flag path resolves an already-read key from a fixed env var — easy to get subtly wrong, and behavior-preservation is the CLI goal.
5. **Module placement: `src/app.rs`** — a thin app-assembly layer *above* `runtime`. The once-only layer spawns processes, opens the Docker socket, scans dirs, and reads the DB; that is app-level I/O orchestration and does not belong inside "the loop." `AgentLoop` stays in `runtime/` and only gains the `Arc` params from decision 2.

---

## 1. Problem

There is **no code path from model output to tool execution that doesn't go through `enforcement::evaluate()`** — that invariant is fine. The problem is one level up, in **how an agent is assembled**.

The transport seam already exists and is correct: `AgentLoop<A: ApprovalGate, O: OutputSink>` (`src/runtime/mod.rs:84`) is generic over the two genuinely-per-transport pieces — the **approval gate** (how we ask the human) and the **output sink** (where output goes). That's "transport = UI", done right.

What was **never extracted** is the *assembly* of the transport-agnostic core: policy + provider + `ToolRegistry` (all tools) + the cross-cutting services (memory injection, audit, cost, pricing, hooks, persistence). It is **open-coded twice**:

- **CLI:** `src/main.rs` (~lines 1075–1438), driven by clap flags.
- **Telegram:** `src/telegram/session.rs` `chat_session()` (~lines 257–643), driven by `SessionConfig`/env, **per chat**.

They started as copies and **drifted**. New capabilities were added to the CLI and never mirrored to Telegram.

## 2. Evidence — the divergence (current state, line numbers verified 2026-05-30)

Tool wiring on the `ToolRegistry`:

| Capability | CLI `main.rs` | Telegram `session.rs` |
|---|---|---|
| base registry (`new` / `new_without_bash` / `with_memory[_no_bash]`) | ✅ ~1076–1089 | ✅ ~352–365 (identical block) |
| sandbox bash + dev-env (`with_container` + `with_dev_environment`) | ✅ 1228–1229 | ✅ 380–381 |
| sub-agents (`with_sub_agents`) | ✅ 1324 | ✅ 425 |
| credentials / HTTP tool (`with_credentials`) | ✅ 1108 | ❌ |
| WASM tools (`with_wasm`) | ✅ 1150 | ❌ |
| container plugin tools (`with_container`) | ✅ 1191 | ❌ |
| **browser** (`with_container([browser])`) | ✅ 1252 | ❌ |
| MCP (`with_mcp`) | ✅ 1281 | ❌ |

Post-`new()` services (all take `&mut self`, mutate in place):

| Service | CLI | Telegram |
|---|---|---|
| `with_hook` (OutputStashingHook) | ✅ 1356 | ✅ 476 |
| `with_memory_injection` | ✅ 1363 | ✅ 483 |
| `with_audit_log` | ✅ 1376 | ✅ 497 |
| `with_cost_tracking` | ✅ 1392 | ✅ 501 |
| `with_pricing_table` | ✅ 1406 | ✅ 514 |
| `with_persistence` | ✅ 1421 (`"cli"`,`"default"`) | ✅ 531 (`"telegram"`, chat_id) |
| `with_show_thinking` | ✅ 1353 | ❌ (never set; uses `verbose` elsewhere) |
| `with_cancel_flag` | ❌ | ✅ 473 |
| `with_task_store` (async approval) | ❌ | ✅ 520 |
| `set_autonomous_mode` / `drain_approved_tasks` | ❌ | ✅ 561 (cron/drain path) |

**Takeaways:**
- CLI-only **tools**: credentials/HTTP, WASM, container-plugins, browser, MCP. These are transport-agnostic and *should* be available to both (gated by build feature + config). This is the bug.
- Telegram-only **services**: `cancel_flag`, `task_store`, autonomous mode. These are legitimately transport-shaped (CLI is synchronous/interactive; Telegram runs turns in background tasks with async approval). Keep them as opt-in. (`show_thinking` is CLI-only today but is transport-agnostic — see §8 note.)
- Symptom the user hit: the Telegram bot reaches for `curl`/`python` via bash because it has **no browser/HTTP tool registered** — purely because `session.rs` never got those lines.

## 3. Root cause

`main.rs` was the first/primary binary and assembled everything inline. When Telegram was added, the per-chat session manager (`session.rs`) needed to build an `AgentLoop` **per chat**, and someone **copied a subset** of `main.rs`'s assembly instead of extracting a shared builder. Every capability added since (WASM, MCP, browser, credentials/HTTP) went into `main.rs` only.

## 4. Goals & invariants to preserve

- **Behavior-preserving for CLI; deliberate capability grant for Telegram.** De-duplicating the assembly is a pure refactor for the CLI path (same tools, provider, policy, enforcement). For Telegram it is *intentionally* behavior-changing: routing `session.rs` through the shared assembly grants the bot the tools it was always supposed to have — HTTP/credentials, browser, MCP, WASM, container-plugins. This is a real expansion of the Telegram agent's tool surface, **not** a no-op, and should be acknowledged as such. It is safe because **every** call still passes through `enforcement::evaluate()` against the same policy — the tool surface widens, the enforcement boundary does not move. Do not describe this as "no behavior change."
- **Build expensive backends once; share by `Arc`; loops are cheap.** (§0.1) The `ToolRegistry` + its process/socket/DB-backed backends are constructed once and shared read-only across N loops. Per-session state (history, cancel flag, cost, persistence id) lives only on `AgentLoop`, never in the shared registry. Never spawn an MCP server / Docker runtime / credential broker per chat.
- **Transport = UI only.** A transport supplies `gate`, `sink`, and a few opt-in per-session services (cancel flag, task store, autonomous flag, persistence id). Nothing else.
- **`gate` and `sink` are unconditionally required.** There is no auto-approve/null gate and there must never be — the gate is the human-approval seam (deny-by-default + human-in-the-loop). `AgentLoop` has no `Default`, so "no agent without a gate" is a **compile-time guarantee today**; the refactor must keep it (making `gate`/`sink` required params of `.build(gate, sink)` preserves it).
- **Security invariants** (from CLAUDE.md "Key Invariants"): single enforcement path, private `CapabilityToken` constructors, model-output-as-data, policy opacity, deny-by-default — all untouched.
- **Per-chat factory.** Telegram builds an agent per chat; CLI builds one. So the per-session layer is a *factory* invoked N times, on top of a shared-services layer built once.

## 5. Builder decision (settled)

- **Skip the typestate builder.** Its only payoff is compile-time enforcement of *required-but-chainable* fields. We have none: `gate`/`sink` are simply required, and making them required params of `.build(gate, sink)` keeps the existing compile guarantee. An `Option`-based chain would *weaken* it to a runtime check.
- **Two layers (§0.1):** a `SharedAgentServices` value built **once** (`Arc<ToolRegistry>`, `Arc<dyn Provider>`, store `Arc`s, guards), and an `AgentBuilder` that **borrows** `&SharedAgentServices` and is invoked **per session**. Required generic UI (`gate`, `sink`) is handed to `.build(gate, sink)`; optional per-session services chain via consuming-self `.with_*()`.
- Adding a future capability = a new field on `AgentConfig` + a line in `build_registry` (if it's a tool) or a new `.with_x()` on `AgentBuilder` (if it's a per-session service); **no signature churn** anywhere.

## 6. Target design

Three types, matching the data → build-once → per-session flow:

```
AgentConfig            (data: what to build; produced by each transport)
   │  SharedAgentServices::build(&cfg).await?      ── ONCE, expensive
   ▼
SharedAgentServices    (Arc<ToolRegistry>, Arc<dyn Provider>, store Arcs, guards)
   │  AgentBuilder::new(&shared)…build(gate, sink) ── PER SESSION, cheap
   ▼
AgentLoop<A, O>        (clones Arcs + own per-session state)
```

### 6a. `AgentConfig` — transport-agnostic, data-only input
A plain struct each binary produces from its own config source (CLI: clap flags + env; Telegram: `SessionConfig` + env). It describes *what to build*; it does no I/O itself.

```rust
pub struct AgentConfig {
    pub policy: Policy,                 // Clone
    pub provider: ProviderSpec,         // enum — see below; owns model + token/thinking budgets
    pub system_prompt: String,          // RESOLVED by the transport (override-or-default); AgentLoop::new needs this, not a model name
    pub skip_builtin_bash: bool,        // sandbox-bash replaces in-process bash

    // shared stores / handles (Arc, cheap to clone, shared across all loops):
    pub memory_store: Option<Arc<dyn MemoryStore>>,
    pub audit_store: Option<Arc<dyn AuditStore>>,
    pub cost_store: Option<Arc<dyn CostStore>>,
    pub pricing_table: Option<PricingTable>,

    // tool sources, feature-gated (see §9) — paths/specs to load, NOT yet-constructed tools:
    pub credential_broker_source: Option<...>,   // master key + pool → broker built in build_registry
    pub wasm_dir: Option<PathBuf>,
    pub container_tools_dir: Option<PathBuf>,
    pub enable_sandbox_bash: bool,
    pub enable_browser: bool,
    pub mcp_config: Option<McpConfig>,
    pub sub_agents: Vec<SubAgentDef>,
}
```
Exact field set finalized during implementation; mirror what `main.rs` reads today.

**`system_prompt` (resolved §0.3).** It is a `String` the transport resolves (override-or-`build_system_prompt(&cwd)`) and passes in. The builder does not construct it — the shared default already lives in `prompt.rs`; only the override *source* is per-transport.

**`ProviderSpec` (resolved §0.4)** — an enum capturing the two mutually-exclusive CLI paths (`main.rs:969–1008`):
```rust
pub enum ProviderSpec {
    Named(ProvidersConfig),                    // --providers <config> → instantiate_named_provider(cfg, "default", &mut Vec::new())
    Flags {                                    // raw --provider/--model flags → direct construction
        provider_type: ProviderType,           //   "anthropic" → AnthropicProvider::new(..).with_thinking_budget(..)
        model: String,                         //   "openai"    → OpenAiProvider::new(..).with_base_url(..)
        base_url: Option<String>,
        thinking_budget: Option<u32>,
        max_tokens: u32,                        // DEFAULT_MAX_TOKENS unless overridden
    },
}
```
`SharedAgentServices::build` branches on this exactly as `main.rs` does today. Do **not** route `Flags` through `instantiate_provider` (§0.4).

### 6b. `build_registry(cfg: &AgentConfig) -> Result<(ToolRegistry, ResourceGuards)>` — async, ONE place, called ONCE
All the `#[cfg(feature = ...)]` tool wiring lives here, exactly once. Mirrors the current `main.rs` order:
base registry → credentials/HTTP → WASM → container plugins → sandbox bash + dev-env → browser → MCP → sub-agents.
This is the function that spawns MCP server processes, constructs the `BollardRuntime` (Docker socket), compiles WASM modules, builds the DB-backed credential broker, and creates IPC temp dirs. It runs **once**, inside `SharedAgentServices::build`.

> **Resource lifetime (clarified against current code).** The container/browser factories return `(Arc<ContainerTool>, PathBuf)` — the `PathBuf` IPC dirs have **no `Drop`** today (verified), so they don't auto-clean and `main.rs`'s `_ipc_dir` bindings are effectively documentary. The *real* lifetime requirement is satisfied automatically in the shared model: the MCP `kill_on_drop` child processes (inside `Arc<Mutex<McpClient>>`), the `BollardRuntime`, and the WASM epoch-ticker thread (inside `Arc<WasmToolRuntime>`) all live **inside the shared `Arc<ToolRegistry>`**, which `SharedAgentServices` holds for the whole process. `build_registry` still returns a `ResourceGuards` bag (the IPC `PathBuf`s) that `SharedAgentServices` keeps — good hygiene and a hook for future RAII cleanup — but correctness no longer depends on threading `_ipc_dir` bindings through each call site.

### 6c. `SharedAgentServices` — built ONCE at startup
```rust
pub struct SharedAgentServices {
    pub registry: Arc<ToolRegistry>,           // build_registry(), wrapped once
    pub provider: Arc<dyn Provider>,           // resolved from cfg.provider; shared → global failover circuit state
    pub policy: Policy,                        // Clone, cheap
    pub system_prompt: String,                 // resolved (§0.3); cloned per loop
    pub memory_store: Option<Arc<dyn MemoryStore>>,
    pub audit_store: Option<Arc<dyn AuditStore>>,
    pub cost_store: Option<Arc<dyn CostStore>>,
    pub pricing_table: Option<PricingTable>,
    _guards: ResourceGuards,                   // IPC PathBufs; kept for process lifetime
}

impl SharedAgentServices {
    pub async fn build(cfg: &AgentConfig) -> Result<Self, CherubError> {
        let provider: Arc<dyn Provider> = match &cfg.provider {
            ProviderSpec::Named(c)  => Arc::from(instantiate_named_provider(c, "default", &mut Vec::new())?),
            ProviderSpec::Flags{..} => Arc::from(build_flag_provider(cfg)?), // mirrors main.rs:974–1008
        };
        let (registry, guards) = build_registry(cfg).await?;
        Ok(Self { registry: Arc::new(registry), provider, policy: cfg.policy.clone(),
                  system_prompt: cfg.system_prompt.clone(), /* stores… */ _guards: guards, .. })
    }
}
```
Owned at the top of `main`/the telegram bot for the entire process. CLI builds it, uses it once. Telegram builds it once, then hands `&shared` (or `Arc<SharedAgentServices>`) to every `chat_session`.

### 6d. `AgentBuilder` — per-session, borrows `&SharedAgentServices`, cheap
```rust
let agent = AgentBuilder::new(&shared)         // borrows; clones Arc<ToolRegistry> + Arc<dyn Provider>
    .persistence(PersistenceId::telegram(chat_id))  // or ::cli()
    .cancel_flag(flag)             // Telegram only; CLI omits
    .task_store(task_store)        // Telegram only; CLI omits
    .autonomous(is_cron)           // Telegram autonomous turns
    .show_thinking(verbose)        // now available to both transports (§8 note)
    .build(gate, sink).await?;     // required generic UI
```
Internally `build(gate, sink)`:
1. `AgentLoop::new(shared.policy.clone(), shared.provider.clone(), shared.registry.clone(), shared.system_prompt.clone(), gate, sink, user_id)` — provider/registry are `Arc::clone` (cheap); the 4th arg is the **system prompt**, not a model name. (`shared.provider.clone()` / `shared.registry.clone()` are already `Arc<dyn Provider>` / `Arc<ToolRegistry>` — exactly the param types, no conversion.)
2. apply the configured optional services via the existing `AgentLoop::with_*` methods (all `&mut self`).
3. `with_persistence(...)` last (it's async, returns `Result`) if a persistence id was set.
4. return the `AgentLoop`.

No tool wiring, no provider construction, no I/O here — that all happened once in §6c. This is safe to call N times.

`AgentLoop`'s existing `with_*` are `&mut self`; `AgentBuilder` is the consuming-self layer that *drives* them. We do not change those.

### 6e. `AgentLoop::new` signature change (§0.2)
```rust
pub fn new(
    policy: Policy,
    provider: Arc<dyn Provider>,   // was: Box<dyn Provider>
    registry: Arc<ToolRegistry>,   // was: ToolRegistry
    system_prompt: String,
    approval_gate: A,
    output: O,
    user_id: &str,
) -> Self
```
Explicit `Arc` params, **not** `impl Into<Arc<…>>` — see §0.2 for why the `impl Into` form is a trap (it rejects both `Box::new(concrete_mock)` and `Arc::new(concrete_mock)`, since unsizing to `dyn Provider` won't fire through a generic bound). With the param typed as the concrete `Arc<dyn Provider>`, argument-position unsizing fires, so call sites pass `Arc::new(mock)` (tests) or `Arc::from(boxed_provider)` (production) with no casts. The struct fields change `registry: ToolRegistry` → `Arc<ToolRegistry>` (`mod.rs:88`) and `provider: Box<dyn Provider>` → `Arc<dyn Provider>`. This is a mechanical, **not** churn-free sweep across all call sites (§10 step 1). Verify every turn-path use of `self.registry`/`self.provider` is fine through `Arc` deref — it is today (all `&self`; see §14).

## 7. Module placement (resolved §0.5)
Create **`src/app.rs`** (or a small `src/app/` module) exposing `AgentConfig`, `ProviderSpec`, `SharedAgentServices`, `AgentBuilder`, `PersistenceId`, and `build_registry`. This is the app-assembly layer: it spawns processes, opens the Docker socket, scans dirs, and reads the DB. Keeping it *above* `runtime` (depending on `runtime`, not living inside it) keeps `runtime` focused on the turn loop — consistent with the project's "keep the loop small" value. `AgentLoop` stays in `runtime/` and only gains the §6e `Arc` params. Both binaries reach `app` through the library (`lib.rs` adds `pub mod app;`).

## 8. What each transport keeps (the only per-transport code)
- **Config parsing** into `AgentConfig` (clap vs env/`SessionConfig`), including resolving `system_prompt` from its own override source (§0.3). Stays in `main.rs` / `bin/telegram.rs`.
- **Building `SharedAgentServices` once** and owning it for the process lifetime. CLI: build, use once. Telegram: build once, share `&shared`/`Arc<SharedAgentServices>` into every spawned `chat_session`.
- **Session lifecycle**: CLI builds one `AgentLoop`; Telegram's `session_manager` builds one per chat (the `chat_senders` map + spawned `chat_session` tasks stay as-is) — now via `AgentBuilder::new(&shared)`.
- **The UI values**: `CliApprovalGate`+`StdoutSink` vs `TelegramApprovalGate`+`TelegramSink`. The transport constructs its own gate and passes the finished value to `.build(gate, sink)`.
  - **Note:** Telegram wires `task_store` onto the *gate* too — `TelegramApprovalGate::new(...).with_task_store(...)` at `session.rs:452–456`, before agent construction — in addition to the agent-side `.task_store()`. Since the transport owns gate construction, this gate-side wiring stays transport-side and is *not* something the builder does. Don't lose it: the builder's `.task_store()` covers only the agent; the gate's copy is the transport's job.
- **Opt-in per-session services**: Telegram passes `cancel_flag`, `task_store`, `autonomous`; both can pass `show_thinking`.
  - **`show_thinking` note:** routing Telegram through the builder makes `with_show_thinking` available to it for the first time (today it's CLI-only). Decide the Telegram default explicitly (e.g. map to its existing `verbose`) so it's intentional, not incidental.

Everything else moves into the shared layer.

## 9. Feature-gating strategy
Keep all `#[cfg(feature = "...")]` **inside `build_registry`** (and the `AgentConfig` field definitions), not scattered across binaries. For features not compiled in, the corresponding `AgentConfig` fields simply aren't populated / the cfg'd branch is absent. After the refactor, building `cherub-telegram` with `--features telegram,sessions,memory,browser,container` (plus Docker + `cherub-browser:latest`) gives the bot the browser tool **for free** — it now calls the same `build_registry`. (The refactor removes the *wiring* gap; it does not force features on.)

## 10. Migration plan (incremental; tests green after each step)

1. **Change `AgentLoop::new` to explicit `Arc` params (§6e).** Field *and* param types both become `Arc<ToolRegistry>` / `Arc<dyn Provider>` (explicit). This is a mechanical sweep, **not** churn-free: update every call site — ~15 test files plus the two production sites. Transformation: `Box::new(mock)` → `Arc::new(mock)`; inline `ToolRegistry::new()` → `Arc::new(ToolRegistry::new())`; the production `let provider: Box<dyn Provider> = …` sites pass `Arc::from(provider)`. (`Arc::new(concrete_mock)` compiles because the param is the concrete `Arc<dyn Provider>` type, so argument-position unsizing fires — this is exactly why §6e uses explicit `Arc` and **not** `impl Into<Arc<dyn Provider>>`, which would reject it.) Also fix `swap_provider` (`mod.rs:220`): its body becomes `self.provider = Arc::from(provider)` (param + its one caller `session.rs:623` stay as-is — see §14). Then `cargo build --all-features && cargo test --all-features --no-run` to confirm the sweep is complete. Re-check every `self.registry`/`self.provider` use compiles through `Arc` deref. This isolated first step unblocks sharing.
2. **Extract `build_registry`.** Move `main.rs`'s tool-assembly block into `app::build_registry(...)`. Have **both** `main.rs` and `session.rs` call it. *This step alone closes the 5-tool gap.* (Intertwined with step 3 — `build_registry` needs *some* parameter shape. Either define a minimal `AgentConfig` first and fold steps 2–3, or have `build_registry` take loose params initially and switch to `&AgentConfig` in step 3. Don't write `build_registry(cfg)` before `cfg`'s type exists.) Verify: `cargo build --all-features`; CLI still runs; Telegram (built with browser/container) now registers browser/HTTP/WASM/MCP/container.
3. **Introduce `AgentConfig` + `ProviderSpec`.** Define them (§6a); have `main.rs` and `bin/telegram.rs` populate them from their config sources (resolving `system_prompt`). `build_registry` takes `&AgentConfig`.
4. **Introduce `SharedAgentServices::build` (§6c).** Move provider instantiation + `build_registry` + `Arc`-wrapping here. CLI builds it once. **Verify the per-chat fix:** Telegram builds it **once** (not per chat); add an assertion/log that `build_registry` runs exactly once for the bot.
5. **Introduce `AgentBuilder` (§6d).** Wrap the `with_*` chain + `AgentLoop::new`. Migrate `main.rs` to `AgentBuilder::new(&shared).…build(CliApprovalGate, StdoutSink)`.
6. **Migrate Telegram.** `chat_session()` calls `AgentBuilder::new(&shared).…build(TelegramApprovalGate, TelegramSink)` per chat, passing `cancel_flag`/`task_store`/`autonomous`/persistence id. Keep the gate-side `with_task_store` (§8).
7. **Delete the duplicated assembly** from both sites; confirm both go through the shared layer. Final invariant + test sweep.

Each step is independently compilable and testable; stop and verify between steps.

## 11. Risks & invariant checklist (run before/after)
- `grep` audit: `CapabilityToken` still has no `pub fn new`/`Default`/`From`/`Clone`/`Copy`; only `enforcement/` constructs it.
- Every tool `execute()` still requires a `CapabilityToken`.
- No policy strings leak into errors (policy opacity).
- `expose_secret()` still only at the documented call sites. The broker construction *moves* (into `build_registry`) but the `expose_secret` calls live in `credential_broker.rs`/`credential_types.rs` and don't move — **call-site count is unchanged**. Re-verify against the CLAUDE.md list anyway.
- `gate`/`sink` remain required (no `Default`, no `Option`); `AgentLoop` still has no `Default`.
- **New (`Arc` change):** confirm the full suite compiles after the call-site sweep (every site now constructs `Arc`s — §10 step 1), and that no turn-path code needed `&mut`/ownership of registry or provider (verified read-only today — §14). Note `Arc::new(concrete_mock)` relies on argument-position unsizing into the explicit `Arc<dyn Provider>` param — do **not** switch the param to `impl Into<Arc<dyn Provider>>` (it would reject `Arc::new(concrete)`/`Box::new(concrete)`).
- **New (per-chat correctness):** assert/log that `SharedAgentServices::build` (hence `build_registry`, hence MCP spawn / Docker connect / broker build) runs **once per process**, not per chat.
- Run `/rust-review src/app.rs` (and any touched `runtime`/`tools` files) and address blocking findings.

## 12. Testing
- All existing tests must pass with **behavior unchanged** (the call-site sweep mechanically edits each `AgentLoop::new` invocation, but no test logic changes): `cargo test --all-features` + the non-DB suite in the commit skill + `cargo nextest run --features memory` for DB tests. (The `Arc`-param change touches every call site — §10 step 1 — so full-suite compilation *is* the coverage.)
- **New test — tool-set parity:** given one `AgentConfig`, assert the `ToolRegistry` tool set is identical whether assembled for the CLI or Telegram path (no transport affects the tool list). Use `ToolRegistry::definitions()` (`src/tools/mod.rs:584`) → map `.name` to a set. (`find()` is `pub(crate)`; there is no public name-list helper — add a tiny one only if `.map(|d| d.name)` reads poorly.)
- **New test — shared registry, isolated sessions:** build one `SharedAgentServices`, build two `AgentLoop`s from it (two `PersistenceId`s), and assert both share the same `Arc<ToolRegistry>` (e.g. `Arc::ptr_eq`) while their per-session state is independent. Guards the §0.1 invariant.
- Manually verify the Telegram bot (built with `browser,container` + Docker) now exposes the browser tool (the model stops using curl/python for JS-heavy pages), and that only **one** set of MCP/container backends is spawned regardless of chat count.

## 13. Out of scope (note, don't do here)
- **Identity model (channel vs. user).** Today `user_id = chat_id` (`session.rs:331`) and `msg.from` is never read, so a *group* chat shares one session/memory. The shared registry/backends are *intended* to be shared across chats — but session/memory state must stay per-`AgentLoop` (it does today). If groups/multi-user are ever used it needs `(chat_id, from.id)` keying and per-user memory scoping. Track separately.
- User-facing incremental streaming (M18b) — already deferred; wire-level SSE already shipped.

## 14. Reference index (file:line, current code — verified 2026-05-30)

> Re-verified by reading the files; line numbers are close but treat as approximate — the *signatures and shapes* matter more.

**`AgentLoop` (`src/runtime/mod.rs`):** struct `:84` (`AgentLoop<A: ApprovalGate, O: OutputSink>`); field `registry: ToolRegistry` `:88` (owned by value today → becomes `Arc<ToolRegistry>`); provider field is `Box<dyn Provider>` → becomes `Arc<dyn Provider>`. `new()` `:135–142`, params **`(policy: Policy, provider: Box<dyn Provider>, registry: ToolRegistry, system_prompt: String, approval_gate: A, output: O, user_id: &str)`** — the 4th arg is the **system prompt**; the model is read from `provider.model_name()`, not passed in. `with_cancel_flag` :175; `with_memory_injection` :187; `with_audit_log` :202; `with_cost_tracking` :214; `with_pricing_table` :277; `with_persistence` :286 (**async**, `Result<(), CherubError>`); `with_show_thinking` :307; `with_hook` :315; `with_task_store` :324; `set_autonomous_mode` :337; `drain_approved_tasks` :350 (**async**, `usize`). **All `with_*`/`set_*` are `&mut self`** (in-place). **Also `swap_provider` `:220`** (`&mut self`, the `/model` hot-swap; one caller `session.rs:623`) — the only provider-field *mutator*, missed by the original draft. It assigns `self.provider`, so the `Arc` move touches it: keep its `Box<dyn Provider>` param and the caller unchanged, change only the body to `self.provider = Arc::from(provider)`. Semantically correct — a per-chat `/model` switch re-points only that loop's `Arc`, never the shared one. **No `impl Default`** — the "no agent without a gate/sink" guarantee is compile-time; keep it.

**Registry is immutable after construction (this is what makes `Arc<ToolRegistry>` sharing safe):** every turn-path access is read-only — `registry.definitions()` at `new` (`:144`), `self.registry.enforcement_name()` / `enrich_params()` (`:1253`/`:1255`), the concurrent `evaluated.execute(token, registry, ctx)` (`:1486`), and `drain_approved_tasks` (`:395`). All mutation happens via the `with_*` constructors *before* `AgentLoop::new`. `Tool::execute` / `ToolImpl::execute` is **`&self`** (`tools/mod.rs:185–192`); `ToolRegistry::find()` returns `Option<&ToolImpl>` (`:530`, `pub(crate)`).

**`ToolRegistry` (`src/tools/mod.rs`):** `new` `:412`; `new_without_bash` :425; `with_memory` :433; `with_memory_no_bash` :447; `with_credentials` :461; `with_wasm` :473; `with_container` :482; `with_dev_environment` :490; `with_mcp` :499; `with_sub_agents` :507 (the `with_*` are consuming-self → `Self`). `definitions() -> Vec<ToolDefinition>` :584. `ToolImpl` enum `:146–162` (note `Container(Arc<ContainerTool>)` `:156`).

**Tool backends already hold shared state behind `Arc` (so the backends are shareable; the wrapper types are NOT `Clone`):**
- `McpToolProxy` `src/tools/mcp/proxy.rs:18` — `client: Arc<Mutex<McpClient>>` `:30`.
- `ContainerTool` `src/tools/container/wrapper.rs:68` — `runtime: Arc<dyn ContainerRuntime>` `:70`, `state: Mutex<ContainerState>` `:85`.
- `WasmTool` `src/tools/wasm/wrapper.rs:311` — `module: Arc<PreparedModule>` `:312`, `runtime: Arc<WasmToolRuntime>` `:313`.
- `CredentialBroker` `src/tools/credential_broker.rs:51` — `store: Arc<dyn CredentialStore>` `:52`.
- `DevEnvironmentTool` `src/tools/dev_environment.rs:34` — `sandbox_bash: Arc<ContainerTool>` `:35`.
- `SubAgentTool` `src/tools/sub_agent.rs:29` — owns `Box<dyn Provider>` + `ToolRegistry` (not `Arc`-backed, not `Clone`). This is *why* we share the whole `ToolRegistry` rather than make every tool wrapper `Clone` (which would dead-end here).

**Expensive construction (run once in `build_registry`):**
- MCP `load_from_config()` `src/tools/mcp/loader.rs:33` — **spawns server processes** (`TokioChildProcess`, `.kill_on_drop(true)`), JSON-RPC handshake, returns `Vec<McpToolProxy>`.
- Container plugins `load_from_dir()` `src/tools/container/loader.rs:101` + `BollardRuntime::new()` `src/tools/container/runtime.rs:134` — **Docker socket** connect; one `Arc<dyn ContainerRuntime>` shared across tools; returns `Vec<Arc<ContainerTool>>`.
- Sandbox bash `container_bash::build()` `:31` → `(Arc<ContainerTool>, PathBuf)`; browser `container_browser::build()` `:24` → `(Arc<ContainerTool>, PathBuf)`. The `PathBuf` IPC dirs have **no `Drop`** (don't auto-clean); `main.rs`'s `_sandbox_bash_ipc_dir` `:1208` / `_browser_ipc_dir` `:1239` bindings are documentary.
- WASM `load_from_dir()` `src/tools/wasm/loader.rs:39` + `WasmToolRuntime::new()` `src/tools/wasm/runtime.rs:62` — **compiles modules** (CPU-bound, `spawn_blocking`), **spawns an epoch-ticker thread** for process lifetime; returns `Vec<WasmTool>`.
- Credential broker `CredentialBroker::new()` `:56` — needs a `Pool`/`CredentialStore` (**DB**); built only if `CHERUB_MASTER_KEY` + pool present.

**Provider (`src/providers/config.rs`):** `instantiate_provider(def: &ProviderDef) -> Result<Box<dyn Provider>, CherubError>` `:249`; `instantiate_named_provider(config: &ProvidersConfig, name: &str, ancestry: &mut Vec<String>) -> Result<Box<dyn Provider>, CherubError>` `:294–298`. `ProviderDef` `:49` has `max_tokens` `:69` and `thinking_budget` `:74` (provider-level, *not* on `AgentLoop`). `Provider::send` is `&self` (so `Arc<dyn Provider>` sharing is safe and gives `FailoverProvider` a global circuit breaker). `Arc::from(boxed_provider)` converts the `Box` the instantiate fns return.

**CLI provider path (NOT failover):** two mutually-exclusive branches at `src/main.rs:969–1008` — named-config (`instantiate_named_provider(.., "default", ..)`) vs. raw flags (direct `AnthropicProvider::new`/`OpenAiProvider::new`; `with_thinking_budget` inline at `:1002`; `with_base_url` inline for OpenAI; `DEFAULT_MAX_TOKENS`). The flag path **never** calls `instantiate_provider`. → modeled by `ProviderSpec` (§6a).

**System prompt:** `build_system_prompt(cwd: &str) -> String` `src/runtime/prompt.rs:106` (shared default). CLI override: `CHERUB_SYSTEM_PROMPT_FILE` then fallback, `main.rs:1330–1340`. Telegram override: `SessionConfig::system_prompt_override` then fallback, `session.rs:436–438`.

**Assembly sites:** CLI `src/main.rs` — registry wiring ~1076–1324; `AgentLoop` construction (`system_prompt` at :1348) + `with_*` ~1348–1438. Telegram `src/telegram/session.rs` `chat_session()` ~257–643 (one `AgentLoop` per chat, construction ~:464); `user_id = chat_id.to_string()` :331; `with_persistence(.., "telegram", &connector_id)` ~:531; gate-side `TelegramApprovalGate::…with_task_store(...)` :452–456 (§8). Session manager: `:98`; `chat_senders: HashMap<ChatId, …>` :103.

**Gate impls:** `CliApprovalGate` `src/runtime/approval.rs:33`; `TelegramApprovalGate` `src/telegram/approval.rs:75`. **Sink impls:** `StdoutSink` `src/runtime/output.rs:48`; `NullSink` :85; `TelegramSink` `src/telegram/output.rs:41`. (No null/auto gate — intentional.)
