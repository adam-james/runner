# runner

A sandboxed agent runner. Model providers and tools are WASM components, and a
persistent local daemon wires them together. The host is the only thing that
ever touches API keys or decides which provider/model/tools are in use.

- The **agent loop** is the host's own code: for each turn it loads the
  session, sends the conversation to the agent's provider, runs the tool calls
  the model asks for, and saves the session. An agent definition in the config
  picks its provider, model, tools, and system prompt.
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
across the host/provider boundary. Tools are plain
synchronous WASI 0.2 components. Everything still builds for the
`wasm32-wasip2` Rust target; the async parts come from the `wasip3` crate.

## Local service and CLI

Runner runs as a persistent local daemon. The daemon owns execution;
closing a client leaves its runs going. CLI and future desktop clients use the
same authenticated local API. The host emits structured events; the CLI owns
terminal input and rendering.

Build the components as described below, then install the CLI:

```bash
cargo install --path runner
runner serve --config runner.toml
```

Keep `serve` running in a separate terminal (or under your system's service
manager). Configuration paths are relative to the daemon's working directory.
The daemon requires an `[agents.default]` table listing the tools runs may use. Provider credentials come from the daemon's environment; clients do not need them.

```bash
runner chat                         # new conversation
runner chat --session research       # create or resume a named conversation
runner run "Summarize today's news" --detach
runner runs list
runner runs inspect RUN_ID
runner runs watch RUN_ID
runner runs watch RUN_ID --after EVENT_ID --json
runner runs cancel RUN_ID
runner sessions list
runner sessions create trip
runner run --session trip "Plan a weekend in Seattle"
runner agents list
runner run --agent researcher "What changed in the latest Rust release?"
```

`run` and `runs watch` print the run's agent and session ID on stderr, followed
by the agent's provider, model, and tools; `run --detach` prints the run ID on
stdout. Without `--detach`, `run` streams output until completion. Ctrl-C while
watching detaches successfully and leaves the run working; use `runs cancel`
to stop it. Failed, cancelled, and
interrupted runs produce a nonzero watcher exit status. `chat` submits one turn
at a time in the same session; its banner names the agent and session, followed
by the agent's provider, model, and tools. When it ends, it prints the command
to resume the session. Resuming a session with `chat --session` first replays
its completed turns: each prompt, the reply, and its tool calls. It then follows
active and queued turns in submission order before offering the next prompt.
Turns added during catch-up are followed too. Failed and cancelled turns from
earlier history aren't replayed, since they weren't saved to the conversation;
failures encountered while following pending work are displayed before continuing.
Ctrl-C while following exits chat normally and prints a resume command.

`runner tui` is a full-screen client for the same daemon. The left side lists
sessions, most recently active first, by their first prompt: ▶ running,
⏸ queued, ✓ or ✗ for how the latest run ended. Below that are the open
session's runs. The right side shows the session's conversation, streaming as
it runs, above a prompt box. Turns within a session run one at a time, so to run
things concurrently, start each in its own session with Ctrl-N. Quitting leaves
everything running. Reopen a session later, from the list or with `--session`,
to see its whole history, including runs still in progress. Launching without
`--session` starts on a new session, which its first prompt creates. It takes the
same `--agent` and `--deadline-secs` flags as `chat`. Unlike `chat`, it also
shows failed and cancelled turns, marked as not saved to the conversation.

| Key | Does |
| --- | --- |
| Enter | send the prompt; open the highlighted session |
| Ctrl-N | new session, to run something alongside the others |
| Ctrl-O | pick a past session (↑/↓, then Enter) |
| Ctrl-A | choose the agent for the next turns |
| ↑/↓, PgUp/PgDn, mouse wheel | scroll the conversation (End follows new output) |
| Tab / Shift-Tab | move between the prompt, Sessions, Runs and Conversation |
| Esc, then c | cancel the highlighted run in the Runs pane, after a confirmation |
| ? (outside the prompt) | show the keys |
| Ctrl-C, or q outside the prompt | quit; runs keep going in the daemon |

The TUI captures the mouse for scrolling, so hold Shift (Option in some macOS
terminals) while dragging to select text.

Queue additional turns while detached with
`runner run --session SESSION "next prompt" --detach`. A watcher shows
"Queued — waiting to run" until execution starts. `runner sessions list` includes
`running_runs` and `queued_runs` counts for each session.
`--session` on `run` requires an existing session. `--agent` on `run` and `chat`
picks a named agent definition from the config's `[agents.<name>]` tables (see
`runner.toml`); without it, runs use `default`.

