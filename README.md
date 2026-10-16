# runner

A sandboxed agent runner. The agent loop (conversation history, system
prompt) runs inside a WASM guest; a native host process is the only thing
that ever touches API keys, the network, or which provider/model is in use.
The guest's only capability besides standard I/O is a single typed `chat`
call into the host — it cannot read environment variables, open sockets, or
see any key, and it has no say in which provider or model serves its calls.
Any guest component works unmodified against whichever provider the host is
configured for.

## Layout

- `wit/chat.wit` — the shared interface between host and guest. `chat-request`
  deliberately has no `provider`/`model` field — that's host configuration,
  not something the guest states or can override.
- `host/` — native binary. Owns `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`, picks
  the provider and model from its own `--provider`/`--model` CLI flags (see
  Examples below), makes the real HTTPS call, and runs the guest component
  under Wasmtime.
- `agent/` — the guest. A REPL: reads a line, calls `chat` with the full
  conversation history, prints the reply, repeats. It has no concept of
  "provider" or "model" at all.

Only the key for the provider actually in use needs to be set. The host
reads both `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` opportunistically and
doesn't fail fast at startup if one is absent — a missing key surfaces as a
`chat error: ChatError::Internal("... is not set")` on the first call that
needs it instead, and the REPL keeps running.

## Setup

```bash
rustup target add wasm32-wasip2
```

## Build and run

```bash
# Build the guest component
cargo build -p agent --release --target wasm32-wasip2

# Run the host against it (set whichever key the chosen provider needs;
# anthropic/claude-sonnet-5 is the default if you pass no extra flags)
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p host --release -- "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/agent.wasm"
```

The host consumes `--provider`/`--model` itself; anything else after the
component path on the host's command line is forwarded through to the guest
as its own argv.

Type `exit` or `quit`, or send EOF (Ctrl-D), to end the session.

Note: if `target/` lives on a filesystem that doesn't support `mmap` for
rlib archiving (e.g. a virtiofs-mounted directory on macOS-hosted sandboxes),
set `CARGO_TARGET_DIR` to a path on a native filesystem before building.

## Examples

Default (Anthropic, `claude-sonnet-5`):

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p host --release -- "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/agent.wasm"
```

OpenAI instead — the exact same, unmodified guest binary:

```bash
export OPENAI_API_KEY=sk-...
cargo run -p host --release -- "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/agent.wasm" \
    --provider openai --model gpt-5
```

A specific Anthropic model:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p host --release -- "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/agent.wasm" \
    --provider anthropic --model claude-opus-5-5
```

Sample session:

```
> My name is Ada, remember it.
Got it, Ada.
> What's my name?
Your name is Ada.
> quit
```

## Verify the guest can't see the API key

```bash
wasm-tools component wit "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/agent.wasm"
```

Imports should be limited to `local:agent/chat-api` plus the standard
`wasi:cli`/`wasi:io`/`wasi:clocks` set — no `wasi:http` or `wasi:sockets`.
