# runner

A sandboxed agent runner. The agent loop (conversation history, system
prompt) runs inside a WASM guest; a native host process is the only thing
that ever touches API keys, the network, or which provider/model/tools are
in use. The agent's only capability besides standard I/O is two typed calls
into the host — `chat` and `call-tool` — it cannot read environment
variables, open sockets, or see any key, and it has no say in which
provider, model, or tools serve its calls. Any agent component works
unmodified against whatever the host's config file sets up.

Tools are a second, separate kind of wasm component. Unlike the agent, a
tool _does_ get real network access (`wasi:http`), because that's what
tools are generally for — but each tool only gets to reach the hosts its
own config entry allow-lists; anything else is rejected by the host before
it ever touches the network.

## Layout

- `wit/chat.wit` — the agent/host interface. `chat-request` has no
  `provider`/`model`/`tools` field — the host builds all of that itself and
  injects it into the real provider API call. The agent only sees `tool-use`
  content blocks in responses and a generic `call-tool(name, input)` import
  to act on them; it never knows what a tool actually does.
- `wit/tool.wit` — the tool/host interface. A tool exports `describe()`
  (name, description, JSON input schema) and `call(input-json)`. The host
  calls `describe()` once at startup to learn what's available and to build
  the provider's native tool-calling schema; `call()` runs it.
- `host/` — native binary. Owns `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`, loads
  the agent and every configured tool from a TOML config file, restricts
  each tool's outbound `wasi:http` to its own allow-list, and runs the show
  under Wasmtime.
- `agent/` — the guest. A REPL: reads a line, calls `chat`, and for as long
  as the response asks for tools, calls them via `call-tool` and feeds the
  results back, before printing the final reply.
- `tools/weather/` — the first tool. Geocodes a location name and fetches
  current conditions from Open-Meteo (free, keyless).

Only the key for the provider actually in use needs to be set. The host
reads both `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` opportunistically and
doesn't fail fast at startup if one is absent — a missing key surfaces as a
`chat error: ChatError::Internal("... is not set")` on the first call that
needs it instead, and the REPL keeps running.

## Setup

```bash
rustup target add wasm32-wasip2
```

## Configuration

The host takes a single positional argument: a TOML config file.

```toml
agent = "target/wasm32-wasip2/release/agent.wasm"
provider = "anthropic"
model = "claude-sonnet-5"

[[tools]]
path = "target/wasm32-wasip2/release/weather.wasm"
allowed_hosts = ["api.open-meteo.com", "geocoding-api.open-meteo.com"]
```

All workspace members (`host`, `agent`, `tools/weather`) share a single
`target/` directory at the repo root — that's plain Cargo workspace
behavior, not specific to this project — so every `.wasm` ends up under
`target/wasm32-wasip2/release/`, never under e.g. `tools/weather/target/`.

Credentials are deliberately **not** in this file — they stay as
`ANTHROPIC_API_KEY`/`OPENAI_API_KEY` env vars, since a config file like this
is the kind of thing that ends up checked into source control.

## Build and run

```bash
# Build the agent and the weather tool
cargo build -p agent --release --target wasm32-wasip2
cargo build -p weather --release --target wasm32-wasip2

# Run the host against a config (see runner.toml for a working example)
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p host --release -- runner.toml
```

Type `exit` or `quit`, or send EOF (Ctrl-D), to end the session.

Note: if `target/` lives on a filesystem that doesn't support `mmap` for
rlib archiving (e.g. a virtiofs-mounted directory on macOS-hosted sandboxes),
set `CARGO_TARGET_DIR` to a path on a native filesystem before building, and
adjust the paths in your TOML config to match.

Sample session:

```
> My name is Ada, remember it.
Got it, Ada.
> What's the weather in Seattle?
Current weather in Seattle: 14.2°C, wind 9.1 km/h
> quit
```

## Switching provider or adding tools

Edit the TOML, no recompiling needed — the same agent and tool binaries work
against any provider/model/tool-set the config points them at:

```toml
agent = "target/wasm32-wasip2/release/agent.wasm"
provider = "openai"
model = "gpt-5"

[[tools]]
path = "target/wasm32-wasip2/release/weather.wasm"
allowed_hosts = ["api.open-meteo.com", "geocoding-api.open-meteo.com"]
```

A new tool is a new `[[tools]]` entry pointing at its own component, with
its own allow-list — the agent's code never changes.

## Verify the sandboxing

```bash
wasm-tools component wit "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/agent.wasm"
```

The agent's imports should be limited to `local:agent/chat-api` plus the
standard `wasi:cli`/`wasi:io`/`wasi:clocks` set — no `wasi:http`, no
`wasi:sockets`, no way to read env vars.

```bash
wasm-tools component wit "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/weather.wasm"
```

The weather tool's imports _should_ include `wasi:http/outgoing-handler` —
that's expected, tools are meant to reach the network. What the host
restricts is which hosts those calls can actually reach: set a tool's
`allowed_hosts` to `[]` in the config and its first request will be
rejected before any network attempt, logged as `tool network access
denied: <host>` on the host's stderr.
