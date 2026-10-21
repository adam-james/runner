#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"

# All workspace members except these native crates are WASM components.
cargo build --locked --release --target wasm32-wasip2 --workspace \
    --exclude host --exclude runner --exclude build-python

cargo build --locked --release -p runner
cargo run --locked --release -p build-python

printf '\nBuild complete. Run these commands from the repository root:\n\n'
printf '  cd %q\n' "$PWD"
cat <<'INSTRUCTIONS'

Terminal 1 — start the server (runner.toml uses OpenAI by default):
  export OPENAI_API_KEY='your-api-key'
  cargo run --locked --release -p runner -- serve --config runner.toml

Terminal 2 — start chat from the same repository:
  cargo run --locked --release -p runner -- chat

Type exit or press Ctrl-D to leave chat. Press Ctrl-C in Terminal 1 to stop
the server. Restart the server after changing runner.toml.

All components are built, including Python. Enable additional tools and add
their names to [agents.default].tools in runner.toml to use them.
INSTRUCTIONS

if [[ -n "${CARGO_TARGET_DIR:-}" && "$CARGO_TARGET_DIR" != target ]]; then
    printf '\nCARGO_TARGET_DIR is %s. Update the target/ paths in runner.toml to match.\n' "$CARGO_TARGET_DIR"
fi
