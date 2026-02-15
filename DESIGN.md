# Secure Agent Runtime — Design Document

**Status:** Draft — Milestone 0 Complete  
**Author:** Kenn Williamson  
**Date:** February 2026

---

## 1. Project Philosophy

### 1.1 The Core Thesis

Every existing AI agent framework places its trust boundary at the prompt level. The agent is told "don't do dangerous things" via a system prompt, and the framework hopes the model complies. This is architecturally equivalent to shipping a chess engine with a copy of the FIDE rulebook and trusting it to never make an illegal move. It doesn't work — not because the models are malicious, but because they optimize for task completion, not rule compliance. A model will acknowledge a rule, agree with the rule, and violate the rule in the same response because the rule is an impediment to the goal, not a constraint on the system.

This project takes a fundamentally different approach: **the agent runtime makes rule violation structurally impossible, regardless of what the model intends, what content it has ingested, or what instructions it has been given.** The enforcement layer is deterministic, non-negotiable, and invisible to the agent. The agent doesn't know what it can't do. It simply experiences rejection when it proposes an illegal action — the same way a chess board rejects an illegal move without explaining the rule.

This is not a guardrail. Guardrails are suggestions. This is a wall.

### 1.2 Why the Enforcement Layer Must Be Separate

LLMs are stochastic systems — bounded but unpredictable in individual outputs. Every framework that places safety guardrails inside the agent is attempting to make the stochastic thing also be the deterministic safety system. That is a category error. You cannot eliminate variance from a stochastic system. You can only bound it.

**The model is stochastic, and that's fine. The enforcement layer is deterministic, and that's non-negotiable.** The model proposes freely. Every proposal passes through a deterministic evaluation layer that the model cannot influence. The variance is bounded. The bounds are absolute.

