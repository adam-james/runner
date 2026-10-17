# TODO

## Near term

- have different agents
- non-streaming chat option - providers always send `"stream": true` now

## Long term

- OAuth/login
- skills
- MCP
- wasm inspector - turn to s-expression and have llm read it - inspect agents and tools
- orchestrator - run more than one agent at a time
- background logging in the host - tool calls, network access (allowed and denied) - a trusted record the agent can't hide or fake
- switch build target to wasm32-wasip3 once Rust 1.100 ships it on stable (~late Nov 2026)
- migrate tools to WASI 0.3 http (off wstd) - wait for a p3 wstd or a need for streaming tool output / parallel tool calls
- wasm shell
- memory
- schedule tasks
- investigate bug where output gets cut off
