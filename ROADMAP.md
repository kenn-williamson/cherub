# Cherub — MVP Roadmap

The goal is to prove the core thesis as fast as possible: **a Rust binary can enforce deterministic capability tiers on AI agent tool calls, and the model cannot bypass the enforcement layer.**

Everything else — connectors, credential brokering, audit logging, IPC plugins — is elaboration. The thesis comes first.

---

## Milestone 0: Skeleton

**Goal:** Cargo project compiles. Directory structure exists. You can `cargo build` and `cargo test` with no errors.

- [ ] `cargo init` with workspace structure
- [ ] Create module stubs: `runtime`, `enforcement`, `tools`, `providers`
- [ ] Define core traits: `Tool`, `Provider`, `ToolProposal`, `CapabilityToken`
- [ ] `CapabilityToken` with private constructor — verify it cannot be constructed outside `enforcement` module
- [ ] Define `Tier` enum: `Observe`, `Act`, `Commit`
- [ ] Compiles, tests pass (even if tests are trivial)

**This milestone is about the type system.** The capability token's private constructor is the first load-bearing security guarantee. If this is wrong, everything built on top is theater.

---

## Milestone 1: Enforcement Layer

**Goal:** The enforcement layer can evaluate a tool proposal against a TOML policy and return allow/reject/escalate. No model involved yet — pure unit tests.

- [ ] Policy file parser (TOML → internal policy struct)
- [ ] Default policy: deny everything not explicitly permitted
- [ ] Pattern matching engine for bash commands against policy rules
- [ ] `evaluate(proposal, policy) → Decision` function (the core of the project)
- [ ] Decision types: `Allow(CapabilityToken)`, `Reject`, `Escalate`
- [ ] `Allow` returns a `CapabilityToken` — the only way to obtain one
- [ ] Tests:
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

- [ ] Anthropic API provider: send messages, receive tool-use responses
- [ ] Streaming support (print tokens as they arrive)
- [ ] System prompt defining the agent's tool-calling interface
- [ ] Agent loop: message → model → parse tool proposals → enforce → execute or reject → feed result back to model
- [ ] CLI interface: readline-style input, formatted output
- [ ] Bash tool: receives `CapabilityToken` + command, executes, returns output
  - Bash tool function signature requires `CapabilityToken` parameter — cannot be called without one
- [ ] Wire it together: user types "list files in /tmp" → model proposes `ls /tmp` → enforcement checks policy → observe tier → allowed → executes → model sees result → responds

**This milestone is the thesis proven.** A human can sit at the terminal, interact with the agent, and observe that:
1. Read-only commands execute freely
2. Write commands execute if the policy allows Act tier
3. Destructive commands are blocked or escalated
4. The model cannot talk its way past the enforcement layer

---

## Milestone 3: Approval Gates

**Goal:** Commit-tier actions pause and ask the human for approval via the CLI.

- [ ] When enforcement returns `Escalate`, the CLI displays: what the model wants to do, why, and asks for y/n
- [ ] Approval → enforcement issues `CapabilityToken` → tool executes
- [ ] Denial → generic rejection sent to model
- [ ] Timeout → denial (configurable, default 60s)
- [ ] Model receives the same generic "action not permitted" for both denial and timeout — no information leakage

---

## Milestone 4: Hardening

**Goal:** The enforcement layer handles adversarial inputs correctly.

- [ ] Command injection tests: can the model craft a command that looks like `ls` but executes `rm`? (pipes, semicolons, backticks, $(), &&, ||)
- [ ] Policy bypass tests: edge cases in pattern matching (unicode, whitespace, null bytes)
- [ ] Context window isolation: model output parsing handles malformed tool calls gracefully
- [ ] The model proposes multiple tools in one response — each evaluated independently
- [ ] Error handling: what happens when the Anthropic API is down? When bash hangs? When the policy file is malformed?
- [ ] Structured logging with `tracing`: every proposal, every decision, every execution

---

## Milestone 5: Telegram Connector

**Goal:** The agent is reachable via Telegram. First real connector, first IPC boundary.

- [ ] Telegram bot using `teloxide` crate
- [ ] Connector runs as a separate process (or tokio task initially, extracted to process later)
- [ ] Messages flow: Telegram → connector → runtime → enforcement → tool → runtime → connector → Telegram
- [ ] Approval gates work via Telegram: bot sends "Agent wants to run `rm -rf /tmp/old`. Allow? (reply Y/N)"
- [ ] Media handling: images, files (basic — forward to model as base64 or file reference)

---

## Milestone 6: Credential Broker

**Goal:** The agent can reference credentials by name. Actual values are injected at execution time, outside the agent's context.

- [ ] Credential vault: encrypted file or system keychain (start simple — encrypted TOML file)
- [ ] Agent sees: `credential:stripe_api` (a name, not a value)
- [ ] HTTP tool: makes API calls with credential injection
- [ ] Enforcement layer validates credential scope: `stripe_api` with capability `["read"]` cannot be used for a POST to `/payments`
- [ ] Credential values never appear in: session history, model context, logs

---

## Milestone 7: Process-Isolated Plugins + IPC Protocol

**Goal:** Tools run as separate OS processes. The enforcement layer is the only bridge.

- [ ] Define IPC protocol: length-prefixed JSON over Unix domain sockets
- [ ] Plugin registration handshake: type, identity, capability declarations
- [ ] Extract bash tool to a separate process
- [ ] Runtime manages plugin lifecycle: spawn, connect, health check, restart
- [ ] A crashing tool plugin does not affect the runtime
- [ ] Write a second tool plugin (HTTP/API tool) to validate the protocol works for more than one tool

---

## Beyond MVP

These are real goals but not blocking the thesis proof:

- Audit log (append-only, structured, queryable)
- Discord connector
- Slack connector
- Policy hot-reload (file watch + re-evaluate)
- Multi-provider support (OpenAI, Ollama)
- Session persistence (JSONL)
- Wasm plugin support via Wasmtime (alternative to process isolation)
- Multi-agent routing (different policies per channel)
- Parameter-level policy rules (Stripe payment > $100 = Commit)
- Policy generation tooling (analyze a tool's actions, suggest tier classifications)

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
