# TODO

## Near term

- send `prompt_cache_key` (session id) to openai - better cache hit odds (anthropic has no equivalent)
- have different agents
- non-streaming chat option - providers always send `"stream": true` now

### From the PR #7 review (local agent service)

- keep tool and provider instances per session - `Runtime::run_turn` (host/src/lib.rs) instantiates the provider, every tool and the session store each turn, so tool state (e.g. python variables) no longer carries over between turns, and the instantiation cost is paid every turn
- remove `ComponentConfig.resolved` - expand env vars once in `load_config`/`from_config` and drop `expand_env` from `SandboxHooks::new` and `mounted_dir`
- close the late-cancel race - a cancel that lands after the agent saves the turn but before it returns can still record `cancelled`; needs the agent to report when it saves
- retry failed terminal-state writes - `contain` (runner/src/service.rs) forgets a finished job even if its fallback `db.finish` fails, leaving the run `running` and blocking later turns in that session until restart; retain pending terminal updates and retry them

## Long term

- MCP
- skills
- wasm inspector - turn to s-expression and have llm read it - inspect agents and tools
- named agent definitions and explicit delegation (the local service now runs concurrent tasks)
- background logging in the host - tool calls, network access (allowed and denied) - a trusted record the agent can't hide or fake
- switch build target to wasm32-wasip3 once Rust 1.100 ships it on stable (~late Nov 2026)
- migrate tools to WASI 0.3 http (off wstd) - wait for a p3 wstd or a need for streaming tool output / parallel tool calls
- wasm shell
- memory
- schedule tasks - see docs/service.md for one-off and recurring task semantics
- investigate bug where output gets cut off
- OAuth/login

## Service foundation

Implemented: local daemon and CLI, durable run queue and event replay, cancellation,
deadlines, session serialization, and restart recovery. Next milestones and API:
[docs/service.md](docs/service.md).
