# Local agent service

The native service is the execution authority. Clients submit work and watch
persisted events; they never own agent lifetimes or receive provider
credentials. Providers and tools are WASM components with the existing
capability restrictions; the agent's turn loop is native host code, and
conversations are kept in the service's SQLite database. The CLI contains the
service's terminal UI. Interactive use goes through `runner chat`.

## Execution and persistence

An **agent definition** selects the provider, model, tools, and system prompt
for the host's turn loop. The top-level `[providers.<name>]` and
`[tools.<name>]` are shared pools; each `[agents.<name>]` table picks from them:
a required `provider` by its table key, an optional `model` overriding the
provider's default (one of the two must set a model), `tools` by their
configured table keys (none if omitted), an optional `system_prompt`, and
optional limits: `max_tokens` per model reply (default 16384, hidden reasoning
included) and `max_model_calls` per turn (default unlimited, bounded only by the
run's deadline). Both must be at least 1. The turn loop sends the system prompt
with every model request; without one it sends "You are a helpful assistant." An
empty `system_prompt` is rejected. The config must declare `[agents.default]`,
which runs use when they don't name a definition. Tool names use 1–64 ASCII
letters, digits, underscores, or hyphens and are used for model calls, dispatch,
and events; `describe` supplies descriptions and input schemas, and its legacy
name field is ignored. Duplicate table keys, invalid tool or provider names,
unknown references, and invalid agent names are rejected before instantiating
components. A run names its definition; a session is not bound to one, so turns
of one conversation may use different definitions. A **session** identifies a
conversation. A **run** performs one turn within a session and has a durable ID,
prompt, deadline, status, and event history.

`host::Runtime` shares its Wasmtime engine and compiled components. Each session
retains its own provider and tool instances across turns, so tool memory (such
as Python variables) survives. Runtime clones share this session state. Turns
within a session are serialized; different sessions use independent stores and
provider mutexes. `fresh_per_call` tools still start with a new instance on
every call. The instances belong to the definition that created them: a turn
that names a different definition than the session's previous turn starts with
new instances for its own definition, and later turns keep those.

Instances remain in memory until the runtime stops. They are not restored after
a daemon restart; saved conversation history is. A cancelled turn discards that
session's instances, since interrupted component calls cannot safely resume.
Tool timeouts and traps recreate that tool before its next call; provider traps
similarly recreate the provider. Normal application errors, including a failed
transcript load or save, retain state. Filesystem mounts remain shared according
to their configured paths and permissions; separate stores do not isolate
changes made to a shared writable directory.

Each turn loads the session's history, appends the prompt, and calls the
provider until a reply asks for no tools, running the requested tools in order
between calls. Each reply is capped at the agent's `max_tokens`. A turn that
reaches `max_model_calls` without a final reply is given up unsaved, like one
whose reply hits the token cap. The loop emits `text` as it streams and `usage`
after each reply, records tool execution and lifecycle transitions, and saves
the history after a completed turn. When the turn is given up, its message (e.g.
`API error 401: invalid x-api-key`, the token-limit cut-off, or the model-call
limit) becomes the run's `error`, with configured header values redacted and
capped at 500 characters. Other errors, such as failing to load a component, are
not published to clients because they may include reflected request data; those
runs say only `agent execution failed`.

SQLite stores sessions, submitted runs, ordered events, and each session's
transcript: its conversation as of its last completed turn, which the turn loop
loads before calling the model and replaces after a completed turn. The host
reaches it through `host::Transcripts`, which the service implements over its
database. A transcript that can't be loaded or saved fails the turn with `could
not load this session's history` or `could not save this turn`; the underlying
error is only logged. Submission and its initial event are one transaction;
terminal status and its event are another. A transcript write is not in the same
transaction as operational state. SQLite operations run on blocking tasks,
preserving `synchronous=FULL` durability without blocking Tokio threads. Each
active run queues output to a blocking consumer; its final events drain before
terminal status is committed. SSE watchers wake on per-run commit notifications
and replay from SQLite; active cancellation uses an in-memory token after the
durable request commits.

The queue executes the oldest eligible run, respecting global capacity and
one active run per session. Capacity defaults to four. Text is batched at 8 KiB
or about 50 ms; non-text events flush preceding text. Client reads use persisted
events rather than an in-memory broadcast channel, so a slow or disconnected
client cannot block execution or lose previously recorded output.

Run states are `queued`, `running`, `completed`, `failed`, `cancelled`, and
`interrupted`. Cancellation is a durable request; active runs become terminal
after execution is dropped. Queued cancellations take effect immediately.
A successful agent session save emits `turn_saved` before returning to the agent.
Once observed, the run completes even if cancellation, shutdown, or a deadline
wins before the agent returns; event persistence failures still fail the run.
Deadlines include component setup and all model/tool work, starting at claim.
Epoch yields make CPU-bound components interruptible; provider tasks are aborted
when the agent store is dropped. Tool-specific timeouts and fresh-instance
settings continue to apply.

On shutdown, active executions stop and become interrupted. On restart, the
service also marks any remaining running records interrupted, preserving queued
work. Interrupted work is never replayed automatically. This provides durable
submission, not exactly-once external side effects or resumption of a suspended
WASM stack. Runs and events are retained until the data directory is removed;
retention management is a later addition.

Sessions are registered in SQLite when created; a transcript is written for a
session after its first completed turn. Each data directory has its own sessions
and transcripts. Changing a daemon's config requires restarting it; queued work
uses the config active when it starts.

## Local API v1

Bind only to `127.0.0.1` on an OS-assigned port. Clients read `connection.json`
in the service directory and send `Authorization: Bearer TOKEN` on every
request. The token rotates on daemon startup. No CORS access is enabled, and
requests with an Origin header are rejected. CLI connections bypass proxies
and redirects. The filesystem lock prevents competing daemons for one data
directory; a descriptor left by a crash is replaced at the next startup.

| Method and route | Input / behavior |
| --- | --- |
| `GET /v1/agents` | List agent definitions, sorted by name, as `[{"name": "default", "provider": "openai", "model": "gpt-5", "tools": ["get_scores"]}, ...]` |
| `POST /v1/sessions` | `{ "id": "optional-name" }`; omitted/null ID generates one; returns session, HTTP 201 |
| `GET /v1/sessions` | List known sessions |
| `POST /v1/runs` | `{ "prompt": "...", "session_id": null, "agent": "default", "deadline_secs": 600 }`; returns queued run, HTTP 202 |
| `GET /v1/runs` | List runs in submission order; optional `session_id` query parameter to list only that session's |
| `GET /v1/runs/{id}` | Inspect status, prompt, session, deadline, cancellation request, and error |
| `POST /v1/runs/{id}/cancel` | Request cancellation; safe to repeat; returns current run |
| `GET /v1/runs/{id}/events` | SSE replay followed by live persisted events; optional `after` cursor or `Last-Event-ID` |

Omitting `session_id` creates a fresh session transactionally with submission.
A supplied session must already exist. Session IDs use the existing host rules:
up to 64 ASCII letters, digits, dots, underscores, or hyphens, starting with a
letter or digit. Prompts must contain non-whitespace text and be at most 256 KiB.
Deadlines range from 1 to 86400 seconds. Omitting `agent` selects `default`; an
unknown definition is rejected. Unknown submission fields are rejected.

Run records contain `id`, `session_id`, `agent`, `prompt`, `status`,
`created_at` (Unix milliseconds), `deadline_secs`, `cancel_requested`, and
nullable `error`. Session records contain `id` and `created_at`.

Each SSE frame has event name `run`, a durable integer `id`, and JSON data:

```json
{"id":42,"run_id":"...","data":{"kind":"text","text":"Hello"}}
```

Event IDs are database-wide, monotonically increasing cursors. A replay returns
only that run's events after the cursor, in order. The query cursor takes
precedence over `Last-Event-ID`. Streams end after all events for a terminal
run have been sent. The CLI reconnects to a dropped stream using its last
rendered cursor; restarting the daemon requires reconnecting the CLI with its
new connection descriptor.

Other event kinds are `status`, `usage`, `tool_started`, `tool_finished`,
`turn_saved`, and `cancellation_requested`. Tool-start events contain name and
input; tool-finish events contain name, success, `elapsed_ms`, output `bytes`,
and the tool's `error` (the same text the model receives). These records may
contain private task data, so they are stored inside the private service
directory.

## Roadmap

1. **Scheduling:** add durable one-off and cron schedules with explicit time
   zones and API/CLI management. Each occurrence creates an ordinary run in a
   fresh session. Unique occurrence IDs prevent duplicate enqueueing. Skip
   missed recurring occurrences and enqueue overdue one-off tasks once when
   the daemon returns. No wake-from-sleep or remote execution initially.
2. **Dynamic agents:** temporary specialists and constrained overrides of static
   definitions. Host-approved models, tools, and targets bound their authority;
   define lifetime, quotas, and persistence before implementation. Static
   delegation is implemented; see [delegation](delegation.md).
3. **Desktop:** use the same API for sessions, live output, run trees,
   cancellation, and scheduling. Desktop and CLI share a daemon. Choose the UI
   framework when implementation starts; it does not determine the runtime.

Remote serving, multi-user authorization, automatic retries, durable stack
checkpoints, and distributed workers are outside this milestone.

## Host activity logs

`runner serve` writes timestamped, readable diagnostic logs to stderr. The default
filter is `warn,host=info,runner=info`; set `RUST_LOG` to change it. Terminal output
uses color. Client and chat commands do not install a logging subscriber.

Run spans identify the run, session, and agent. Tool records identify the
configured tool name and call ID, outcome, elapsed time, and result byte count.
Provider spans identify the component; transcript spans record the operation
(`load` or `save`) and outcome. Outbound HTTP records identify the component,
request ID, method, scheme, host, port, and allow/deny decision. Allowed
requests report status and time to response headers, a transport-error category,
or interruption before headers. They do not measure response body completion.
API requests log method, matched route template, status, and time to headers,
including authentication rejections; unmatched routes use a fixed label.

These host-generated records omit prompts, tool arguments/results, raw errors,
headers, bodies, URL paths, and query strings. Existing SQLite events still serve
chat replay and may contain tool arguments and errors. Guest stdout/stderr is not
an activity log. Extra dependency diagnostics enabled through `RUST_LOG` have their
own content policies.

A bounded background writer (4096 queued lines) keeps slow stderr off Tokio
threads. Delivery is best effort: overload drops logs, and a crash can lose queued
records. Normal shutdown retains the writer guard to drain queued output. These
logs are diagnostic, not a durable audit trail; there are no log files, rotation,
or log-viewing command.

## Reattaching to session work

`runner chat --session SESSION` replays completed history, then shows and follows
pending turns in submission order before accepting input. Catch-up refreshes the
run list after each batch, including work submitted while attached. A followed
turn that fails or is cancelled is reported and catch-up continues; connection
errors stop catch-up rather than showing an idle prompt.

Chat remains turn-by-turn: use `runner run --session SESSION "prompt" --detach`
to queue additional work from another terminal. It does not continuously monitor
other clients' submissions while idle at the prompt. If new work arrives before
your next submission, the daemon serializes it and the watcher displays the queue
wait separately from thinking time. Indicator timings measure the local watch,
not historical execution.

Ctrl-C while following a run exits successfully without cancellation and prints
a normal detach notice and resume command. Use `runner runs cancel RUN_ID` to
cancel work. Session create/list responses retain `id` and `created_at` and add
numeric `running_runs` and `queued_runs` fields; creation returns zero counts.
List counts come from one database snapshot. No schema migration is required.
