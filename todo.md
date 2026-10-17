# TODO

## Near term

- search tool - tavily
- sessions and session storage (resume)
- make agent look better
- have different agents
- non-streaming chat option - providers always send `"stream": true` now

## Long term

- wasm inspector - turn to s-expression and have llm read it - inspect agents and tools
- orchestrator - run more than one agent at a time
- skills
- MCP
- switch build target to wasm32-wasip3 once Rust 1.100 ships it on stable (~late Nov 2026)
- migrate tools to WASI 0.3 http (off wstd) - wait for a p3 wstd or a need for streaming tool output / parallel tool calls
