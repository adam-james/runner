# runner

A sandboxed agent runner. Everything that runs is a WASM component — the
agent, the model provider, and each tool — and a small native host process
wires them together. The host is the only thing that ever touches API keys
or decides which provider/model/tools are in use.

- The **agent** (conversation loop, system prompt) has no network access at
  all. Its only capability besides standard I/O is two typed calls into the
  host — `chat` and `call-tool` — and it has no say in which provider,
  model, or tools serve them.
- A **provider** translates a `chat` call into one vendor's API (Anthropic,
  OpenAI, …). It gets `wasi:http`, but can only reach the hosts its config
  allow-lists, and never sees an API key: the host adds the credential
  headers to its outbound requests on the way out.
- A **tool** is the same deal: `wasi:http` restricted to its own
  allow-list, plus any headers the config injects.

Swapping provider, model, or tools is a config change; none of the
components need recompiling.

Replies stream: text is printed as the model generates it. That's built on
WASI 0.3 (Component Model async): `chat` is an `async func` returning a
`stream<string>` of text pieces plus a `future` of the complete response,
both across the agent/host and host/provider boundaries. Tools are plain
synchronous WASI 0.2 components. Everything still builds for the
`wasm32-wasip2` Rust target; the async parts come from the `wasip3` crate.

## Layout

- `wit/messages.wit` — the conversation itself (messages and their content
  blocks: text, tool uses, tool results), shared by the agent, providers,
  and session stores.
- `wit/types.wit` — the types around a `chat` call (`chat-request`/
  `chat-response`, errors, and `chat-stream`: a reply in progress) shared
  by the agent and providers.
- `wit/chat.wit` — the agent/host interface. `chat` is async and returns
  a `chat-stream`. `chat-request` has no
  `provider`/`model`/`tools` field — the host supplies all of that. The
  agent only sees `tool-use` content blocks in responses and a generic
  `call-tool(name, input)` import to act on them; it never knows what a tool
  actually does.
- `wit/provider.wit` — the provider/host interface. A provider exports an
  async `chat(model, tools, request)` returning a `chat-stream`; the host
  fills in `model` and `tools` from its config, passes the agent's request
  through unchanged, and copies the stream back to the agent as it arrives
  (each component has its own store, so streams can't be handed across
  directly).
- `wit/tool.wit` — the tool/host interface. A tool exports `describe()`
  (name, description, JSON input schema) and `call(input-json)`. The host
  calls `describe()` once at startup to learn what's available and to build
  the provider's native tool-calling schema; `call()` runs it.
- `wit/session.wit` — sessions. The agent imports `session` (`load()` and
  `save(messages)`, with no session id: the host decides which session
  this run is); a session store exports `session-store` (`load`/`save` by
  id, plus `ids()`), and the host passes the agent's calls through to it.
- `host/` — native binary. Loads the agent, the provider, and every tool
  from a TOML config file, restricts each provider/tool's outbound
  `wasi:http` to its own allow-list, injects configured headers
  (credentials) into those requests, and runs the show under Wasmtime. It
  has no provider-specific code.
- `agent/` — the guest. A REPL: reads a line, calls `chat` and prints its
  text as it streams in, and for as long as the response asks for tools,
  calls them via `call-tool` and feeds the results back. Its entry point is
  the async WASI 0.3 `wasi:cli/run` export, which streaming requires.
  Loads its session at startup and saves it after every turn.
- `providers/anthropic/`, `providers/openai/` — the provider components,
  for the Messages API and Chat Completions API respectively. Each sends a
  streaming request over WASI 0.3's `wasi:http/client`, forwards text
  deltas as they arrive, and rebuilds the full response (including tool
  calls) from the server-sent events.
- `tools/weather/` — the first tool. Geocodes a location name and fetches
  current conditions from Open-Meteo (free, keyless), plus an hourly
  forecast for up to the next 48 hours if asked for `hours`.
- `tools/sports/` — the second tool. Scores and schedules for a league on a
  given day (NFL, NBA, MLB, NHL, EPL, …) from ESPN's public scoreboard API
  (keyless), optionally filtered to one team. Reads the wall clock
  (`wasi:clocks`) to resolve "today", "yesterday", etc. in US Eastern time.
- `tools/search/` — web search via Tavily. The first tool that needs an
  API key; like the providers, it never sees it — the host injects the
  `authorization` header from the config's `headers`.
- `stores/fs/` — a session store that keeps each session as a JSON file
  in the one directory its config gives it.

Only the keys the config actually uses need to be set — the provider's,
plus `TAVILY_API_KEY` if the search tool is configured. The host only reads
the env vars the config's `headers` reference, and fails at startup with
`environment variable ... is not set` if one is missing.

## Setup

```bash
rustup target add wasm32-wasip2
```

## Configuration

The host takes a TOML config file as its argument (default `runner.toml`).

```toml
agent = "target/wasm32-wasip2/release/agent.wasm"

[provider]
path = "target/wasm32-wasip2/release/anthropic.wasm"
model = "claude-sonnet-5"
allowed_hosts = ["api.anthropic.com"]
headers = { x-api-key = { value = "${ANTHROPIC_API_KEY}", hosts = ["api.anthropic.com"] } }

[[tools]]
path = "target/wasm32-wasip2/release/weather.wasm"
allowed_hosts = ["api.open-meteo.com", "geocoding-api.open-meteo.com"]

[[tools]]
path = "target/wasm32-wasip2/release/sports.wasm"
allowed_hosts = ["site.api.espn.com"]

[[tools]]
path = "target/wasm32-wasip2/release/search.wasm"
allowed_hosts = ["api.tavily.com"]
headers = { authorization = { value = "Bearer ${TAVILY_API_KEY}", hosts = ["api.tavily.com"] } }
```

