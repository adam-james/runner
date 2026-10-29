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
similarly recreate the provider. Normal application errors, including an
unreadable transcript, retain state. Filesystem mounts remain shared according
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
transcript: its conversation as of its last completed turn, The service loads it
before a run executes and hands it to the host's turn loop, which returns the
conversation with the turn added; the host stores nothing itself. A transcript
that can't be read fails the turn with `could not load this session's history`,
and the underlying error is only logged. Submission and its initial event are
one transaction; terminal status and its event are another. For a completed run,
that same transaction also replaces the session's transcript and records
`turn_saved`, so a session's history includes exactly its completed runs: a run
that fails, is cancelled, misses its deadline, or is interrupted saves nothing,
and a crash can't leave a saved turn on a run that isn't completed. SQLite
operations run on blocking tasks, preserving `synchronous=FULL` durability
without blocking Tokio threads. Each active run queues output to a blocking
consumer; its final events drain before terminal status is committed. SSE
watchers wake on per-run commit notifications and replay from SQLite; active
cancellation uses an in-memory token after the durable request commits.

The queue executes the oldest eligible run, respecting global capacity and
one active run per session. Capacity defaults to four. Text is batched at 8 KiB
or about 50 ms; non-text events flush preceding text. Client reads use persisted
events rather than an in-memory broadcast channel, so a slow or disconnected
client cannot block execution or lose previously recorded output.

Run states are `queued`, `running`, `completed`, `failed`, `cancelled`, and
`interrupted`. Cancellation is a durable request; active runs become terminal
after execution is dropped. Queued cancellations take effect immediately. A turn
that returns before cancellation, shutdown, or its deadline takes effect
completes, even if cancellation was requested meanwhile; one that hasn't
returned by then saves nothing. Event persistence failures still fail the run.
Deadlines include component setup and all model/tool work, starting at claim.
Epoch yields make CPU-bound components interruptible; dropping a turn drops its
in-flight provider call. Tool-specific timeouts and fresh-instance settings
continue to apply.

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
| `GET /v1/agent-versions/{id}` | What runs of an agent version were allowed: `{"id", "agent", "first_seen_at", "definition"}`; 404 if no daemon using this data directory has loaded it |
| `POST /v1/sessions` | `{ "id": "optional-name", "agent": "default" }`; omitted/null ID generates one; returns session, HTTP 201 |
| `GET /v1/sessions` | List known sessions |
| `POST /v1/runs` | `{ "prompt": "...", "session_id": null, "agent": "default", "deadline_secs": 600, "notify": null }`; `notify` overrides when it notifies (`all`, `problems` or `never`); returns queued run, HTTP 202 |
| `GET /v1/runs` | List runs in submission order; optional `session_id` query parameter to list only that session's |
| `GET /v1/runs/{id}` | Inspect status, prompt, session, deadline, cancellation request, and error |
| `POST /v1/runs/{id}/cancel` | Request cancellation; safe to repeat; returns current run |
| `GET /v1/runs/{id}/events` | SSE replay followed by live persisted events; optional `after` cursor or `Last-Event-ID` |
| `GET /v1/runs/{id}/summary` | What the run and every run it delegated to did, made by the host from their audit records: `{"run_id", "agent", "schedule_id", "status", "alerts": [{"kind", "message"}], "runs", "unfinished_runs", "not_completed": [{"run_id", "status"}], "turns", "duration_ms", "actions", "hosts": [{"kind", "component", "host", "allowed", "denied"}], "credentials_withheld", "denials": {"total", "records"}, "errors": {"total", "records"}, "trail_problems"}`; at most 50 records listed each |
| `POST /v1/schedules` | `{ "id": "optional-name", "agent": "default", "prompt": "...", "at": "2026-10-29T07:00", "tz": "Europe/Warsaw", "deadline_secs": 600 }`, or `"in_secs": 2` (1 second to a year from now) or `"cron": "0 7 * * MON-FRI"` (recurring) instead of `at`; pins the agent's versions; returns schedule, HTTP 201 |
| `GET /v1/schedules` | List schedules that haven't been deleted |
| `GET /v1/schedules/{id}` | Inspect a schedule, deleted ones included |
| `GET /v1/schedules/{id}/history` | What was done to it and what it did, oldest first |
| `POST /v1/schedules/{id}/pause` | Pause an active schedule; cancels its occurrence if queued; 409 otherwise |
| `POST /v1/schedules/{id}/resume` | Resume a paused schedule; 409 if its agents changed |
| `POST /v1/schedules/{id}/approve` | Pin an active or paused schedule to its agents' current versions and make it active: `{"schedule", "changes": [{"agent", "from", "to", "fields"}]}` |
| `DELETE /v1/schedules/{id}` | Delete it; cancels its occurrence if queued; returns it |
| `POST /v1/schedules/{id}/notify` | `{"notify": "all" \| "problems" \| "never"}`: which of its runs notify; returns schedule. Creating a schedule takes `notify` too (default `problems`) |
| `GET /v1/notifications` | Newest first, at most `limit` (1–1000, default 100); `?unread=true` for unread only: `[{"id", "run_id", "created_at", "read_at", "schedule_id", "agent", "status", "duration_ms", "alerts", "schedule_status"}]`, with `schedule_id` and `schedule_status` null for a run outside a schedule |
| `POST /v1/notifications/{id}/read` | Mark it read: `{"marked": 1}`, or 0 if it already was; 404 if there's none |
| `POST /v1/notifications/read` | Mark every unread one read: `{"marked": n}` |

Omitting `session_id` creates a fresh session transactionally with submission.
A supplied session must already exist. Session IDs use the existing host rules:
up to 64 ASCII letters, digits, dots, underscores, or hyphens, starting with a
letter or digit. Prompts must contain non-whitespace text and be at most 256 KiB.
Deadlines range from 1 to 86400 seconds. Omitting `agent` selects `default`; an
unknown definition is rejected. Unknown submission fields are rejected.

Run records contain `id`, `session_id`, `agent`, `prompt`, `status`,
`created_at` (Unix milliseconds), `deadline_secs`, `cancel_requested`, and
nullable `error`, plus nullable `schedule_id` and `pinned_versions` for a
schedule's run. Session records contain `id`, `created_at`, and `agent` (required and fixed at creation). Runs with a different agent are rejected; an omitted agent resumes the owner or uses `default` for a new session.

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

The focus is background agents that are secure and auditable: nobody watches
a background run, so what it may do is fixed before it starts, anything beyond
that waits for an approval that defaults to deny, and the audit log is how you
find out what happened. Agents are the unit of capability; there's no
separate grant system. Each milestone ships something usable on its own;
[todo.md](../todo.md) tracks the details.

1. **Agent versions:** at the start of each run, the host snapshots the
   resolved agent definition: provider and model, each tool's allowed hosts,
   header names and the hosts they go to (never values), directory, limits,
   and component digest, plus delegates, flags, and a hash of the system
   prompt. The snapshot's hash is the agent version. The service stores each
   distinct version once and records it on the run and in its audit trail, so
   a run can be checked against what it was allowed after `runner.toml` has
   changed. Sessions stay locked to the agent name; each run records its
   version.
2. **Schedules pinned to an agent version:** done; see
   [Schedules](#schedules). Durable
   one-off and cron schedules with explicit time zones and API/CLI
   management. Each occurrence
   creates an ordinary run in a fresh session. Unique occurrence IDs prevent
   duplicate enqueueing. Skip missed recurring occurrences and enqueue overdue
   one-off tasks once when the daemon returns. No wake-from-sleep or remote
   execution initially. A schedule names an agent and pins the version it was
   created with; if the version changes, the schedule pauses until it's
   approved again.
3. **Run summaries, alerts, and notifications:** the host summarizes every background run
   from its audit records, without a model: hosts reached, denials, errors,
   trail gaps, and pending approvals. Summaries are done, served on demand
   (`GET /v1/runs/{id}/summary`); pending approvals join them with step 4. Denials, gaps, and hosts a schedule has
   never reached before raise alerts (done, in the summary), and only flagged runs go to the
   `run_auditor` agent. Audit records, and the agent versions they reference,
   are kept longer than their runs, since background runs are reviewed later.
   The user is notified in the foreground when a background run finishes,
   fails, or raises an alert, as a desktop notification and in the TUI; each
   schedule chooses every finish, only failures and alerts, or never.
   Notifications are durable records, so none are lost while no client is
   connected, and their text is written by the host (task, outcome, duration,
   flags), never taken from the model's reply, which a prompt injection could
   steer. Approval requests use the same channel.
4. **Asynchronous approvals:** tools declare side-effecting actions in their
   WIT interface. Such an action pauses its run durably, surviving restarts,
   until it's approved or denied from the CLI, TUI, or a remote client.
   Approvals expire, and an expired approval is a denial. Agents may hold
   standing approvals for routine, narrow actions, as part of their version.
   Every approval, denial, and expiry is audited.
5. **Desktop:** use the same API for sessions, live output, run trees,
   cancellation, schedules, summaries, notifications, and approvals. Desktop
   and CLI share a daemon. Choose the UI framework when implementation starts;
   it does not determine the runtime.

Deferred: dynamic agents (temporary specialists and overrides of static
definitions created at runtime), since a background agent that defines new
agents is hard to audit; static delegation fits the focus (see
[delegation](delegation.md)). If revisited, host-approved models, tools, and
targets bound their authority, and lifetime, quotas, and persistence are
defined before implementation. Memory across a schedule's runs waits for the
milestones above, since it's how a prompt injection would persist.

Remote serving, multi-user authorization, automatic retries, durable stack
checkpoints, and distributed workers are outside these milestones.

## Schedules

A **schedule** runs a prompt as an ordinary run in a fresh session locked to
its agent: once, at a local time in an explicit IANA time zone or a delay from
now (`in_secs`, e.g. for testing), whose local time is then stored to the
second; or on a five-field cron expression read in the zone. A one-off's time
is stored as given, with the zone, and as the instant it names; a local time
that a clock change skips or repeats is refused. A recurring schedule's `at`
and `next_fire_at` are its next occurrence. Schedule ids follow session ids'
rules. Statuses are `active`, `paused`, `done` (a one-off that fired, or a
recurring schedule with no occurrences left) and `deleted`.

Cron expressions are parsed and stepped by `croner`, whose clock change rules
are Vixie cron's: in a spring-forward gap a fixed-time job (one whose minute
and hour are single values) runs right after the gap and an interval job skips
the gap; in a fall-back overlap a fixed-time job runs once and an interval job
runs in both passes. Zone rules come from `chrono-tz`, built into the binary,
so a change to them needs a rebuild.

A schedule is pinned to agent versions: its agent's and those of every agent
it can reach by delegating, transitively, since a version names delegates
only by name (`host::version::pin`). The daemon stores every agent's current
version when it starts, so both what a schedule was approved for and what it
would run with now can be read with `runner agents version`, even after the
config changes. Because the config only changes when the daemon restarts, the
pin is checked:

- **at startup**, which pauses every active schedule whose pin no longer
  matches, so `schedules list` shows it before it's due;
- **when it falls due**, which pauses it instead of enqueuing a run;
- **when its run starts**, since a run queued before a restart would
  otherwise start under the new config. The run fails with the agents that
  changed and its schedule pauses. A one-off's occurrence is given back: a
  refused run never executed, so approving the schedule makes it fire again.
  A recurring schedule keeps its next occurrence instead.

Delegated child runs aren't checked separately: their root was checked when
it started, and no version can change while it runs.

A paused schedule resumes only if its pin still matches; otherwise it has to
be approved, which re-pins it and reports which agents and which snapshot
fields changed. Pausing or deleting a schedule cancels its occurrence if it's
queued, not if it's running.

The scheduler looks for due schedules every second. Each fires in one
transaction that creates the session and run, records `fired` in the
schedule's history, and marks a one-off `done` or moves a recurring schedule
to its next occurrence after now. Occurrence ids
(`<schedule id>@<scheduled Unix ms>`) are unique, so an occurrence is never
enqueued twice. A one-off that fell due while the daemon was stopped fires
once when it starts. A recurring schedule doesn't make up for lost time: an
occurrence more than a minute late is skipped as `missed`, and one whose
previous run is still queued or running is skipped as
`previous run unfinished`, so its runs never overlap. A paused schedule that's
resumed or approved after its next occurrence passed skips it the same way.
There is no wake from sleep. A schedule's history records `created`,
`paused`, `resumed`, `approved`, `fired`, `skipped`, `refused`,
`notify_changed` and `deleted`, with the reason, runs and versions involved;
it's metadata only, like audit records.

## Notifications

The daemon's notifier looks every second for top-level runs that have ended
and whose delegation tree has ended too, so the summary is complete, and
checks each once. Every top-level run counts, scheduled or not: the daemon
assumes nobody is watching, since a client can be attached while nobody looks
and alerts never show in chat. A level decides whether the run gets a
notification: `all`, `problems` (any status but `completed`, i.e. `failed`,
`interrupted` or `cancelled`, for the run or any run it delegated to, or any
summary alert) or `never`. The run's own level
(`notify` on `POST /v1/runs`) wins, then its schedule's, then `problems`.
Delegated runs aren't checked separately; their root's summary covers them. A
notification stores only host-written fields: the schedule (if any), agent,
status, duration, the summary's alerts and the schedule's status (e.g.
`paused` after a refused run). It never
holds the model's reply or the run's error, which a prompt injection could
steer. Notifications stay until read. Runs that ended before notifications
existed count as checked, so upgrading notifies about nothing old.

After checking, the notifier posts each notification it hasn't yet to the
desktop (unless `[notifications] desktop = false`), skipping ones already
read, and marks it posted (`delivered_at`) whether or not posting worked: it
stays stored either way, and a desktop that can't show notifications would
only fail again. More than three waiting at once are posted as one. A failed
post is logged once until posting works again. Notifications from before
desktop posting existed count as posted. Posting is in-process, through the
`notify-rust` crate: Notification Center on macOS (posting as Terminal, set
explicitly so the crate doesn't look an app up with AppleScript) or the
freedesktop D-Bus interface on Linux; no command is run. On macOS the crate
doesn't report a failed post, so nothing is logged there.

## Host activity logs

`runner serve` writes timestamped, readable diagnostic logs to stderr. The default
filter is `warn,host=info,runner=info`; set `RUST_LOG` to change it. Terminal output
uses color. Client and chat commands do not install a logging subscriber.

Run spans identify the run, session, and agent. Tool records identify the
configured tool name and call ID, outcome, elapsed time, and result byte count.
Provider spans identify the component. Outbound HTTP records identify the
component, request ID, method, scheme, host, port, and allow/deny decision.
Allowed requests report status and time to response headers, a transport-error
category, or interruption before headers. They do not measure response body
completion. API requests log method, matched route template, status, and time to
headers, including authentication rejections; unmatched routes use a fixed
label.

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