Agents can delegate to explicitly allowed configured specialists using host
`delegate` and `wait` tools. Try `runner chat --agent coordinator` for sports and
weather, or `runner chat --agent research_coordinator` for local notes plus web
research. Children get fresh sessions and their own configured capabilities.
See [delegation examples and limits](docs/delegation.md).

The daemon runs up to four tasks concurrently (`serve --concurrency N`), while
serializing turns within a session. Runs have a ten-minute deadline by default;
set `run --deadline-secs N` or `chat --deadline-secs N` (1–86400 seconds). The
deadline starts when execution starts, excluding queue time. Text, usage, tool
activity, and status events are saved and replayable.

Service state lives in `~/.runner/service` by default. Override it with
`--data-dir PATH` on any command or `RUNNER_DATA_DIR`. The directory is private
on macOS/Linux and contains SQLite state, a daemon lock, and a private connection
file with a generated local API token. Only one daemon can use a data directory.
The service registers sessions in SQLite when they are created, and keeps each
session's conversation history there too.

Queued work survives daemon restarts. Work that was running becomes
`interrupted`; Runner never automatically retries it, because a tool may already
have changed external state. A turn is saved to its session's conversation only
together with its run's completion, so an interrupted, failed, or cancelled run
leaves the conversation as it was. Local background work requires the machine to
be awake and the daemon running.

See [service architecture and roadmap](docs/service.md) for the API and the
next milestones: scheduling, explicit delegation, and desktop clients.

### Verify the service

The standard tests use deterministic execution without API keys. The additional
runtime tests use the real turn loop and SQLite transcripts with a fixture
provider and tool, and also exercise CLI/daemon processes and crash recovery:

```bash
cargo test -p host -p runner
cargo build -p test-provider -p test-tool --target wasm32-wasip2
cargo test -p runner --test runtime -- --ignored
cargo clippy -p host -p runner --all-targets -- -D warnings
```

Fixtures are test components and should not be included in normal configs.

## Layout

- `wit/messages.wit` — the conversation itself (messages and their content
  blocks: text, tool uses, tool results), shared by the host and providers.
- `wit/types.wit` — the types around a `chat` call (`chat-request`/
  `chat-response`, errors, and `chat-stream`: a reply in progress) shared
  by the host and providers.
- `wit/provider.wit` — the provider/host interface. A provider exports an
  async `chat(model, tools, request)` returning a `chat-stream`; the host
  fills in `model` and `tools` from the agent definition and reads the
  stream's text as it arrives.
- `wit/tool.wit` — the tool/host interface. A tool exports `describe()`
  (name, description, JSON input schema) and `call(input-json)`. The host
  calls `describe()` once at startup to learn what's available and to build
  the provider's native tool-calling schema; `call()` runs it.
- `runner/` — local daemon, durable run queue, HTTP/SSE API, and CLI client.
- `host/` — runtime library. Loads the providers and tools from a TOML config
  file, restricts each provider/tool's outbound
  `wasi:http` to its own allow-list, injects configured headers
  (credentials) into those requests, and runs them under Wasmtime. Its turn
  loop calls the agent's provider, emits typed text, usage, and tool events,
  feeds tool results back to the model, and saves the session after a
  completed turn. It has no provider-specific code, and all terminal
  interaction lives in the CLI.
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
- `tools/files/` — `read_file`: reads a text file with numbered lines, a
  page at a time for long ones, or lists a directory. It sees only its
  config's `dir`, read-only, mounted as `/`. See [Files](#files).