All workspace members (`host`, `agent`, `providers/*`, `tools/*`) share a single
`target/` directory at the repo root — that's plain Cargo workspace
behavior, not specific to this project — so every `.wasm` ends up under
`target/wasm32-wasip2/release/`, never under e.g. `tools/weather/target/`.

`headers` (on the provider or any tool) are added by the host to that
component's HTTPS requests, overwriting any same-named header the component
set itself. `${NAME}` in a value is replaced with env var `NAME` when the
host starts. That's how credentials stay out of both the components and
this file — a config file like this is the kind of thing that ends up
checked into source control.

Two rules keep a header from going further than intended:

- **Explicit hosts.** Every header names the hosts it's sent to, which must
  also be in `allowed_hosts` — even when that just repeats the entry's one
  allowed host. Adding a host to `allowed_hosts` later never starts sending
  it credentials by itself.
- **HTTPS only.** A plain-`http://` request to an allowed host still goes
  through, but without the configured headers (the host logs `not sending
  header ... over plain http`), so a component can't send a credential in
  the clear.

## Build and run

```bash
# Build the agent, providers, and tools
cargo build -p agent -p anthropic -p openai -p weather -p sports -p search \
    -p fs-store --release --target wasm32-wasip2

# Run the host against a config (see runner.toml for a working example)
export ANTHROPIC_API_KEY=sk-ant-...
export TAVILY_API_KEY=tvly-...   # for the search tool
cargo run -p host --release -- runner.toml
```

Type `exit` or `quit`, or send EOF (Ctrl-D), to end the session.

Note: if `target/` lives on a filesystem that doesn't support `mmap` for
rlib archiving (e.g. a virtiofs-mounted directory on macOS-hosted sandboxes),
set `CARGO_TARGET_DIR` to a path on a native filesystem before building, and
adjust the paths in your TOML config to match.

Sample session:

```
runner · type exit or quit, or Ctrl-D, to leave

> My name is Ada, remember it.
Got it, Ada.

> What's the weather in Seattle?
⚙ weather(location: "Seattle") ✓ 0.8s · 52 B
Current weather in Seattle: 14.2°C, wind 9.1 km/h

> Did the Seahawks win last night?
⚙ sports(league: "nfl", days_from_today: -1, team: "Seahawks") ✓ 0.4s · 1.2 KB
Yes — the Seahawks beat the 49ers 24–17.

> quit
```

Each tool call is shown as it runs, with its arguments (long ones cut
short), how long it took, and the size of its result. In a terminal the
REPL also uses color and shows `…` while waiting for a reply; piped
output stays plain text.

## Sessions

With a `[session_store]` in the config, every run is a session that's
saved after each turn, and can be picked up again later:

```toml
[session_store]
path = "target/wasm32-wasip2/release/fs_store.wasm"
dir = "${HOME}/.runner/sessions"
```

```bash
cargo run -p host --release -- runner.toml                    # a new session
cargo run -p host --release -- runner.toml --sessions         # list saved ones
cargo run -p host --release -- runner.toml --session 20261017-081500   # resume
```

A new session is named after when it started (UTC), and the host prints
`session saved; resume with --session <id>` on exit. `--session <id>` with
an id that isn't saved yet starts a session under that name, so
`--session trip` works too. Ids are letters, digits, `-`, `_` and `.`.

The store is a sandboxed component like any other. `dir` gives it one
directory on the host (created if missing), mounted as its `/`, and it
can't see anything outside it. `dir` works the same on a provider or tool,
and expands `${NAME}` like header values do. A store backed by something
else, such as a database behind an HTTP API, would use `allowed_hosts` and
`headers` instead; the agent can't tell the difference. Without a
`[session_store]`, nothing is saved.

## Switching provider or adding tools

Edit the TOML, no recompiling needed — the same agent and tool binaries work
against any provider/model/tool-set the config points them at:

```toml
agent = "target/wasm32-wasip2/release/agent.wasm"

[provider]
path = "target/wasm32-wasip2/release/openai.wasm"
model = "gpt-5"
allowed_hosts = ["api.openai.com"]
headers = { authorization = { value = "Bearer ${OPENAI_API_KEY}", hosts = ["api.openai.com"] } }

[[tools]]
path = "target/wasm32-wasip2/release/weather.wasm"
allowed_hosts = ["api.open-meteo.com", "geocoding-api.open-meteo.com"]
```

A new tool is a new `[[tools]]` entry pointing at its own component, with
its own allow-list — the agent's code never changes. A new provider is a
new component exporting `provider-api` — the host's code never changes.

## Verify the sandboxing

```bash
wasm-tools component wit "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/agent.wasm"
```

The agent's imports should be limited to `local:agent/chat-api` and
`local:agent/session` (and the type-only `local:agent/messages` and
`local:agent/types`) plus the
standard `wasi:cli`/`wasi:io`/`wasi:clocks` set — no `wasi:http`, no
`wasi:sockets`, no way to read env vars. Its one export is
`wasi:cli/run@0.3.0`.

```bash
wasm-tools component wit "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/weather.wasm"
```

The weather tool's imports _should_ include `wasi:http/outgoing-handler`,
and the providers' `wasi:http/client@0.3.0` — that's expected, they're
meant to reach the network. What the host restricts is which hosts those calls can actually
reach: set a provider's or tool's `allowed_hosts` to `[]` in the config and
its first request will be rejected before any network attempt, logged as
`network access denied: <host>` on the host's stderr.
