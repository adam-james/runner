# Housekeeping

- move runner code to host
- report a `dir` that fails to mount: a missing read-only `dir` isn't created (on purpose: an empty read-only mount hides a typo), but the reason reaches no one: `sandbox.create` ends `interrupted` and the client sees "agent execution failed"
  - check each component's `dir` when the daemon starts and refuse to start with the reason, like a missing env var
  - log why a component failed to load on the daemon's stderr (host-made errors: paths, not guest output)
  - record the reason in the trail, e.g. `sandbox.create` with `outcome: "dir_missing"`, instead of `interrupted`
- desktop notifications on Linux: enable `notify-rust` there too (pure-Rust zbus to the freedesktop D-Bus interface, no subprocess); escape `&`, `<`, `>` in the body, since servers may render markup; test it on a real desktop session; notifications stored meanwhile were never marked posted, so mark them posted when it ships rather than posting them all as one
- OAuth/login
- encrypted store for API keys - nice to have, not a differentiator
- prune bundled demo tools (sports, weather) in favor of a few genuinely useful ones (files, web, python, git, http fetch)
- are there APIs for booking flights and other useful things like that?
- wasm shell
- switch build target to wasm32-wasip3 once Rust 1.100 ships it on stable (~late Nov 2026)
- migrate tools to WASI 0.3 http (off wstd) - wait for a p3 wstd or a need for streaming tool output / parallel tool calls
