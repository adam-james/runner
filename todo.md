# TODO

Direction: the agent runtime you can safely hand untrusted tools and untrusted
prompts. Every tool and provider is a WASM component that never sees an API key
and can only reach what its config allows. Make that visible, enforceable, and
the reason to pick runner.

Focus: background agents that are secure and auditable. Nobody is watching a
background run, so what it may do is fixed before it starts, anything beyond
that waits for an approval that defaults to deny, and the audit log is how you
find out what happened. Agents are the unit of capability: no separate grant
system; for narrower access, define a narrower agent.

## 1. Make capabilities visible

- ~~show what a tool or provider can do before it's enabled~~ done: `runner tools inspect` / `providers inspect`, and per-tool summaries in `agents list` and session banners
- ~~wasm inspector - turn to s-expression and have llm read it~~ done: `runner tools wat` and the host `inspect_component` tool
  - done: include each tool's `describe()` text as a bounded, quoted, untrusted claim; execute it in a separate sandbox without network, credentials or files
- security audit logging alongside debug logs, surfaced as `runner runs audit RUN_ID` and `runner sessions audit SESSION_ID`
  - done: host-made, versioned, sequenced, metadata-only records for sandboxes, tool/model calls, outbound requests, limit refusals and traps; SQLite storage; gap detection; fail closed when a turn's log is full (see README "Audit log")
  - next: alerts on denials/refusals, retention beyond the run, a separate audit-read token, recording per-file access if the host ever mediates it
  - threat model: malicious guest components; trust the runner and host to enforce policy and record activity
  - add structured, versioned events with event IDs, sequence numbers, actor/run/session identity, component version, action, policy decision, and outcome
  - record activity at host-controlled boundaries so guests cannot forge, suppress, or bypass records; bound and escape guest-controlled metadata
  - use durable host-owned storage inaccessible to guests; define behavior when storage fails or guests flood the audit stream, including which actions must block
  - cover tool execution, allowed and denied network/filesystem access, capability violations, and guest traps or resource-limit termination
  - define retention, audit access controls, alerts, and detection of missing events or collector outages
  - keep audit records metadata-only: omit prompts, credentials, tool payloads, and raw errors
- ~~add an agent that can audit security~~ done: `security_auditor` in runner.toml
- ~~add an audit agent that reviews what runs actually did, from their audit trails~~ done: `run_auditor` in runner.toml and the host `audit_trail` tool (`read_audit_trails`)
  - next: run it automatically when a background run's summary flags something (see section 2)
- ~~docs: say loudly that keys never enter the sandbox~~ done: README "API keys never enter the sandbox"

## 2. Background agents you can trust

In order; each step ships something usable on its own.

1. agent versions - an audit trail must show what a run was allowed, not just what it did, even after runner.toml changes
   - today records keep only the agent's name, component names and digests, and a count of allowed hosts: which hosts, which headers went where, and the agent's tools, provider, model, delegates and flags are lost when the config changes
   - at the start of each run, snapshot the resolved agent definition: provider and model, each tool's capabilities (allowed hosts, header names and their hosts but never values, dir, limits, component digest; `inspect::Capabilities` has the shape), delegates, flags, and a hash of the system prompt
   - its hash is the version: store each distinct snapshot once in an `agent_versions` table, and record the version on the run and in `turn.started`
   - `runs audit` and `run_auditor` compare what a run did with what its version allowed
   - sessions stay locked to the agent name; each run records its version, so a config change mid-session shows in the trail
2. schedules pinned to an agent version - one-off and cron schedules (see docs/service.md); each occurrence is an ordinary run in a fresh session
   - a schedule names an agent and pins the version it was created with; if the version changes (config edit or swapped binary), the schedule pauses until re-approved, so an edit made for chat can't quietly widen what an unattended agent can do
   - read-only background agents (e.g. a morning brief) are safe and useful at this step
3. after-run summaries, alerts and notifications - the audit log becomes the primary interface
   - host-made summary of every background run from its audit records, no model involved: hosts reached, denials, errors, trail gaps, pending approvals
   - alerts on denials, trail gaps, and a host the schedule has never reached before
   - `run_auditor` only when the summary flags something
   - audit retention beyond the run, and for the agent versions runs reference, since background runs are reviewed later
   - notifications: tell the user in the foreground when a background task finishes, fails or raises an alert
     - the daemon keeps durable notification records, so nothing's lost while no client is connected or the machine sleeps; unread ones wait until seen
     - delivered as desktop notifications (macOS Notification Center, freedesktop on Linux) and listed in the TUI; opening one shows the run
     - host-written text only (task, outcome, duration, the summary's flags), never the model's reply: a prompt injection could otherwise put a convincing fake message or link in a trusted-looking system notification
     - per schedule: notify on every finish, only on failures and alerts, or never
     - approval requests (step 4) use the same channel
4. asynchronous approval checkpoints - what lets a background agent act
   - tools declare side-effecting actions (send email, book, `git push`) in their WIT interface
   - the run pauses durably (survives restarts) and the action waits in a queue, approved or denied from CLI/TUI/remote
   - approvals expire, and an expired approval is a denial; never let a missed approval turn into a yes
   - standing approvals on an agent for routine, narrow actions (e.g. post to this one channel); they're part of its version
   - record every approve, deny and expiry in the audit log
5. later: memory across a schedule's runs - the main way a prompt injection persists, so it waits for the above
   - taint tracking of history from sensitive tools, instead of a blanket lock

- ~~lock each session to one agent for its whole life~~ done: required agent at creation, mismatch errors, migration, and TUI switches start new sessions
  - later: explicit handoff to a new session seeded with prompts and replies only (no tool results), behind a confirmation
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