- `tools/write-file/`, `tools/edit-file/` — `write_file` writes a whole
  text file (creating parent directories), and `edit_file` replaces an
  exact piece of one. Both need a `dir` with `dir_writable`. See
  [Files](#files).
- `tools/rustdocs/` — `rust_docs`: a crate's docs, or one item's, from
  docs.rs (doc.rust-lang.org for `std`, `core` and `alloc`), as text. See
  [Rust docs](#rust-docs).
- `tools/python/` — runs Python the model writes. The component is
  CPython plus `app.py`, built into a `tool-api` component by
  [componentize-py](https://github.com/bytecodealliance/componentize-py).
  The crate here (`build-python`) is the native program that does that
  build, using componentize-py as a library, so it needs no Python
  install. Python can't be compiled to WASM by itself, so
  this is one interpreter component that takes each script as its input,
  rather than a new component per script. See [Python](#python).

Only the keys the config actually uses need to be set — each configured
provider's, plus `TAVILY_API_KEY` if the search tool is configured. The host only reads
the env vars the config's `headers` reference, and fails at startup with
`environment variable ... is not set` if one is missing.

## Setup

```bash
rustup target add wasm32-wasip2
```

## Configuration

The daemon takes a TOML config file with `runner serve --config PATH`
(default `runner.toml`).

```toml
[providers.anthropic]
path = "target/wasm32-wasip2/release/anthropic.wasm"
model = "claude-sonnet-5"
allowed_hosts = ["api.anthropic.com"]
headers = { x-api-key = { value = "${ANTHROPIC_API_KEY}", hosts = ["api.anthropic.com"] } }

[tools.get_weather]
path = "target/wasm32-wasip2/release/weather.wasm"
allowed_hosts = ["api.open-meteo.com", "geocoding-api.open-meteo.com"]

[tools.get_scores]
path = "target/wasm32-wasip2/release/sports.wasm"
allowed_hosts = ["site.api.espn.com"]

[tools.web_search]
path = "target/wasm32-wasip2/release/search.wasm"
allowed_hosts = ["api.tavily.com"]
headers = { authorization = { value = "Bearer ${TAVILY_API_KEY}", hosts = ["api.tavily.com"] } }

[agents.default]
provider = "anthropic"
tools = ["get_weather", "get_scores", "web_search"]
```

Providers are named the same way: each `[providers.<name>]` table is a provider
component with its default `model`, and every agent definition picks one with
`provider = "<name>"`, optionally overriding its `model`. Several providers can
be configured at once, and different agents can use different ones. Each
configured provider's credentials must be set when the daemon starts, so
leave unused ones out or commented out.

Tool table keys are the names the model sees and agent definitions select. For
example, `[tools.read_file]` is selected with `tools = ["read_file"]`. Names must
contain 1–64 ASCII letters, digits, underscores, or hyphens. The component's
`describe` supplies its description and input schema; its legacy name is ignored.
To migrate an existing config, replace each `[[tools]]` header with
`[tools.<name>]` and use that name in agent definitions. All other tool settings
stay in that table. The same component may appear under several names with
different directories or permissions.

An agent definition may set `system_prompt` to replace the default ("You are a
helpful assistant."), `max_tokens` to cap each model reply (default 16384,
hidden reasoning included), and `max_model_calls` to cap the model requests in
one turn; a turn that hits either limit fails without being saved. The included
`runner.toml` also defines two agents you can try after restarting the server:

```bash
runner chat --agent reader       # read_file only, using the default model
runner chat --agent researcher   # web_search only, gpt-5-mini, its own system prompt
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

## Inspect capabilities before enabling tools

API keys never enter the sandbox. The host resolves configured credentials
and injects them into permitted outbound HTTPS requests. Guests receive no
credential environment variables or configured header values.

Inspect a configured tool without starting the daemon or supplying API keys:

```bash
runner tools inspect web_search --config runner.toml
```

The JSON report lists the component's WIT imports, configured allowed hosts,
injected header names and their destination hosts, directory access, and resource
limits. Header values are never included. Inspection compiles the component to
read its interface but does not instantiate it or call guest code. Directory
environment variables are expanded; header environment variables are not read.

Imports show which interfaces a component requests; config shows the host grants.
An HTTP import alone grants no network access, and socket imports do not grant
socket permissions. Directory access is confined to the configured mount, with
writes enabled only by `dir_writable`. An allowed host or directory can be
configured even if the component does not import the corresponding interface.
Inspection describes capabilities, not whether a tool's behavior is trustworthy.

`runner agents list` includes these reports for each agent's provider and tools.
Chat, run, and TUI banners summarize their network, filesystem, and injected
header grants. Delegation targets are listed separately; inspect those agents'
capabilities too when assessing what a delegating agent can do.

## Build and run

From the repository root, build the CLI, all Rust components, and the Python
tool in release mode:

```bash
./build.sh
```

The script prints the server and chat commands when the build finishes. It
builds everything without running tests or starting the server. Install the
`wasm32-wasip2` target first (see [Setup](#setup)).

In one terminal, start the server using the default OpenAI config:

```bash
export OPENAI_API_KEY='your-api-key'
cargo run --release -p runner -- serve --config runner.toml
```

In another terminal, from the same repository, start an interactive client:

```bash
cargo run --release -p runner -- chat
```

To use another provider or enable more tools, edit `runner.toml` and restart
the server. Set any credentials that config references in the server's terminal.
Add enabled tools to `[agents.default].tools` to make them available in chat.

Type `exit` or `quit`, or send EOF (Ctrl-D), to end the client session.
The daemon continues running.

Note: if `target/` lives on a filesystem that doesn't support `mmap` for
rlib archiving (e.g. a virtiofs-mounted directory on macOS-hosted sandboxes),
set `CARGO_TARGET_DIR` to a path on a native filesystem before building, and
adjust the paths in your TOML config to match.

Sample session:

```
session: <id> · type exit or quit, or Ctrl-D, to leave

> My name is Ada, remember it.
Got it, Ada.

> What's the weather in Seattle?
tool: weather
Current weather in Seattle: 14.2°C, wind 9.1 km/h

> Did the Seahawks win last night?
tool: sports
Yes — the Seahawks beat the 49ers 24–17.

> quit
```

The CLI displays reply text and tool activity from the daemon's event stream.
Terminal styling lives in the client; the agent emits no terminal output.

## Sessions

Each session's conversation is saved in the daemon's SQLite database after
every completed turn, and can be picked up later:

```bash
runner chat                          # new session
runner sessions list                 # list saved sessions
runner chat --session trip           # create or resume a named session
```

New sessions receive UUIDs. `chat --session ID` creates a named session if it
isn't saved yet, while `run --session ID` requires an existing session. IDs
use letters, digits, `-`, `_` and `.`, up to 64 characters, and cannot start
with a dot. Session listing uses the service's SQLite records.

## Python

The `run_python` tool runs a script the model writes and returns what it
printed, or the traceback if it failed. The code comes from the model, not
from us, so its config entry uses three settings no other tool needs (any
tool can use them):

```toml
[tools.run_python]
path = "target/wasm32-wasip2/release/python.wasm"
timeout_secs = 10      # a call that runs longer is cut off: "timed out after 10s"
max_memory_mb = 256    # past this, the script gets a MemoryError
fresh_per_call = true  # a new instance for every call
```

- `timeout_secs` covers both a script busy computing (`while True: pass`,
  interrupted by Wasmtime's epoch deadline) and one stuck waiting
  (`time.sleep(1000)`, cut off by a timer). Either way the model just
  gets an error back, and the agent continues the turn.
- Every tool's result (or error) is cut to 64 KB by the host before the
  model sees it. `app.py` cuts printed output shorter itself, but a script
  runs in the same interpreter and could undo that, so the host's cap is
  the one that holds.
- With `fresh_per_call = false` (the default), Python variables and other tool
  memory carry over between turns in the same session. Other sessions use
  separate instances. This memory lasts until the daemon stops or that session's
  execution is cancelled or traps; saved conversation history remains on disk.
- `fresh_per_call = true` gives every script a new instance. Starting an instance
  is cheap: componentize-py snapshots the interpreter after start-up. A tool that
  times out or traps is recreated before its next call with either setting.

Files, if you want them: give it a `dir`, and scripts can read what's in
it (as `/`, so `open("data.csv")` works) but can't write, create, delete
or rename anything there. Add `dir_writable = true` to let them write too;
what they write stays there for later calls, and nothing limits how much
they write, so point it at a scratch directory rather than your work.
Symlinks and `..` can't reach outside it either way, and the directory is
opened fresh for every call, so scripts always see its current contents.

The tool can't know any of this itself, as it's host config, so the host
adds a sentence to the description of any tool with a `dir`, saying
whether it's read-only or writable. That way the model knows what it can
do there.

Otherwise, no network, files or env vars: it has no `allowed_hosts`, gets
no env vars, and Python's `socket`, `http`, `subprocess` and `threading`
modules aren't even in the build. componentize-py bundles only the
standard library modules imported while it builds, which is why `app.py`
imports a list of useful ones (`datetime`, `statistics`, `csv`, `re`, …)
it doesn't use itself. To make another one available, add it there and
rebuild. Packages with C extensions (numpy etc.) aren't supported.

The component is about 20 MB. The host keeps Wasmtime's on-disk compile
cache turned on, so it's only compiled on the first run. The cache lives in
`~/Library/Caches/BytecodeAlliance.wasmtime` on macOS and
`~/.cache/wasmtime` on Linux, and is safe to delete. Its location and size
limits can be changed in the `[cache]` section of Wasmtime's
`config.toml`: `~/Library/Application Support/BytecodeAlliance.wasmtime/`
on macOS, `~/.config/wasmtime/` on Linux.

## Files

The `read_file` tool lets the model read files in one directory you
choose:

```toml
[tools.read_file]
path = "target/wasm32-wasip2/release/files.wasm"
dir = "${HOME}/notes"   # must exist; the model sees it as /
```

Given a directory, it lists what's there, with file sizes. Given a file,
it returns the lines numbered, at most 500 at a time by default (the model
can ask for up to 2000), and says which `offset` to read on from. Lines
over 1000 characters are cut, and a file that isn't UTF-8 text is refused
rather than dumped. As with every tool, the host caps a result at 64 KB.

It never writes: the tool has no code that writes, and the directory is
mounted read-only anyway, without `dir_writable`. The WASI sandbox keeps it
inside `dir`, so `..` and symlinks can't reach anything outside. With no
`dir` at all, every call fails, as there's nothing mounted to read.

### Writing and editing

Two more tools let the model change files, as separate components so
granting reads never grants writes:

```toml
[tools.write_file]
path = "target/wasm32-wasip2/release/write_file.wasm"
dir = "${HOME}/notes"
dir_writable = true

[tools.edit_file]
path = "target/wasm32-wasip2/release/edit_file.wasm"
dir = "${HOME}/notes"
dir_writable = true
```

- `write_file` takes a `path` and the file's whole `content`, creates any
  missing parent directories, and says whether it created the file or
  overwrote one. It won't replace a directory.
- `edit_file` replaces `old_string` with `new_string`. `old_string` must
  match the file exactly and appear in it only once, unless `replace_all`
  is set, so an edit can't land somewhere the model didn't mean; it's
  refused otherwise, with a reason the model can act on. The reply gives
  the line each replacement was made at.

Without `dir_writable` the directory is mounted read-only and every write
fails. Either tool can change or overwrite anything in `dir`, and nothing
limits how much is written, so point them at a directory you'd be fine
with the model rewriting, not your home directory. `..` and symlinks still
can't reach outside it.

## Rust docs

The `rust_docs` tool looks up Rust documentation, so the model can check
an API rather than go from memory:

```toml
[tools.rust_docs]
path = "target/wasm32-wasip2/release/rustdocs.wasm"
allowed_hosts = ["docs.rs", "doc.rust-lang.org"]
```

The model names a crate and, optionally, a path in it and a version
(`latest` by default; for `std`, a Rust version, `beta` or `nightly`):

- no path: the crate's front page, its docs and what's in it;
- `sync::Mutex`: that item's page, with its signature, docs, methods, and
  the traits it implements (headers only; blanket impls are left out);
- `sync`: a module's page;
- `sync::Mutex::lock`: just that method, field or variant.

It finds an item by its path in the crate's `all.html` index, so a path
that's wrong, such as `HashMap` for `collections::HashMap`, gets the close
matches back instead, and a member that doesn't exist gets the list of the
ones that do. Pages are converted to markdown-style text, with code
fenced, and come back 400 lines at a time by default, with an `offset` to
read on: `Iterator`'s page alone runs to over 4000 lines.

## Switching provider or adding tools

Edit the TOML, no recompiling needed — the same tool binaries work against any
provider/model the config points them at:

```toml
[providers.openai]
path = "target/wasm32-wasip2/release/openai.wasm"
model = "gpt-5"
allowed_hosts = ["api.openai.com"]
headers = { authorization = { value = "Bearer ${OPENAI_API_KEY}", hosts = ["api.openai.com"] } }

[tools.get_weather]
path = "target/wasm32-wasip2/release/weather.wasm"
allowed_hosts = ["api.open-meteo.com", "geocoding-api.open-meteo.com"]

[agents.default]
provider = "openai"
tools = ["get_weather"]
```

A new tool is a new `[tools.<name>]` table pointing at its own component, with
its own allow-list — the host's code never changes. A new provider is a
new component exporting `provider-api` — the host's code never changes.

## Verify the sandboxing

```bash
wasm-tools component wit "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/weather.wasm"
```

The weather tool's imports _should_ include `wasi:http/outgoing-handler`,
and the providers' `wasi:http/client@0.3.0` — that's expected, they're
meant to reach the network. What the host restricts is which hosts those calls can actually
reach: set a provider's or tool's `allowed_hosts` to `[]` in the config and
its first request will be rejected before any network attempt, logged as
`network access denied: <host>` on the host's stderr.

```bash
wasm-tools component wit "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/python.wasm"
```

The python tool imports no `wasi:http` at all. It does import
`wasi:sockets`, as every componentize-py component does, but the host
grants no socket permissions, so any connection attempt is denied.
