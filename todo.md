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
- background logging in the host - tool calls, network access (allowed and denied) - a trusted record the agent can't hide or fake
- schedule tasks - see docs/service.md for one-off and recurring task semantics
- cofigure system prompt for agent
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

## Service foundation

Implemented: local daemon and CLI, durable run queue and event replay, cancellation,
deadlines, session serialization, and restart recovery. Next milestones and API:
[docs/service.md](docs/service.md).
