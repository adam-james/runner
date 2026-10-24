# TODO

Direction: the agent runtime you can safely hand untrusted tools and untrusted
prompts. Every tool and provider is a WASM component that never sees an API key
and can only reach what its config allows. Make that visible, enforceable, and
the reason to pick runner.

## 1. Make capabilities visible

- show what a tool or provider can do before it's enabled: component imports plus config `allowed_hosts`, `dir`/`dir_writable`, injected headers
  - e.g. "`web_search` can reach api.tavily.com, gets an Authorization header there, has no filesystem access"
  - `runner tools inspect NAME`, and in `agents list` / session banners
- ~~wasm inspector - turn to s-expression and have llm read it~~ done: `runner tools wat` and the host `inspect_component` tool
  - next: include each tool's `describe()` text so the auditor sees its stated purpose, not just its name
- security audit logging alongside debug logs, surfaced as `runner runs audit RUN_ID`
  - threat model: malicious guest components; trust the runner and host to enforce policy and record activity
  - add structured, versioned events with event IDs, sequence numbers, actor/run/session identity, component version, action, policy decision, and outcome
  - record activity at host-controlled boundaries so guests cannot forge, suppress, or bypass records; bound and escape guest-controlled metadata
  - use durable host-owned storage inaccessible to guests; define behavior when storage fails or guests flood the audit stream, including which actions must block
  - cover tool execution, allowed and denied network/filesystem access, capability violations, and guest traps or resource-limit termination
  - define retention, audit access controls, alerts, and detection of missing events or collector outages
  - keep audit records metadata-only: omit prompts, credentials, tool payloads, and raw errors
- ~~add an agent that can audit security~~ done: `security_auditor` in runner.toml
- docs: say loudly that keys never enter the sandbox

## 2. Enforce capabilities at the moment they matter

- lock each session to one agent for its whole life
  - why: history (including tool results) carries across a session's runs, so switching agents combines capabilities no single agent has, e.g. `reader` reads a secret, then `researcher` can leak it via `web_search`; it can also hand history to a different provider
  - runs are already locked to their agent (set at submit, never updated); the session is the gap
  - record the agent on the session at creation or first run (schema change); reject runs naming a different agent
  - switching agents means a new session: TUI Ctrl-A opens one; `chat`/`run --session X --agent Y` with a mismatch errors with a hint
  - existing mixed-agent sessions lock to their most recent agent
  - later, if missed: explicit handoff to a new session seeded with prompts and replies only (no tool results), behind a confirmation
  - longer term: taint tracking of history from sensitive tools instead of a blanket lock
- dynamic tool config and permissioning
  - per-run grants, e.g. "this run may write to `~/.runner/files` until it ends"; record every decision in the audit log
- approval checkpoints: tools declare side-effecting actions (send email, book, `git push`); the run pauses durably until approved from CLI/TUI/remote
- dynamic agent definitions for delegation
  - create temporary specialists or override static presets with task-specific instructions, models, and tool selections
  - restrict choices to host-approved providers/models, tools, and delegation targets; a guest cannot expand its delegation authority
  - define lifetime, ownership, quotas, persistence/restart behavior, and whether definitions can be reused across turns
  - retain supervised cancellation, isolated sessions, depth/fan-out limits, and metadata-only activity logs
  - add runnable examples for creating a specialist and collecting its result

## 3. Use other people's tools safely

- run existing MCP servers under runner's network/filesystem policy (sandboxed stdio, or compiled to components) - biggest adoption lever
- component registry: signed, content-addressed components with capability manifests
  - `runner tools add <source>` shows a permission prompt before install
  - settle the manifest format early; it shapes the WIT interfaces
- a tool to write tools: agent writes a tool, it compiles to a component, runs with no network/filesystem by default, and goes live only after its permissions are approved

## 4. Background work you can trust

- schedule tasks - see docs/service.md for one-off and recurring task semantics
  - per-schedule permissions (e.g. morning brief may reach these hosts and nothing else)
  - after-run summary of what it touched
- agent to agent communication
- memory

## 5. Portability and determinism

- record/replay: capture a run's provider responses and HTTP traffic and replay exactly, for debugging, evals, and regression tests (builds on the deterministic test harness)
- portable agents: bundle prompt, tools, and permissions into one artifact that runs with the same guarantees anywhere

## Housekeeping

- move runner code to host
- OAuth/login
- encrypted store for API keys - nice to have, not a differentiator
- prune bundled demo tools (sports, weather) in favor of a few genuinely useful ones (files, web, python, git, http fetch)
- are there APIs for booking flights and other useful things like that? (pairs with approval checkpoints)
- wasm shell
- switch build target to wasm32-wasip3 once Rust 1.100 ships it on stable (~late Nov 2026)
- migrate tools to WASI 0.3 http (off wstd) - wait for a p3 wstd or a need for streaming tool output / parallel tool calls

## Service foundation

Implemented: local daemon and CLI, durable run queue and event replay, cancellation,
deadlines, session serialization, and restart recovery. Next milestones and API:
[docs/service.md](docs/service.md).
