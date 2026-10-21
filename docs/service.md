# Local agent service

The native service is the execution authority. Clients submit work and watch
persisted events; they never own agent lifetimes or receive provider credentials.
Agents, providers, tools, and session stores remain WASM components with the
existing capability restrictions. The CLI contains the service's terminal UI.
The agent has one structured turn interface and no terminal mode or CLI entry
point; interactive use goes through `runner chat`.

## Execution and persistence

An **agent definition** selects the agent component, provider, model, and tools.
This milestone exposes the existing config as the single `default` definition.
A **session** identifies a conversation. A **run** performs one turn within a
session and has a durable ID, prompt, deadline, status, and event history.

`host::Runtime` shares its Wasmtime engine and compiled components. Each run
creates independent stores and component instances, including providers and
tools; a run never waits for another run's provider mutex. Filesystem mounts
remain shared according to their configured paths and permissions. Separate
execution stores do not isolate changes made to a shared writable directory.

The guest exports `agent-api.run-turn(prompt)` and imports typed `run-events`
for text and usage. It retains its model/tool loop and session load/save calls.
The host records tool execution and lifecycle transitions itself. Terminal
formatting is not part of the service's component protocol. The host captures
guest stdout/stderr separately, up to 64 KiB each, and emits them as
diagnostics when execution ends. Raw component/provider errors are not published
to clients because they may include reflected request data.

SQLite stores sessions, submitted runs, and ordered events. The existing
session-store component stores completed conversation turns. Submission and its
initial event are one transaction; terminal status and its event are another.
A transcript write is not in the same transaction as operational state.

The queue executes the oldest eligible run, respecting global capacity and
one active run per session. Capacity defaults to four. Text is batched at 8 KiB
or about 50 ms; non-text events flush preceding text. Client reads use persisted
events rather than an in-memory broadcast channel, so a slow or disconnected
client cannot block execution or lose previously recorded output.

Run states are `queued`, `running`, `completed`, `failed`, `cancelled`, and
`interrupted`. Cancellation is a durable request; active runs become terminal
after execution is dropped. Queued cancellations take effect immediately.
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

The daemon imports existing session IDs at startup. Do not run a second daemon
with a different data directory against the same writable
session store at the same time: the service's session serialization only covers
its own queue. Changing a daemon's config requires restarting it; queued work
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
| `POST /v1/sessions` | `{ "id": "optional-name" }`; omitted/null ID generates one; returns session, HTTP 201 |
| `GET /v1/sessions` | List known sessions |
| `POST /v1/runs` | `{ "prompt": "...", "session_id": null, "deadline_secs": 600 }`; returns queued run, HTTP 202 |
| `GET /v1/runs` | List runs in submission order |
| `GET /v1/runs/{id}` | Inspect status, prompt, session, deadline, cancellation request, and error |
| `POST /v1/runs/{id}/cancel` | Request cancellation; safe to repeat; returns current run |
| `GET /v1/runs/{id}/events` | SSE replay followed by live persisted events; optional `after` cursor or `Last-Event-ID` |

Omitting `session_id` creates a fresh session transactionally with submission.
A supplied session must already exist. Session IDs use the existing host rules:
up to 64 ASCII letters, digits, dots, underscores, or hyphens, starting with a
letter or digit. Prompts must contain non-whitespace text and be at most 256 KiB.
Deadlines range from 1 to 86400 seconds. Unknown submission fields are rejected.

Run records contain `id`, `session_id`, `agent` (`default`), `prompt`, `status`,
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
`diagnostic`, and `cancellation_requested`. Tool-start events contain name and
input; tool-finish events contain name and success. Diagnostic events contain
source (`stdout`/`stderr`) and text. These records may contain private task data,
so they are stored inside the private service directory.

## Roadmap

1. **Scheduling:** add durable one-off and cron schedules with explicit time
   zones and API/CLI management. Each occurrence creates an ordinary run in a
   fresh session. Unique occurrence IDs prevent duplicate enqueueing. Skip
   missed recurring occurrences and enqueue overdue one-off tasks once when
   the daemon returns. No wake-from-sleep or remote execution initially.
2. **Explicit delegation:** add named agent definitions and an async host
   capability to submit child tasks and receive results. Persist parent-child
   links. Children use separate sessions and their target definition's
   capabilities. Enforce target permissions, deadlines, and depth limits;
   parents awaiting children release execution capacity to prevent deadlocks.
   Broader persistent mailboxes and workflow graphs follow only when needed.
3. **Desktop:** use the same API for sessions, live output, run trees,
   cancellation, and scheduling. Desktop and CLI share a daemon. Choose the UI
   framework when implementation starts; it does not determine the runtime.

Remote serving, multi-user authorization, automatic retries, durable stack
checkpoints, and distributed workers are outside this milestone.
