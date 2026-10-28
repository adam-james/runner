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
  - next: retention beyond the run, a separate audit-read token (alerts are done, and capability linking replaces recording per-file access; see section 2, step 3)
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

1. ~~agent versions~~ done: each run's resolved agent definition is snapshotted (never header values; system prompt as a hash), stored once per version in `agent_versions`, linked from the run and named in `turn.started`; `runner agents version ID`, and `run_auditor` reviews runs against their versions
   - next: `runs audit` could show where a run went beyond its version (it can't today, since the host enforces the version, but it would catch a host bug)
2. ~~schedules pinned to an agent version~~ done: one-off and cron schedules (see docs/service.md); each occurrence is an ordinary run in a fresh session
   - done: one-off schedules (`runner schedules`, `/v1/schedules`), pinned to the versions of their agent and every agent it can delegate to; checked at startup, when due, and when the run starts; a changed agent pauses the schedule until `approve`, which reports what changed
   - done: recurring cron schedules with time zones (`--cron`), parsed and stepped by `croner` with its Vixie cron DST rules, on `chrono` + `chrono-tz` in place of `jiff`; an occurrence more than a minute late, or whose previous run hasn't finished, is skipped and recorded; a refused run keeps the schedule's next occurrence
     - trade-off: `chrono-tz` builds the zone database into the binary, so zone rule updates need a rebuild
   - later: `approve` could show the changed fields before re-pinning and ask to confirm, rather than reporting them after
   - read-only background agents (e.g. a morning brief) are safe and useful at this step
3. after-run summaries, alerts and notifications - the audit log becomes the primary interface
   - ~~host-made summary of every background run from its audit records, no model involved~~ done: `runner runs summary` / `GET /v1/runs/{id}/summary`, over the run's whole delegation tree: hosts reached, denials, errors, withheld credentials, trail problems; leaves out the run's error text
     - next: pending approvals, once step 4 exists
     - next: store each background run's summary when it finishes, for notifications and for when audit retention prunes its records
   - ~~alerts on denials, trail gaps, and a host the schedule has never reached before~~ done: a summary's `alerts`, computed with it; a schedule's first run sets its host baseline, and trail alerts wait until the whole tree has ended
     - next: keep a table of each schedule's known hosts if comparing against every earlier run's audit records gets slow
   - ~~link only the WASI a component needs~~ done (part 1): `host/src/linking.rs` links `wasi:http` (0.2 and 0.3, through the allow-list hook), `wasi:io`, `wasi:clocks`, `wasi:random`, stdin/stdout/stderr (empty and discarded; unlinked, a panic's message would trap and read as a denial) and `wasi:filesystem` only with a `dir`; everything else a component imports gets a host stub: a call that can fail is refused with `access-denied` (or `not-permitted`), a list or option query answers empty and isn't recorded (so reading an empty environment isn't an alert), and anything else traps; refusals and traps are recorded as `capability.denied`, so they're denials in summaries and raise alerts; `tools inspect` and `tools wat` list what isn't linked
     - ~~wrap only what stays granted~~ done (part 2): `host/src/filesystem.rs` wraps wasmtime's `wasi:filesystem` for a component with a `dir`; a `not-permitted` result (an escape via `..` or a symlink, or a write under a read-only dir) is recorded as `filesystem.denied` with the operation, never the path; other errors (a missing file, an OS `access` error) aren't recorded
     - next: investigate a `dir` that fails to mount: a read_file tool with `dir` set fails to load on macOS (on main too); whether the directory exists isn't confirmed yet; `sandbox.create` ends `interrupted` with no `dir` field, so either the existence check or opening the dir failed, and the reason is logged nowhere (the client sees "agent execution failed")
       - check each component's `dir` when the daemon starts and refuse to start with the reason, like a missing env var
       - log why a component failed to load on the daemon's stderr (host-made errors: paths, not guest output)
       - record the reason in the trail, e.g. `sandbox.create` with `outcome: "dir_missing"`, instead of `interrupted`
     - next (part 3): show each component's grants in every run's summary, from its agent version: sandboxes are reused across a session's turns, so only the first run's trail has `sandbox.create`, and with no `dir` a file attempt never reaches the host at all
     - ~~test that a component's environment is empty~~ done: `wasi:cli/environment` isn't linked, and the fixture probe checks it reads no env vars
   - notifications: tell the user in the foreground when a background task finishes, fails or raises an alert (before `run_auditor`, so alerts reach someone)
     - ~~durable notification records, host-written only, with a per-schedule level~~ done: the daemon's notifier checks every top-level run once its tree has ended, scheduled or not, assuming nobody watched; level `all|problems|never` (problems: the run or a delegated run didn't complete, or an alert), per schedule (`schedules add --notify`) or per run (`runner run --notify`), default `problems`; `runner notifications`, `/v1/notifications`; status, duration, alerts and schedule status, never the model's reply or the run's error
     - after capability linking: delivered as desktop notifications (macOS Notification Center, freedesktop on Linux) and listed in the TUI; opening one shows the run
     - approval requests (step 4) use the same channel
   - `run_auditor` only when the summary flags something: advisory, never clears an alert, not in notification text, opt-in per schedule
   - audit retention beyond the run, and for the agent versions runs reference, since background runs are reviewed later
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