For the deeper philosophical foundation — why stochastic systems require structural bounds rather than behavioral guidance, and why this maps to centuries-old frameworks for reasoning about contingency within order — see [Providence in the Probabilistic](https://kennwilliamson.org/blog/providence-in-the-probabilistic-faith-and-non-deterministic-systems).

### 1.3 Intelligence Augmentation as Architecture

This project is a concrete implementation of the Intelligence Augmentation (IA) framework — the principle that AI's inability to assess its own knowledge gaps makes human validation architecturally necessary. In this system:

- **The human** defines the capability policy. What the agent can observe, what it can act on, what requires human approval. The human is the authority.
- **The model** proposes actions. It reasons, plans, and generates tool invocations. The model is the intelligence.
- **The runtime** enforces the boundary. Every proposed action passes through a deterministic evaluation layer that the model cannot influence, inspect, or circumvent. The runtime is the law.

These three roles are separated by design. The model never sees the policy. The runtime never reasons about intent. The human never manually validates routine actions. Each component does what it is structurally suited to do, and nothing more.

### 1.4 Why Rust

The choice of Rust is not incidental — it is load-bearing for the security thesis.

In JavaScript/TypeScript (the language of OpenClaw, Pi, and most agent frameworks), the enforcement layer and the agent loop would share a runtime. JavaScript is prototype-based and radically mutable. Any code in the same V8 process can monkey-patch built-in objects, override methods, and pollute prototypes. TypeScript's type system compiles away entirely at runtime, leaving no structural guarantees. A "capability token" in TypeScript is a plain object that any code in the same heap can fabricate.

In Rust, a capability token is a struct with a private constructor. The only way to obtain one is through the function that the operator's configuration invokes at startup. No code in the agent runtime can construct, copy, or forge a capability it was not granted, because the compiler will not permit it. This guarantee survives compilation. It is not a convention. It is a structural property of the binary.

Additionally, Rust's ownership model prevents the shared mutable state bugs that lead to privilege escalation in long-running daemons. Its lack of a garbage collector provides predictable latency for an always-on assistant. Its compilation to a single static binary simplifies deployment. And the Rust brand carries implicit credibility on safety claims in a way that matters for adoption of a security-focused project.

### 1.5 Minimalism as Principle

Pi's core insight is correct: frontier models have been trained extensively on coding tasks and inherently understand what a coding agent is. A massive system prompt with specialized tools adds tokens without adding capability. If the agent needs ripgrep, it can run `rg` via bash. The model is smart enough.

This project preserves that minimalism at the agent layer. The agent loop is simple. The system prompt is small. The tools are few. All the complexity lives in the enforcement layer underneath, where it belongs — invisible to the agent, transparent to the operator, and deterministic in behavior.

### 1.6 Build for Yourself First

This project is built as a personal tool before it is published as an open source project. The development philosophy is: build what you need, use it daily, refine it through real use, and only publish when it is something you would genuinely recommend to another person. Feature surface expands in response to actual need, not speculative use cases or community requests. If someone else builds something better before this is ready to publish, that's fine — the goal is a tool that works, not a GitHub star count.

---

## 2. Threat Model

### 2.1 What We Are Not Protecting Against

- **Inbound message authentication.** If you are the only person talking to your agent, the communication channel is already authenticated by the platform (Telegram bot tokens, Discord bot permissions, Slack app scoping). Hardening the user-to-agent message path solves a problem that doesn't exist for a personal assistant.
- **Model provider compromise.** If Anthropic's or OpenAI's API is returning malicious completions, you have bigger problems than this runtime can solve.
- **Physical access to the host machine.** Standard operational security applies. This is not a hardened enclave project.

### 2.2 What We Are Protecting Against

**Prompt injection via ingested content.** The agent reads a webpage, an email, a Slack message, a GitHub issue, a document — any of which could contain adversarial instructions. Those instructions now sit in the same context window as the agent's tool-calling capability. The injection doesn't come from the user. It comes from the world the agent interacts with. This is the primary and most realistic threat vector.

**Agent goal optimization overriding safety constraints.** Models optimize for task completion. When a safety rule conflicts with completing the user's request, the model will often find creative interpretations of the rule that technically comply while violating the intent. This is not malice — it is the expected behavior of a system trained to be helpful. The enforcement layer must be immune to creative interpretation because it does not interpret at all. It evaluates. Binary. Legal or illegal.

**Credential exfiltration.** If the agent has access to API keys, tokens, or passwords — even temporarily in its context window — any of the above attack vectors could cause it to leak those credentials. The agent should never see credential values. It should reference credentials by name, and a broker should inject actual values at execution time, outside the agent's context.

**Plugin/extension supply chain.** If plugins run in the same process as the core runtime, a malicious or buggy plugin can corrupt the enforcement layer's state. Plugins must be isolated at the process level at minimum.

**Scope creep via self-modification.** The Pi/OpenClaw philosophy encourages agents to extend themselves — writing new tools, installing new capabilities. In a security-focused runtime, self-modification must be subject to the same capability policy as any other action. The agent cannot grant itself new permissions by writing code that bypasses the enforcement layer.

### 2.3 The Two-Layer Defense

**Layer 1: OS-level sandboxing (Docker/containers).** This handles the coarse-grained isolation — filesystem boundaries, network namespaces, resource limits. Docker is good at this and there is no reason to reinvent it. If the agent runs `rm -rf /`, the container catches it. This layer is well-understood and battle-tested.

**Layer 2: Semantic capability enforcement (this project).** This handles the fine-grained, application-level decisions that Docker cannot express. Docker can say "this container can reach the network." It cannot say "this agent can call the Stripe API to check a balance but not initiate a transfer." It cannot say "this agent can post to Slack in #bot-testing but not #general." It cannot say "this agent can create a GitHub issue but not merge a pull request." Every meaningful action an agent takes against an external API requires semantic understanding of what that action means. That's this layer.

---

## 3. Architecture Overview

### 3.1 Why a Full Runtime (The Bypass Problem)

A natural question: why not build the enforcement layer as a standalone library or daemon that existing agent frameworks can call? The answer is structural.

If the enforcement layer is a library embedded in a TypeScript agent framework, the security guarantee depends on the framework cooperating. In a JavaScript runtime, prototype pollution can replace the function that calls the enforcement layer with a no-op. Any code in the same V8 heap can monkey-patch `fetch`, `child_process.exec`, or the IPC client. A prompt injection that gets the model to emit raw JS can reach tools directly. The enforcement layer becomes a lock on a door in a room with no walls.

If the enforcement layer is an external daemon that the agent calls before executing tools, the same problem applies at a different level: the agent must *choose* to call the daemon. That is a convention, not a constraint. It is the exact trust model this project rejects.

The only architecture where the security guarantees hold is one where the enforcement layer owns the entire execution path. The model's output enters the Rust binary as **data**, not code. The binary parses it into a struct (the model cannot influence how parsing works). The struct passes to the enforcement layer (the model cannot skip this step — it is the only code path). The enforcement layer evaluates against the policy (deterministic, no model involvement). If allowed, the runtime executes the tool (the model never touches the tool directly).

There is no bypass because there is no alternative path. The enforcement layer is not a gate the model walks through. It is the only road that exists.

This is why the project must be a full runtime — not a library, not a middleware layer, not a sidecar. The connectors, the agent loop, the tool execution, the credential injection — all of it must flow through the same Rust binary where the enforcement layer lives. Extracting any of these into an untrusted process that the enforcement layer doesn't control creates a path that bypasses the wall.

### 3.2 Component Separation

The system consists of four distinct component types, each with a clear role and a defined boundary:

**Core Runtime** — The heart of the system. Written in Rust. Contains the agent loop, the enforcement layer, the capability policy engine, the credential broker, and the audit log. This is the only component that is trusted. It runs as a single process with no dynamically loaded code from external sources. It is the chess board.

**Connectors** — Bridge external platforms (Telegram, Discord, Slack, etc.) to the runtime's internal message format. Each connector is a separate process that communicates with the core runtime over IPC. Connectors handle platform-specific authentication, message formatting, media handling, and presence. A connector can crash without affecting the core runtime or other connectors.

**Tools** — Capabilities the agent can invoke (bash execution, HTTP requests, file operations, API calls). Each tool is a separate process or a function within a sandboxed execution environment. Every tool invocation passes through the enforcement layer before execution. The agent proposes; the enforcement layer evaluates; only then does the tool execute.

**Providers** — LLM inference backends (Anthropic, OpenAI, local models via Ollama, etc.). Each provider is a separate process or module that handles API communication, streaming, token counting, and model-specific formatting. Provider selection can be configured per-agent or per-task.

### 3.3 The Enforcement Layer

The enforcement layer sits between the agent loop and tool execution. It is synchronous, deterministic, and stateless (relative to the current policy). It does not learn, adapt, or make probabilistic decisions. For every proposed tool invocation, it returns one of three results: **allow**, **reject**, or **escalate** (require human approval).

The enforcement layer evaluates proposals against a capability policy defined by the operator. The policy is loaded at startup from a human-authored configuration file. The agent cannot read, modify, or reason about the policy. The agent does not know the policy exists. When a proposal is rejected, the agent receives a generic "action not permitted" response — not an explanation of which rule was violated or why.

This is a deliberate design decision. The moment you explain *why* something was blocked, you give a pattern-matching system the information it needs to find the adjacent loophole. The enforcement layer is opaque to the agent by design.

### 3.4 The Capability Tiering Model

Every action an agent can take through a tool is classified into one of three tiers:

**Observe** — Read-only operations. Check an account balance. List calendar events. Read a Slack channel. Search a codebase. These operations do not modify state and carry minimal risk. The default for every new tool action is Observe.

**Act** — Reversible or low-consequence state changes. Draft an email (but don't send it). Create a calendar event. Post a message to a designated channel. Write a file to the agent's workspace. These operations modify state but are easily undone or carry limited blast radius.

**Commit** — Irreversible or high-consequence state changes. Send money. Merge code to a production branch. Delete data. Send a message to an important contact on the user's behalf. Publish content. These operations require explicit human approval by default.

The tiering is defined per-action within each tool's capability declaration. When a connector or tool plugin registers with the runtime, it declares each of its actions and the author's recommended tier classification. The operator can override any classification in their policy file. The enforcement layer respects the operator's overrides, falling back to the plugin author's recommendations for actions the operator hasn't explicitly configured.

**The critical default: new tools and new actions default to Observe only.** An agent that installs a new capability or a plugin that adds new actions cannot grant itself Act or Commit permissions. Only the human operator can promote actions above Observe.

### 3.5 Approval Gates

Any action classified at the Commit tier (or any action the operator has explicitly flagged for approval) pauses execution and sends a confirmation request to the operator through their preferred channel. The agent's message to the operator includes what it wants to do and why. The operator responds with approval or denial. The enforcement layer holds the action in a pending state until the human signal arrives or a configurable timeout expires (default: deny on timeout).

This is the IA framework made literal. The agent proposes. The human validates. The runtime enforces the decision.

### 3.6 The Credential Broker

The agent never sees credential values. API keys, tokens, passwords, and secrets are stored in a credential vault (system keychain, encrypted file, or external secret manager). The agent knows credentials exist by reference name only — e.g., it knows there is a credential called "stripe_api" with capabilities ["payments"]. When the agent proposes an API call that requires authentication, it references the credential by name. The enforcement layer validates that the proposed action is within the credential's declared capability scope. The credential broker then injects the actual credential value into the outgoing request, outside the agent's context. The agent receives the API response. The credential value never appears in the agent's context window, session history, or any log the agent can access.

This eliminates credential exfiltration as a threat vector. The agent cannot leak what it has never seen.

### 3.7 The Audit Log

Every interaction in the system is logged with full context:

- Every inbound message and its source
- Every agent proposal (tool invocations the agent wanted to make)
- Every enforcement decision (allow, reject, escalate) and the policy rule that triggered it
- Every tool execution and its result
- Every credential reference (but never credential values)
- Every human approval or denial

The audit log is append-only and written by the core runtime. Plugins cannot modify it. The agent cannot read it (it would leak policy information). The operator can review it at any time to understand exactly what happened, what was blocked, and why.

---

## 4. Plugin Architecture

### 4.1 Process Isolation

Every plugin (connector, tool, or provider) runs as a separate OS process. The core runtime never loads plugin code into its own address space. Plugins communicate with the runtime exclusively through IPC. This means:

- A crashing plugin cannot take down the runtime
- A malicious plugin cannot corrupt the enforcement layer's memory
- A buggy plugin cannot escalate its own privileges
- Plugin processes can be monitored, restarted, and resource-limited independently

### 4.2 IPC Protocol

Plugins communicate with the core runtime over a defined protocol. The transport is local IPC (Unix domain sockets for v1, with ZeroMQ or nng as potential upgrades if more sophisticated messaging patterns become necessary). The message format is length-prefixed JSON (simple, debuggable, universally supported across languages).

The protocol is the plugin interface. Any process that can open a Unix socket and serialize JSON can be a plugin. This makes the ecosystem language-agnostic by design. The first plugins will be written in Rust because it's convenient, but a Python developer or a Go developer can write a connector or tool plugin without touching Rust. They implement the protocol, not a Rust trait.

### 4.3 Plugin Lifecycle

1. **Startup.** The core runtime starts and reads its configuration. For each configured plugin, it either spawns the plugin process or connects to an already-running plugin process at a known socket path.
2. **Registration.** The plugin sends a registration message declaring its type (connector, tool, or provider), its identity, and its capability declarations (what actions it supports, with recommended tier classifications).
3. **Validation.** The runtime validates the registration against the operator's policy. If the plugin declares actions that the operator has not authorized, those actions are masked — the plugin is loaded but the unauthorized actions are never routed to it.
4. **Operation.** The plugin sends and receives messages according to its type. Connectors forward inbound messages from external platforms and receive outbound messages to send. Tools receive invocation requests and return results. Providers receive inference requests and stream responses.
5. **Shutdown.** The runtime sends a shutdown signal. The plugin performs cleanup and exits. If a plugin crashes without clean shutdown, the runtime detects the broken connection and can optionally restart it according to the operator's restart policy.

### 4.4 Language Interoperability

The protocol specification is the contract. It is documented as a JSON schema with behavioral expectations for each message type. A plugin implementor reads the spec, implements message handling in their language of choice, and connects.

Reference implementations of the plugin SDK will be provided in Rust (because the core runtime is Rust and the first plugins will be Rust). Community SDKs in other languages are welcome but not a project responsibility for v1. The protocol is simple enough that an SDK is a convenience, not a necessity — raw socket + JSON is sufficient.

Future considerations: WebAssembly plugins loaded via Wasmtime could provide tighter integration for plugins that want lower latency without sacrificing isolation. This would be a second plugin loading path alongside the process-based path. Wasm plugins would be subject to the same capability system and enforcement layer. This is not a v1 concern.

---

## 5. Open Questions

### 5.1 Architecture Questions

- **Agent self-extension policy.** Pi and OpenClaw celebrate the agent extending itself by writing code. In a security-focused runtime, how much self-extension should be permitted? Should the agent be able to write and register new tools at all? If so, should new tools always start at Observe-only with no way to self-promote? Is there a safe middle ground between "the agent can do anything" and "the agent is frozen"?

- **Multi-agent routing.** OpenClaw supports routing different channels to different agents with different configurations. Is this in scope for v1? If so, how do capability policies compose across agents? Can one agent escalate to another with higher privileges?

- **Session persistence format.** Pi uses JSONL session files that can span multiple model providers. What format should sessions use? How much session history should be retained? Should the enforcement layer's decisions be recorded in the session (visible to the agent in future turns) or only in the separate audit log?

- **Content/instruction separation.** Beyond tool execution enforcement, should the runtime attempt to separate "content the agent read" from "instructions the agent should follow"? This is the prompt injection problem at the context level rather than the execution level. It's a harder problem and may not be solvable deterministically. Is it in scope?

- **Failure modes.** When the enforcement layer cannot determine whether an action is legal (e.g., the policy is ambiguous, the action doesn't match any declared capability), should it default to deny or escalate? Deny-by-default is safer. Escalate-by-default is more usable. What's the right default, and should it be configurable?

### 5.2 Protocol Questions

- **Message schema design.** What does a registration message look like? What does a tool invocation proposal look like? What does a policy rejection response look like? What metadata is required on every message (timestamps, correlation IDs, source identity)?

- **Streaming.** LLM responses are streamed. Tool outputs may be streamed. How does the protocol handle streaming data across the IPC boundary? Is length-prefixed JSON sufficient, or does streaming require a different framing approach?

- **Binary data.** Images, files, audio — how are non-text payloads handled in the protocol? Inline as base64? Out-of-band via filesystem references? Both depending on size?

- **Error semantics.** What error categories exist? How does a plugin signal transient failure vs. permanent failure vs. rate limiting vs. authentication failure? How does the runtime distinguish between "the plugin crashed" and "the plugin chose to reject the request"?

### 5.3 Policy Questions

- **Policy language.** What format does the operator's capability policy use? TOML/YAML for simplicity? A dedicated DSL for expressiveness? How complex can policy rules get? Can rules reference context (time of day, message source, recent action history) or are they purely static?

- **Tier override granularity.** Can the operator override tiers at the tool level (all Stripe actions are Commit), the action level (Stripe create_payment is Commit but Stripe get_balance is Observe), or the parameter level (Stripe create_payment over $100 is Commit but under $100 is Act)?

- **Policy hot-reload.** Can the operator modify the policy while the runtime is running? If so, how are in-flight actions handled? Does a policy change affect currently pending approval gates?

- **Default policy generation.** When a new plugin registers with actions the policy doesn't mention, should the runtime auto-generate Observe-tier entries and notify the operator? Or should unknown actions be denied entirely until explicitly configured?

### 5.4 Operational Questions

- **Deployment model.** Single binary? Core runtime + plugin binaries? Docker compose for the full stack? What's the simplest path from "clone the repo" to "agent is running and connected to Telegram"?

- **Monitoring and health.** How does the operator know the system is healthy? What metrics are exposed? Is there a simple dashboard, or is it CLI/log-only for v1?

- **Upgrade path.** When the protocol changes, how are plugins migrated? Is protocol versioning in scope for v1, or is it acceptable to break compatibility during early development?

- **Resource management.** For a personal assistant running on modest hardware (a VPS, a Raspberry Pi, a home server), what are the resource expectations? How many concurrent plugin processes are reasonable? What's the memory baseline?

### 5.5 Ecosystem Questions

- **Naming.** The project needs a name. Something that evokes the enforcement layer concept — the substrate underneath, the board not the pieces, the law not the suggestion. Working ideas welcome.

- **Licensing.** Open source, but which license? MIT/Apache-2.0 for maximum adoption? AGPL to prevent closed-source forks? Something else?

- **Documentation strategy.** The protocol spec is the primary document. What else is needed for a first public release? Operator guide? Plugin author guide? Architecture overview? Threat model?

---

## 6. Non-Goals

To keep scope focused, the following are explicitly **not** goals for this project:

- **Competing with OpenClaw's feature surface.** No macOS menu bar app, no iOS/Android nodes, no voice wake, no canvas, no webchat UI. This is a runtime, not a product.
- **MCP compatibility.** MCP is a protocol designed for a different trust model. If MCP compatibility is needed, it can be bridged via a plugin, not baked into the core.
- **Multi-user support.** This is a personal assistant runtime. One operator, one agent (or a small number of agents under one operator's policy). Enterprise multi-tenant concerns are out of scope.
- **Model training or fine-tuning.** The runtime is model-agnostic. It sends prompts and receives completions. What happens inside the model is not its concern.
- **Replacing Docker/container sandboxing.** OS-level sandboxing is a solved problem. This project provides the semantic layer above it, not a replacement for it.

---

## 7. Competitive Landscape

*Research conducted February 2026. This section documents the state of the field to validate the project's novelty and identify components worth studying or integrating.*

### 7.1 The Core Finding

No existing project combines all four pillars of this design into a single, unified runtime:

1. **Deterministic tool-call enforcement** with compiler-enforced unforgeable capability tokens
2. **Observe/Act/Commit tiered permissions** as a first-class concept
3. **Credential brokering** where the agent never sees secret values
4. **Agent-opaque policy** where the agent cannot inspect, reason about, or influence the enforcement rules

The building blocks exist across 15+ projects. Nobody has composed them into a unified runtime. The market is split between infrastructure companies building platform-locked services, authorization companies extending existing products, security startups building detection/monitoring platforms, and open-source tools solving individual slices.

### 7.2 Closest Competitors

**IronClaw** (github.com/nearai/ironclaw) — The nearest existing project. A Rust reimplementation of OpenClaw with WASM sandboxing and capability-based permissions. Supports Telegram and Slack. Uses WASI capabilities for tool isolation. However: no Observe/Act/Commit tiering, no agent-opaque policies, no credential brokering, and WASM capabilities are runtime grants rather than compile-time unforgeable tokens. IronClaw sandboxes tools but does not own the full execution path in the way this design requires. Study its connector architecture and WASM integration.

**Microsoft Wassette** (github.com/microsoft/wassette) — A Rust-based security runtime for WebAssembly MCP tools. Deny-by-default capability system via Wasmtime. Each tool starts with zero privileges and must be explicitly granted access. Production-ready since August 2025. However: it sandboxes individual MCP tools, not the agent itself. No tiered permissions, no credential brokering, no approval gates. It solves one sub-problem (tool isolation) that this design solves as part of a larger system. Study the WASI capability granting model for potential future Wasm plugin support.

**Microsoft FIDES** (github.com/microsoft/fides) — A research agent planner using information-flow control (IFC) to enforce security policies deterministically. Attaches confidentiality/integrity labels to all data. Achieves 100% attack prevention on AgentDojo benchmarks. Published as academic paper (arXiv:2505.23643). However: it is a planner architecture, not a deployable runtime. No credential brokering, no tiered permissions. Validates the thesis that deterministic enforcement works; does not ship a usable system.

**AWS Bedrock AgentCore Policy** — Cedar-based deterministic policy enforcement on all agent traffic through AgentCore Gateways. Preview since December 2025. Backed by AWS. However: vendor-locked to the AWS Bedrock ecosystem. Not self-hostable. Not open source. Validates that the industry recognizes the need for deterministic policy enforcement, but is the wrong deployment model for a personal assistant.

**StrongDM Leash** (github.com/strongdm/leash) — Open-source (Apache 2.0) host-level enforcement using eBPF and LSM hooks. Uses Cedar policies. Intercepts MCP transport at the kernel level. Less than 1% performance overhead. However: OS-level enforcement, not semantic tool-call enforcement. Cannot distinguish "Stripe check balance" from "Stripe initiate transfer" because it operates below the application layer. Complementary to this project, not competitive.

### 7.3 Component Solutions (Partial Overlaps)

**Tenuo** (github.com/tenuo-ai/tenuo) — Cryptographically attenuated capability warrants for AI agents. Rust core with Python bindings. ~27μs verification. Sub-tasks get narrower permissions via attenuation. However: a library for capability tokens, not a runtime. No process isolation, IPC, connectors, or credential brokering. v0.1 Beta. Study the warrant attenuation model for potential integration.

**Cerbos** (cerbos.dev) — Open-source stateless authorization engine. Sub-millisecond allow/deny decisions from declarative YAML policies. Active MCP integration work. However: a Policy Decision Point, not an enforcement runtime. Requires the calling application to actually enforce the decision — which is the trust problem this project solves at the architectural level. Study the policy language design.

**Aembit** (aembit.io) — IAM for agentic AI with MCP Identity Gateway. Assigns cryptographic identities to agents. Issues ephemeral credentials scoped to tasks. Agent never sees credential values. However: commercial SaaS, not self-hostable. Validates the credential brokering pattern this design implements.

**Auth0 Token Vault** — OAuth 2.0 Token Exchange (RFC 8693) for AI agents. Agents use access tokens without handling refresh tokens. Early Access. However: commercial, not self-hostable. Supports Google, Slack, GitHub, Microsoft Graph.

**Progent** (arxiv.org/abs/2504.11703) — UC Berkeley research. JSON-based DSL for fine-grained tool privilege policies. Deterministic enforcement with provable guarantees. Agent-opaque. Reduces attack success to 0% while preserving utility. However: Python research prototype, not a deployable runtime. No credential brokering, no process isolation. The strongest academic validation that this project's architectural approach is correct.

**MiniScope** (arxiv.org/abs/2512.11147) — UC Berkeley Sky Computing Lab. Automatically constructs permission hierarchies for tool-calling agents. Mechanical enforcement, 1-6% latency overhead. However: research prototype. Study the automatic hierarchy construction for potential policy generation tooling.

### 7.4 Framework Security Models (Why a New Runtime Is Needed)

Existing agent frameworks implement security as features bolted onto fundamentally unsecured architectures:

- **Claude Code** has the strongest structural enforcement of any existing framework — PreToolUse hooks can approve/deny/modify, permission rules are deterministic (deny > ask > allow), sandboxing reduced permission prompts by 84%. But it is specific to Claude Code, not a general-purpose runtime.
- **OpenAI Agents SDK** has a `needsApproval` structural gate and HITL state serialization, but guardrails are LLM-based (probabilistic). No unified capability system.
- **LangGraph** has graph-based deterministic execution and interrupt-based approval, with emerging two-phase commit patterns. But no built-in tiered capabilities — developers must build their own.
- **CrewAI** has post-hoc output validation only. No tool-level permissions. Weakest security model of the major frameworks.
- **AutoGen** has Docker sandboxing for code execution only. If a tool is registered, the agent can call it. No per-tool permissions.
- **MCP** has OAuth 2.1 authentication but **no native tool-level permission model**. Authentication tells you who is calling; it does not constrain what they can do. The largest gap in the MCP ecosystem.

None of these frameworks can be secured after the fact because their enforcement layers share a runtime with the agent. A TypeScript framework's "capability token" is a plain object any code in the same heap can fabricate. The only way to make enforcement structural is to own the entire execution path — which requires a purpose-built runtime.

### 7.5 The Bypass Problem (Why a Library Won't Work)

A key architectural decision validated through analysis: the enforcement layer cannot be a standalone library or daemon that existing frameworks call. If the enforcement layer is external, the framework must choose to call it. In a TypeScript runtime, prototype pollution can replace the enforcement call with a no-op. In any runtime where the agent has code execution capability, a prompt injection can reach tools directly, bypassing the enforcement layer entirely.

The enforcement layer's guarantees are only meaningful if it controls the entire execution path from model output to tool execution. The model's output enters the Rust binary as data, not code. The binary parses it, evaluates it against the policy, and either executes or rejects. There is no bypass because there is no alternative path. This is why the project must be a full runtime, not a library.

### 7.6 Industry Validation

The problem this project solves is increasingly recognized as critical:

- **63% of organizations cannot enforce purpose limitations** on their AI agents (industry survey, 2025)
- **Over 40% of agentic AI projects expected to be cancelled by 2027** due to inadequate risk controls (Gartner)
- Cisco flagged OpenClaw specifically as a "security nightmare" due to leaked credentials and lack of sandboxing
- MIT Technology Review published "Is a Secure AI Assistant Possible?" (February 2026) examining the structural gap
- OWASP published an AI Agent Security Cheat Sheet recommending deny-by-default allowlists, runtime authorization, and credential injection at host boundary — the exact architecture this project implements
- Academic papers ("Systems Security Foundations for Agentic Computing," ePrint 2025/2173) argue that model-level safety is insufficient and system-level enforcement is required

---

## 8. Reference Material

### 8.1 Projects to Study

**Direct Competitors and Adjacent Systems:**
- **IronClaw** (github.com/nearai/ironclaw) — Rust OpenClaw rewrite with WASM sandboxing. Study connector architecture, capability grants, deployment model.
- **Wassette** (github.com/microsoft/wassette) — Rust WASM tool runtime. Study the WASI capability model and policy.yaml format for potential future Wasm plugin support.
- **FIDES** (github.com/microsoft/fides) — Information-flow control for agents. Study the IFC label system and how it achieves 100% attack prevention.
- **Tenuo** (github.com/tenuo-ai/tenuo) — Cryptographic capability warrants. Study the attenuation model for sub-task permission narrowing.
- **Leash** (github.com/strongdm/leash) — eBPF agent enforcement. Study Cedar policy language integration.

**Existing Frameworks (Study Their Gaps):**
- **OpenClaw** — The current state of the art for personal AI assistants. Study its architecture, plugin system, channel integrations, and its known security vulnerabilities (Cisco analysis).
- **Claude Code** — Study the PreToolUse hooks, deny/ask/allow rule precedence, and sandboxing architecture. The strongest existing enforcement model, but framework-specific.
- **LangGraph** — Study the interrupt/checkpoint/approve cycle and emerging two-phase commit patterns.

**Policy and Authorization:**
- **Cerbos** (cerbos.dev) — Study the YAML policy language design and sub-millisecond evaluation architecture.
- **Cedar** (cedarpolicy.com) — AWS's open-source policy language used by AgentCore and Leash. Study the principal/action/resource model.
- **Progent** (arxiv.org/abs/2504.11703) — Study the JSON policy DSL and provable security guarantees.
- **MiniScope** (arxiv.org/abs/2512.11147) — Study automatic permission hierarchy construction.

**Credential Brokering:**
- **Aembit** (aembit.io) — Study the MCP Identity Gateway and ephemeral credential model.
- **Auth0 Token Vault** — Study OAuth 2.0 Token Exchange (RFC 8693) for delegated agent access.

**Rust Ecosystem:**
- **cap-std** (github.com/bytecodealliance/cap-std) — Capability-oriented version of Rust's standard library. Study for potential use in the core runtime's filesystem/network access.
- **Soma** (trysoma.ai) — Rust/TypeScript agent runtime. Study the single-binary deployment and governance plane.
- **Pica** (picahq/pica) — Rust agentic infrastructure. Study the security proxy for credentials.

### 8.2 Key Concepts

- **Capability-based security** — The principle that access to resources is controlled by possession of unforgeable tokens rather than by identity checks against access control lists. The runtime grants capabilities; the agent exercises them; the compiler prevents forgery.
- **Principle of least privilege** — Every component has the minimum access necessary for its function. New plugins default to Observe. New actions default to Observe. Escalation is always explicit and human-authorized.
- **Deny by default** — If the policy doesn't explicitly permit an action, the action is denied. The system does not guess, infer, or assume. Silence in the policy means no.
- **Opacity to the agent** — The agent does not know the policy exists. It does not receive explanations for rejections. It does not know what capabilities other plugins have. It experiences the enforcement layer as a fact of its environment, not as a rule it could reason about circumventing.
- **Information-flow control** — Tracking data provenance through the system to ensure untrusted content (ingested web pages, emails, messages) cannot influence enforcement decisions. Studied by Microsoft FIDES. Potentially relevant for content/instruction separation (see Open Questions 5.1).

### 8.3 Academic Foundations

- **"Securing AI Agents with Information Flow Control"** (Microsoft Research, arXiv:2505.23643) — Theoretical foundation for deterministic agent enforcement via IFC labels.
- **"Progent: Programmable Privilege Control for LLM Agents"** (UC Berkeley, arXiv:2504.11703) — Proves deterministic tool-call enforcement can achieve 0% attack success while preserving utility.
- **"MiniScope: A Least Privilege Framework for Authorizing Tool Calling Agents"** (UC Berkeley, arXiv:2512.11147) — Automatic construction of permission hierarchies with 1-6% overhead.
- **"Systems Security Foundations for Agentic Computing"** (ePrint 2025/2173) — Survey of 11 real attacks on agentic systems; argues system-level enforcement is necessary.
- **"Zero-Cost Capabilities: Retrofitting Effect Safety in Rust"** (UC Davis) — Demonstrates compile-time capability enforcement in Rust's type system.

---

*This document will evolve as design decisions are made and validated through implementation. Open questions will be resolved and moved into the architecture specification. The document is intended to be a living reference, not a frozen plan.*