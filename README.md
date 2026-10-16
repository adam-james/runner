# runner

A sandboxed agent runner. The agent loop (conversation history, system prompt,
model choice) runs inside a WASM guest; a native host process is the only
thing that ever touches the Anthropic API key or the network. The guest's
only capability besides standard I/O is a single typed `chat` call into the
host — it cannot read environment variables, open sockets, or see the key.

## Layout

- `wit/chat.wit` — the shared interface between host and guest.
- `host/` — native binary. Owns `ANTHROPIC_API_KEY`, makes the real HTTPS
  call to `api.anthropic.com`, and runs the guest component under Wasmtime.
- `agent/` — the guest. A REPL: reads a line, calls `chat` with the full
  conversation history, prints the reply, repeats.

## Setup

```bash
rustup target add wasm32-wasip2
```

## Build and run

```bash
# Build the guest component
cargo build -p agent --release --target wasm32-wasip2

# Run the host against it
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p host --release -- "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/agent.wasm"
```

Type `exit` or `quit`, or send EOF (Ctrl-D), to end the session.

Note: if `target/` lives on a filesystem that doesn't support `mmap` for
rlib archiving (e.g. a virtiofs-mounted directory on macOS-hosted sandboxes),
set `CARGO_TARGET_DIR` to a path on a native filesystem before building.

## Verify the guest can't see the API key

```bash
wasm-tools component wit "${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/agent.wasm"
```

Imports should be limited to `local:agent/chat-api` plus the standard
`wasi:cli`/`wasi:io`/`wasi:clocks` set — no `wasi:http` or `wasi:sockets`.
