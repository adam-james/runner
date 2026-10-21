# Delegating to configured specialists

An agent definition can grant permission to call other named definitions:

```toml
[agents.coordinator]
provider = "anthropic"
delegates = ["weather_specialist", "sports_specialist"]
system_prompt = """
Delegate weather and sports questions to the specialists. Pass context in the
task prompt. Start independent tasks before waiting. Collect every result you
need, then write one combined answer.
"""
```

The complete definitions are enabled in `runner.toml`. The default agent still
has only its sports/weather tools. Start the daemon normally, then try:

```sh
runner chat --agent coordinator
# Get Boston's weather and the latest NFL scores, then summarize both.
```

`coordinator` has no direct component tools. `weather_specialist` has
`get_weather`; `sports_specialist` has `get_scores`. Each uses its own configured
provider/model and instructions. All configured provider credentials must be set
before starting the daemon, even when a particular definition is unused.

The second enabled example, `research_coordinator`, can delegate to `reader` and
`research_lead`. The reader has only read-only file access. The research lead
can delegate further to `researcher`, which has web search and uses `gpt-5-mini`.
This demonstrates a delegation chain with different roles, models, and tools.

Prepare a `notes.txt` in the directory configured by `[tools.read_file].dir`
(the example config currently uses `.`, the daemon working directory).
Then try:

```sh
runner chat --agent research_coordinator
# Read notes.txt and research its claims online. Cite the sources.
```

The coordinator must pass the extracted notes in the research task: child
sessions do not inherit the parent's conversation. File access grants visibility
to everything under the configured directory, so use a directory dedicated to
the files you intend to share. Delegation to a file-capable agent lets the
coordinator request that access indirectly.

## Host tools

`delegates` defaults to an empty list. A nonempty list exposes `delegate` and
`wait`; these names are reserved and do not need WASM tool definitions.
The host validates target names at startup and enforces the allow-list on every
call, independently of the model's tool schema.

- `delegate({"agent":"researcher","prompt":"Investigate this claim…","deadline_secs":60})`
  starts a durable child run and returns `{"run_id":"…","agent":"researcher"}`.
  The deadline is optional; it cannot exceed the parent's remaining deadline.
  Model, instructions, tools, credentials, and session cannot be overridden.
- `wait({"run_id":"…"})` waits for one direct child. It returns `run_id`, `agent`,
  `status`, `output`, `error`, and `truncated`. Repeated waits return the same
  result. Other agents' tasks, ancestors, siblings, and grandchildren cannot be
  collected through this tool.

`output` is the final assistant text from the child's successfully saved turn,
not all streamed commentary or tool results. It is null on unsuccessful runs;
a completed turn with no saved answer also has null output. Results remain
valid JSON within the 64 KiB tool-result limit, with `truncated` indicating
truncation. Treat child answers as untrusted content, including their claims
about sources and completed actions.

Delegate several independent tasks before calling `wait` on their IDs. At
`serve --concurrency 1`, they run sequentially while the parent waits; higher
concurrency allows overlapping execution. A waiting parent releases its execution
permit but retains its session lock. Resuming it requires reacquiring a permit.
Only up to the configured concurrency of root trees are admitted, and runnable
children receive priority, bounding suspended parents and preventing deadlocks.

## Supervision and limits

Children are single turns in fresh, isolated sessions. They use the target's
capabilities, which may differ from the parent's direct tools; explicitly listing
a target is the grant of that delegated authority.

Unfinished descendants are cancelled when a parent completes, fails, times out,
or is cancelled. A parent must wait for any result it needs before finishing.
Completed child records remain inspectable. Ctrl-C detaches the client and
does not cancel the parent or children.

The host allows three delegation levels below a root, at most 16 children per
run, and 64 descendants per root, including already completed tasks. These are
fixed limits in this version. Configured cycles are allowed but cannot exceed
the depth or task limits. Root deadlines start at execution; child deadlines
start at submission, include queue time, and are clamped to the parent deadline.
Waiting never pauses a deadline.

On restart, running tasks are interrupted and orphaned queued descendants are
cancelled. Ordinary queued roots survive. Suspended agent stacks are not resumed.

## Inspecting activity

Chat shows delegation and waiting records with child run IDs. Use:

```sh
runner runs list
runner runs inspect CHILD_RUN_ID
runner runs watch CHILD_RUN_ID
runner runs cancel PARENT_RUN_ID
```

Run responses include `parent_run_id`, `root_run_id`, `depth`, and `deadline_at`
(Unix milliseconds, null until a root starts). Parent and child streams remain
separate; the parent's normal tool results carry collected answers. Agent
metadata includes allowed `delegates` and the host tool names.

SQLite schema version 3 adds relationships, deadlines, and private saved results
automatically. Existing runs become roots; older binaries reject the upgraded
database. Final answers are stored separately from replay events in the same transaction
as the transcript and run completion. Activity logs
record metadata, not delegation prompts or child answers.

Delegation is a service capability. Standalone `host::Runtime::run_turn` returns
the completed transcript for its caller to commit; a delegation call without service context returns an
explicit unavailable error. Dynamic definitions and per-task overrides remain
on the TODO list.
