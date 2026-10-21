# TODO

- detach from a chat session, let it keep working in the background, and re-attach to it later
  - today: Ctrl-C mid-reply detaches that one run (prints its run ID) and `runner runs watch RUN_ID` follows it, but `chat --session` only replays completed turns - it doesn't show or follow a turn still running
  - re-attach: after the history, show the session's running (and queued) turns and follow them live, then give the prompt
  - a detached session could keep taking work: queue several prompts and leave, or let a long multi-turn task run unattended
  - a prompt typed while an earlier turn is still running queues silently behind it ("Thinking…") - say it's waiting
  - the detach notice prints in red like an error; make it a normal dim line
  - `runner sessions list` could show which sessions have running/queued work

## Long term

- explicit delegation between agent definitions - host-provided `delegate`/`wait` tools; see docs/service.md roadmap
- schedule tasks - see docs/service.md for one-off and recurring task semantics
- can the agent implementation just be native?
- can we get rid of the session store since that's in sqlite now
- agent to agent communication
- memory
- component registry
- OAuth/login
- switch build target to wasm32-wasip3 once Rust 1.100 ships it on stable (~late Nov 2026)
- migrate tools to WASI 0.3 http (off wstd) - wait for a p3 wstd or a need for streaming tool output / parallel tool calls
- wasm inspector - turn to s-expression and have llm read it - inspect agents and tools
- wasm shell
- security audit logging alongside debug logs
  - threat model: malicious guest components; trust the runner and host to enforce policy and record activity
  - add structured, versioned events with event IDs, sequence numbers, actor/run/session identity, component version, action, policy decision, and outcome
  - record activity at host-controlled boundaries so guests cannot forge, suppress, or bypass records; bound and escape guest-controlled metadata
  - use durable host-owned storage inaccessible to guests; define behavior when storage fails or guests flood the audit stream, including which actions must block
  - cover tool execution, allowed and denied network/filesystem access, capability violations, and guest traps or resource-limit termination
  - define retention, audit access controls, alerts, and detection of missing events or collector outages
  - keep audit records metadata-only: omit prompts, credentials, tool payloads, and raw errors

## Service foundation

Implemented: local daemon and CLI, durable run queue and event replay, cancellation,
deadlines, session serialization, and restart recovery. Next milestones and API:
[docs/service.md](docs/service.md).
