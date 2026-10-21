# TODO

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
