# TODO

Direction: the agent runtime you can safely hand untrusted tools and untrusted
prompts. Every tool and provider is a WASM component that never sees an API key
and can only reach what its config allows. Make that visible, enforceable, and
the reason to pick runner.

Focus: background agents that are secure and auditable. Nobody is watching a
background run, so what it may do is fixed before it starts, and the audit log
is how you find out what happened. Agents are the unit of capability: no separate grant
system; for narrower access, define a narrower agent.

## 1. Make capabilities visible

- a separate audit-read token
- audit log retention: how long records are kept, and how they're pruned

## 2. Background agents you can trust

- deferred: dynamic agent definitions for delegation - an agent that creates specialists at runtime is the opposite of auditable; static delegation between config-defined agents fits the focus. Revisit with these constraints:
  - restrict choices to host-approved providers/models, tools, and delegation targets; a guest cannot expand its delegation authority
  - define lifetime, ownership, quotas, persistence/restart behavior, and whether definitions can be reused across turns
- deferred: agent to agent communication

## 3. Use other people's tools safely

- run existing MCP servers under runner's network/filesystem policy (sandboxed stdio, or compiled to components) - biggest adoption lever
- component registry: signed, content-addressed components with capability manifests
  - `runner tools add <source>` shows a permission prompt before install
  - settle the manifest format early; it shapes the WIT interfaces
- a tool to write tools: agent writes a tool, it compiles to a component, runs with no network/filesystem by default, and goes live only after its permissions are approved

## 4. Portability and determinism

- record/replay (also a strong audit tool for background runs): capture a run's provider responses and HTTP traffic and replay exactly, for debugging, evals, and regression tests (builds on the deterministic test harness)
- portable agents: bundle prompt, tools, and permissions into one artifact that runs with the same guarantees anywhere

## Housekeeping

- move runner code to host
- report a `dir` that fails to mount: a missing read-only `dir` isn't created (on purpose: an empty read-only mount hides a typo), but the reason reaches no one: `sandbox.create` ends `interrupted` and the client sees "agent execution failed"
  - check each component's `dir` when the daemon starts and refuse to start with the reason, like a missing env var
  - log why a component failed to load on the daemon's stderr (host-made errors: paths, not guest output)
  - record the reason in the trail, e.g. `sandbox.create` with `outcome: "dir_missing"`, instead of `interrupted`
- desktop notifications on Linux: enable `notify-rust` there too (pure-Rust zbus to the freedesktop D-Bus interface, no subprocess); escape `&`, `<`, `>` in the body, since servers may render markup; test it on a real desktop session; notifications stored meanwhile were never marked posted, so mark them posted when it ships rather than posting them all as one
- OAuth/login
- encrypted store for API keys - nice to have, not a differentiator
- prune bundled demo tools (sports, weather) in favor of a few genuinely useful ones (files, web, python, git, http fetch)
- are there APIs for booking flights and other useful things like that?- wasm shell
- switch build target to wasm32-wasip3 once Rust 1.100 ships it on stable (~late Nov 2026)
- migrate tools to WASI 0.3 http (off wstd) - wait for a p3 wstd or a need for streaming tool output / parallel tool calls

## Service foundation

Implemented: local daemon and CLI, durable run queue and event replay, cancellation,
deadlines, session serialization, and restart recovery. Next milestones and API:
[docs/service.md](docs/service.md).
