use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use host::version::Pin;

#[derive(Clone)]
pub struct Database(Arc<Mutex<Connection>>, Arc<Signals>);

struct Signals {
    changes: Mutex<HashMap<String, Weak<Notify>>>,
    cancellations: Mutex<HashMap<String, CancellationToken>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    /// Set at session creation; never changes.
    pub agent: String,
    pub id: String,
    pub created_at: i64,
    #[serde(default)]
    pub running_runs: u64,
    #[serde(default)]
    pub queued_runs: u64,
    /// The first line of the session's first prompt.
    #[serde(default)]
    pub title: Option<String>,
    /// When the session, or its latest run, was created.
    #[serde(default)]
    pub updated_at: i64,
    /// Its latest run's status.
    #[serde(default)]
    pub last_status: Option<String>,
    /// Created for a delegated child run rather than by a client.
    #[serde(default)]
    pub delegated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub session_id: String,
    pub agent: String,
    pub prompt: String,
    pub status: String,
    pub created_at: i64,
    pub deadline_secs: u64,
    pub cancel_requested: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub parent_run_id: Option<String>,
    #[serde(default)]
    pub root_run_id: String,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub deadline_at: Option<i64>,
    /// The schedule whose occurrence this run is.
    #[serde(default)]
    pub schedule_id: Option<String>,
    /// For a scheduled run, the agent versions its schedule was approved
    /// for. It runs only if they're still what the daemon would use.
    #[serde(default)]
    pub pinned_versions: Option<Pin>,
}

impl Run {
    pub fn terminal(&self) -> bool {
        is_terminal(&self.status)
    }
}

pub fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "interrupted")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub run_id: String,
    pub data: Value,
}

/// An agent definition's version: what runs of it were allowed. See
/// `host::version`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentVersion {
    pub id: String,
    pub agent: String,
    /// When a daemon first loaded it, or a run first used it.
    pub first_seen_at: i64,
    pub definition: Value,
}

/// A task to run once at a set time, as its agent was when it was approved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub agent: String,
    pub prompt: String,
    /// The local time it fires at, in `tz`, e.g. `2026-10-29T07:00:00`.
    pub at: String,
    /// An IANA time zone name, e.g. `Europe/Warsaw`.
    pub tz: String,
    /// When it next fires (Unix milliseconds); `None` once it has fired or
    /// been deleted.
    pub next_fire_at: Option<i64>,
    pub deadline_secs: u64,
    /// `active`, `paused`, `done` (fired) or `deleted`.
    pub status: String,
    /// What its runs may use; see `host::version::pin`.
    pub pinned_versions: Pin,
    pub paused_reason: Option<String>,
    pub created_at: i64,
    /// Agents whose version is no longer what it's pinned to. Worked out
    /// by the service against its current config, not stored.
    #[serde(default)]
    pub changed_agents: Vec<String>,
}

/// Why a scheduled occurrence was refused or its schedule paused, shown to
/// clients. Names only agents, which come from the config.
pub fn version_change_reason(changed: &[String]) -> String {
    format!(
        "agent versions changed since approval: {}",
        changed.join(", ")
    )
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn read_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: row.get(0)?,
        session_id: row.get(1)?,
        agent: row.get(2)?,
        prompt: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
        deadline_secs: row.get(6)?,
        cancel_requested: row.get(7)?,
        error: row.get(8)?,
        parent_run_id: row.get(9)?,
        root_run_id: row.get(10)?,
        depth: row.get(11)?,
        deadline_at: row.get(12)?,
        schedule_id: row.get(13)?,
        pinned_versions: row
            .get::<_, Option<String>>(14)?
            .map(|text| from_json(14, &text))
            .transpose()?,
    })
}

const RUN_COLUMNS: &str = "id, session_id, agent, prompt, status, created_at, deadline_secs, cancel_requested, error, parent_run_id, root_run_id, depth, deadline_at, schedule_id, pinned_versions";

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
        params![table, column],
        |row| row.get(0),
    )?)
}

fn from_json<T: serde::de::DeserializeOwned>(column: usize, text: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn read_schedule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: row.get(0)?,
        agent: row.get(1)?,
        prompt: row.get(2)?,
        at: row.get(3)?,
        tz: row.get(4)?,
        next_fire_at: row.get(5)?,
        deadline_secs: row.get(6)?,
        status: row.get(7)?,
        pinned_versions: from_json(8, &row.get::<_, String>(8)?)?,
        paused_reason: row.get(9)?,
        created_at: row.get(10)?,
        changed_agents: Vec::new(),
    })
}

const SCHEDULE_COLUMNS: &str = "id, agent, prompt, at, tz, next_fire_at, deadline_secs, status, pinned_versions, paused_reason, created_at";

/// Appends to a schedule's history: what was done to it and what it did.
fn schedule_event(tx: &Connection, id: &str, data: Value) -> Result<()> {
    tx.execute(
        "INSERT INTO schedule_events (schedule_id, created_at, data) VALUES (?1, ?2, ?3)",
        params![id, now(), data.to_string()],
    )?;
    Ok(())
}

/// Stores each of `versions` that isn't stored yet.
fn store_versions(tx: &Connection, versions: &[host::version::AgentVersion]) -> Result<()> {
    for version in versions {
        tx.execute(
            "INSERT OR IGNORE INTO agent_versions (id, agent, first_seen_at, definition)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                version.id,
                version.definition["agent"].as_str().unwrap_or_default(),
                now(),
                version.definition.to_string()
            ],
        )?;
    }
    Ok(())
}

/// Cancels a schedule's occurrences that haven't started, so pausing or
/// deleting it stops them too. Returns their ids.
fn cancel_queued_occurrences(tx: &Connection, schedule: &str) -> Result<Vec<String>> {
    let ids = {
        let mut stmt =
            tx.prepare("SELECT id FROM runs WHERE schedule_id=?1 AND status='queued'")?;
        stmt.query_map([schedule], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in &ids {
        tx.execute(
            "UPDATE runs SET cancel_requested=1, status='cancelled' WHERE id=?1",
            [id],
        )?;
        append_event(tx, id, json!({"kind":"cancellation_requested"}))?;
        append_event(tx, id, json!({"kind":"status","status":"cancelled"}))?;
    }
    Ok(ids)
}

fn upsert_transcript(tx: &Connection, session_id: &str, messages: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO transcripts (session_id, messages, updated_at) VALUES (?1, ?2, ?3)
        ON CONFLICT(session_id) DO UPDATE SET messages=excluded.messages,
            updated_at=excluded.updated_at",
        params![session_id, messages, now()],
    )?;
    Ok(())
}

fn append_event(tx: &Connection, id: &str, data: Value) -> Result<()> {
    tx.execute(
        "INSERT INTO events (run_id, data) VALUES (?1, ?2)",
        params![id, data.to_string()],
    )?;
    Ok(())
}

/// SQLite gives the WAL and SHM files the database file's mode, so a private
/// database keeps its sidecars private regardless of the umask or directory.
/// Sidecars left by an older version are tightened too.
#[cfg(unix)]
fn make_private(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let file = std::path::Path::new(&file);
        match std::fs::metadata(file) {
            Ok(meta) if meta.permissions().mode() & 0o077 != 0 => {
                std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
                    .with_context(|| format!("making {} private", file.display()))?;
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("checking {}", file.display())),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_private(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

impl Database {
    /// Keep SQLite locks, busy waits and durable commits off the async runtime.
    pub async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce(Database) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let db = self.clone();
        tokio::task::spawn_blocking(move || operation(db)).await?
    }

    /// Subscriptions are shared per run and disappear when their watchers disconnect.
    pub fn changes(&self, id: &str) -> Arc<Notify> {
        let mut changes = self.1.changes.lock().unwrap();
        changes.retain(|_, signal| signal.strong_count() > 0);
        if let Some(signal) = changes.get(id).and_then(Weak::upgrade) {
            return signal;
        }
        let signal = Arc::new(Notify::new());
        changes.insert(id.to_owned(), Arc::downgrade(&signal));
        signal
    }

    fn notify(&self, id: &str) {
        let mut changes = self.1.changes.lock().unwrap();
        if let Some(signal) = changes.get(id).and_then(Weak::upgrade) {
            signal.notify_waiters();
        } else {
            changes.remove(id);
        }
    }

    pub fn cancellation(&self, id: &str) -> CancellationToken {
        self.1
            .cancellations
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .expect("claimed run has a token")
    }

    pub fn open(path: &std::path::Path) -> Result<Self> {
        make_private(path)?;
        Self::from_connection(Connection::open(path)?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        let version = conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
        if version > 7 {
            bail!("service database was created by a newer runner version");
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, created_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                agent TEXT NOT NULL, prompt TEXT NOT NULL, status TEXT NOT NULL,
                created_at INTEGER NOT NULL, deadline_secs INTEGER NOT NULL,
                cancel_requested INTEGER NOT NULL DEFAULT 0, error TEXT);
            CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL REFERENCES runs(id), data TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS events_by_run ON events(run_id, id);
            CREATE INDEX IF NOT EXISTS queued_runs ON runs(status, created_at);
            CREATE INDEX IF NOT EXISTS runs_by_session ON runs(session_id, created_at);
            -- Each session's conversation, as the host's turn loop encodes it. Not a
            -- foreign key: the runtime can be driven directly, without the queue.
            CREATE TABLE IF NOT EXISTS transcripts (session_id TEXT PRIMARY KEY,
                messages TEXT NOT NULL, updated_at INTEGER NOT NULL);
            ",
        )?;
        if version < 3 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "ALTER TABLE runs ADD COLUMN parent_run_id TEXT REFERENCES runs(id);
                 ALTER TABLE runs ADD COLUMN root_run_id TEXT NOT NULL DEFAULT '';
                 ALTER TABLE runs ADD COLUMN depth INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE runs ADD COLUMN deadline_at INTEGER;
                 UPDATE runs SET root_run_id=id;
                 CREATE INDEX runs_by_parent ON runs(parent_run_id);
                 CREATE INDEX runs_by_root ON runs(root_run_id);
                 CREATE TABLE run_results (
                     run_id TEXT PRIMARY KEY REFERENCES runs(id), output TEXT NOT NULL);
                 PRAGMA user_version=3;",
            )?;
            tx.commit()?;
        }
        if version < 4 {
            let tx = conn.unchecked_transaction()?;
            // Security audit records, which the host emits as `audit` events.
            // Apart from the run's own event stream, so clients following a
            // run never see them; kept for as long as the run is.
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS audit_events (id INTEGER PRIMARY KEY AUTOINCREMENT,
                     run_id TEXT NOT NULL REFERENCES runs(id), data TEXT NOT NULL);
                 CREATE INDEX IF NOT EXISTS audit_events_by_run ON audit_events(run_id, id);
                 PRAGMA user_version=4;",
            )?;
            tx.commit()?;
        }
        if version < 5 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "ALTER TABLE sessions ADD COLUMN agent TEXT NOT NULL DEFAULT 'default';
                 UPDATE sessions SET agent=COALESCE((SELECT agent FROM runs
                     WHERE session_id=sessions.id ORDER BY created_at DESC, rowid DESC LIMIT 1), 'default');
                 PRAGMA user_version=5;",
            )?;
            // Old queued turns must not execute under another agent's history.
            let mismatches = {
                let mut stmt = tx.prepare(
                    "SELECT r.id FROM runs r JOIN sessions s ON s.id=r.session_id
                    WHERE r.status='queued' AND r.agent!=s.agent",
                )?;
                stmt.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for id in mismatches {
                let error =
                    "session is locked to another agent after upgrade; create a new session";
                tx.execute(
                    "UPDATE runs SET status='failed', error=?2 WHERE id=?1",
                    params![id, error],
                )?;
                append_event(
                    &tx,
                    &id,
                    json!({"kind":"status","status":"failed","error":error}),
                )?;
            }
            tx.commit()?;
        }
        if version < 6 {
            let tx = conn.unchecked_transaction()?;
            // What each run was allowed, kept apart from the config, which
            // can change after the run. A version is stored once however many
            // runs use it, and never changes: its id is its snapshot's hash.
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS agent_versions (id TEXT PRIMARY KEY,
                     agent TEXT NOT NULL, first_used_at INTEGER NOT NULL,
                     definition TEXT NOT NULL);",
            )?;
            let linked: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runs') WHERE name='agent_version')",
                [],
                |row| row.get(0),
            )?;
            if !linked {
                tx.execute_batch(
                    "ALTER TABLE runs ADD COLUMN agent_version TEXT REFERENCES agent_versions(id);",
                )?;
            }
            tx.execute_batch("PRAGMA user_version=6;")?;
            tx.commit()?;
        }
        if version < 7 {
            let tx = conn.unchecked_transaction()?;
            // Schedules: tasks that run later, pinned to the agent versions
            // they were approved for. Versions are now also stored when a
            // schedule is approved, before any run uses them. Occurrence ids
            // are unique, so no occurrence is ever enqueued twice. Like the
            // previous step, this tolerates what it adds already existing.
            if has_column(&tx, "agent_versions", "first_used_at")? {
                tx.execute_batch(
                    "ALTER TABLE agent_versions RENAME COLUMN first_used_at TO first_seen_at;",
                )?;
            }
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS schedules (id TEXT PRIMARY KEY, agent TEXT NOT NULL,
                     prompt TEXT NOT NULL, at TEXT NOT NULL, tz TEXT NOT NULL,
                     next_fire_at INTEGER, deadline_secs INTEGER NOT NULL,
                     status TEXT NOT NULL, pinned_versions TEXT NOT NULL,
                     paused_reason TEXT, created_at INTEGER NOT NULL);
                 CREATE INDEX IF NOT EXISTS due_schedules ON schedules(status, next_fire_at);
                 CREATE TABLE IF NOT EXISTS schedule_events (id INTEGER PRIMARY KEY AUTOINCREMENT,
                     schedule_id TEXT NOT NULL REFERENCES schedules(id),
                     created_at INTEGER NOT NULL, data TEXT NOT NULL);
                 CREATE INDEX IF NOT EXISTS schedule_events_by_schedule
                     ON schedule_events(schedule_id, id);",
            )?;
            for (column, definition) in [
                ("schedule_id", "TEXT REFERENCES schedules(id)"),
                ("occurrence_id", "TEXT"),
                ("pinned_versions", "TEXT"),
            ] {
                if !has_column(&tx, "runs", column)? {
                    tx.execute_batch(&format!(
                        "ALTER TABLE runs ADD COLUMN {column} {definition};"
                    ))?;
                }
            }
            tx.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS runs_by_occurrence ON runs(occurrence_id);
                 PRAGMA user_version=7;",
            )?;
            tx.commit()?;
        }
        Ok(Self(
            Arc::new(Mutex::new(conn)),
            Arc::new(Signals {
                changes: Mutex::new(HashMap::new()),
                cancellations: Mutex::new(HashMap::new()),
            }),
        ))
    }

    pub fn create_session(&self, id: Option<String>, agent: String) -> Result<Session> {
        let id = id.unwrap_or_else(new_id);
        host::check_session_id(&id)?;
        let created_at = now();
        let session = Session {
            agent,
            id,
            created_at,
            updated_at: created_at,
            ..Session::default()
        };
        self.0.lock().unwrap().execute(
            "INSERT INTO sessions (id, created_at, agent) VALUES (?1, ?2, ?3)",
            params![session.id, session.created_at, session.agent],
        )?;
        Ok(session)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.created_at,
                COUNT(CASE WHEN r.status='running' THEN 1 END),
                COUNT(CASE WHEN r.status='queued' THEN 1 END),
                (SELECT substr(prompt, 1, 200) FROM runs WHERE session_id=s.id
                    ORDER BY created_at, rowid LIMIT 1),
                max(s.created_at, COALESCE(MAX(r.created_at), 0)),
                (SELECT status FROM runs WHERE session_id=s.id
                    ORDER BY created_at DESC, rowid DESC LIMIT 1),
                EXISTS (SELECT 1 FROM runs WHERE session_id=s.id AND parent_run_id IS NOT NULL),
                s.agent
             FROM sessions s LEFT JOIN runs r ON r.session_id=s.id
             GROUP BY s.id ORDER BY s.created_at, s.rowid",
        )?;
        Ok(stmt
            .query_map([], |row| {
                Ok(Session {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    running_runs: row.get(2)?,
                    queued_runs: row.get(3)?,
                    title: row.get::<_, Option<String>>(4)?.and_then(|prompt| {
                        let line = prompt.lines().map(str::trim).find(|l| !l.is_empty())?;
                        Some(line.to_string())
                    }),
                    updated_at: row.get(5)?,
                    last_status: row.get(6)?,
                    delegated: row.get(7)?,
                    agent: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn submit(
        &self,
        session_id: Option<String>,
        agent: String,
        prompt: String,
        deadline_secs: u64,
    ) -> Result<Run> {
        if prompt.trim().is_empty() || prompt.len() > 256 * 1024 {
            bail!("prompt must contain 1–262144 bytes");
        }
        if !(1..=86400).contains(&deadline_secs) {
            bail!("deadline_secs must be between 1 and 86400");
        }
        if let Some(id) = &session_id {
            host::check_session_id(id)?;
        }
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let session_id = match session_id {
            Some(id) => {
                if !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                    [&id],
                    |r| r.get::<_, bool>(0),
                )? {
                    bail!("session does not exist; create it first");
                }
                id
            }
            None => {
                let id = new_id();
                tx.execute(
                    "INSERT INTO sessions (id, created_at, agent) VALUES (?1, ?2, ?3)",
                    params![id, now(), agent],
                )?;
                id
            }
        };
        let owner: String = tx.query_row(
            "SELECT agent FROM sessions WHERE id=?1",
            [&session_id],
            |r| r.get(0),
        )?;
        if owner != agent {
            bail!(
                "session '{session_id}' is locked to agent '{owner}'; create a new session to use agent '{agent}'"
            );
        }
        let id = new_id();
        let run = Run {
            id: id.clone(),
            parent_run_id: None,
            root_run_id: id,
            depth: 0,
            deadline_at: None,
            schedule_id: None,
            pinned_versions: None,
            session_id,
            agent,
            prompt,
            status: "queued".into(),
            created_at: now(),
            deadline_secs,
            cancel_requested: false,
            error: None,
        };
        tx.execute("INSERT INTO runs (id, session_id, agent, prompt, status, created_at, deadline_secs, root_run_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?1)",
            params![run.id, run.session_id, run.agent, run.prompt, run.status, run.created_at, run.deadline_secs])?;
        append_event(&tx, &run.id, json!({"kind":"status", "status":"queued"}))?;
        tx.commit()?;
        self.notify(&run.id);
        Ok(run)
    }

    pub fn run(&self, id: &str) -> Result<Option<Run>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id=?1"),
                [id],
                read_run,
            )
            .optional()?)
    }

    /// Runs in submission order, optionally only those of one session.
    pub fn runs(&self, session_id: Option<&str>) -> Result<Vec<Run>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE ?1 IS NULL OR session_id=?1
            ORDER BY created_at, rowid"
        ))?;
        Ok(stmt
            .query_map([session_id], read_run)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Claim the oldest runnable job. One writer per session, including queued turns.
    pub fn claim(&self) -> Result<Option<Run>> {
        self.claim_for_worker(true)
    }

    pub fn claim_for_worker(&self, allow_roots: bool) -> Result<Option<Run>> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let run = tx.query_row(&format!("SELECT {RUN_COLUMNS} FROM runs AS candidate
            WHERE status='queued' AND (parent_run_id IS NOT NULL OR ?1)
            AND (parent_run_id IS NULL OR EXISTS (
                SELECT 1 FROM runs parent WHERE parent.id=candidate.parent_run_id
                AND parent.status='running' AND parent.cancel_requested=0))
            AND NOT EXISTS (
                SELECT 1 FROM runs AS active WHERE active.session_id=candidate.session_id AND active.status='running')
            ORDER BY (parent_run_id IS NULL), created_at, rowid LIMIT 1"), [allow_roots], read_run).optional()?;
        let Some(mut run) = run else {
            return Ok(None);
        };
        let deadline_at = run
            .deadline_at
            .unwrap_or_else(|| now() + run.deadline_secs as i64 * 1000);
        tx.execute(
            "UPDATE runs SET status='running', deadline_at=?2 WHERE id=?1",
            params![run.id, deadline_at],
        )?;
        run.deadline_at = Some(deadline_at);
        append_event(&tx, &run.id, json!({"kind":"status", "status":"running"}))?;
        tx.commit()?;
        self.notify(&run.id);
        self.1
            .cancellations
            .lock()
            .unwrap()
            .insert(run.id.clone(), CancellationToken::new());
        run.status = "running".into();
        Ok(Some(run))
    }

    pub fn event(&self, id: &str, data: Value) -> Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        append_event(&tx, id, data)?;
        tx.commit()?;
        self.notify(id);
        Ok(())
    }

    /// Stores each of `versions` that isn't stored yet: the daemon's
    /// current ones at startup, so each can be read before anything uses it.
    pub fn store_agent_versions(&self, versions: &[host::version::AgentVersion]) -> Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        store_versions(&tx, versions)?;
        tx.commit()?;
        Ok(())
    }

    /// Stores agent version `version` (if it's new) and links run `id` to it.
    pub fn record_agent_version(
        &self,
        id: &str,
        version: &str,
        agent: &str,
        definition: &Value,
    ) -> Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO agent_versions (id, agent, first_seen_at, definition)
             VALUES (?1, ?2, ?3, ?4)",
            params![version, agent, now(), definition.to_string()],
        )?;
        tx.execute(
            "UPDATE runs SET agent_version=?2 WHERE id=?1",
            params![id, version],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn agent_version(&self, id: &str) -> Result<Option<AgentVersion>> {
        let conn = self.0.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT agent, first_seen_at, definition FROM agent_versions WHERE id=?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(agent, first_seen_at, definition)| {
            Ok(AgentVersion {
                id: id.to_string(),
                agent,
                first_seen_at,
                definition: serde_json::from_str(&definition)?,
            })
        })
        .transpose()
    }

    /// The agent version run `id` ran with, once it has started.
    pub fn run_agent_version(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT agent_version FROM runs WHERE id=?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn audit_event(&self, id: &str, data: Value) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO audit_events (run_id, data) VALUES (?1, ?2)",
            params![id, data.to_string()],
        )?;
        Ok(())
    }

    /// A run's audit records, oldest first. A turn records a bounded number,
    /// so this is never more than a few thousand.
    pub fn audit(&self, id: &str) -> Result<Vec<Event>> {
        let conn = self.0.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, data FROM audit_events WHERE run_id=?1 ORDER BY id")?;
        let rows = stmt.query_map(params![id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (seq, data) = row?;
            Ok(Event {
                id: seq,
                run_id: id.to_string(),
                data: serde_json::from_str(&data)?,
            })
        })
        .collect()
    }

    /// Whether any run in `session_id` belongs to the delegation tree rooted
    /// at `root_run_id`.
    pub fn session_in_tree(&self, session_id: &str, root_run_id: &str) -> Result<bool> {
        Ok(self.0.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND root_run_id=?2)",
            params![session_id, root_run_id],
            |row| row.get(0),
        )?)
    }

    /// The audit records of every run in a session, a run at a time in the
    /// order they started, each oldest first. `None` if there's no such
    /// session.
    pub fn session_audit(&self, session_id: &str) -> Result<Option<Vec<Event>>> {
        let conn = self.0.lock().unwrap();
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            params![session_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        // Runs are ordered by their first record rather than `created_at`,
        // which only has second resolution.
        let mut stmt = conn.prepare(
            "SELECT a.id, a.run_id, a.data FROM audit_events a JOIN runs r ON r.id=a.run_id
             WHERE r.session_id=?1
             ORDER BY (SELECT MIN(id) FROM audit_events WHERE run_id=a.run_id), a.id",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (seq, run_id, data) = row?;
            Ok(Event {
                id: seq,
                run_id,
                data: serde_json::from_str(&data)?,
            })
        })
        .collect::<Result<_>>()
        .map(Some)
    }

    pub fn events(&self, id: &str, after: i64) -> Result<Vec<Event>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, data FROM events WHERE run_id=?1 AND id>?2 ORDER BY id LIMIT 256",
        )?;
        let rows = stmt.query_map(params![id, after], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (seq, data) = row?;
            Ok(Event {
                id: seq,
                run_id: id.to_string(),
                data: serde_json::from_str(&data)?,
            })
        })
        .collect()
    }

    /// Atomically create a fresh child session, run, and parent relationship.
    pub fn delegate(
        &self,
        parent_id: &str,
        agent: &str,
        prompt: &str,
        deadline_secs: Option<u64>,
    ) -> Result<Run> {
        if prompt.trim().is_empty() || prompt.len() > 256 * 1024 {
            bail!("prompt must contain 1–262144 bytes");
        }
        if deadline_secs.is_some_and(|value| !(1..=86400).contains(&value)) {
            bail!("invalid child deadline");
        }
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let parent = tx.query_row(
            &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id=?1"),
            [parent_id],
            read_run,
        )?;
        if parent.status != "running" || parent.cancel_requested {
            bail!("parent is no longer running");
        }
        if parent.depth >= 3 {
            bail!("delegation depth limit reached");
        }
        let children: u32 = tx.query_row(
            "SELECT COUNT(*) FROM runs WHERE parent_run_id=?1",
            [parent_id],
            |r| r.get(0),
        )?;
        let descendants: u32 = tx.query_row(
            "SELECT COUNT(*) FROM runs WHERE root_run_id=?1 AND id!=?1",
            [&parent.root_run_id],
            |r| r.get(0),
        )?;
        if children >= 16 || descendants >= 64 {
            bail!("delegation child limit reached");
        }
        let timestamp = now();
        let parent_deadline = parent.deadline_at.context("parent deadline unavailable")?;
        if parent_deadline <= timestamp {
            bail!("parent deadline exceeded");
        }
        let deadline_secs = deadline_secs.unwrap_or(parent.deadline_secs);
        let deadline_at = parent_deadline.min(timestamp + deadline_secs as i64 * 1000);
        let run = Run {
            id: new_id(),
            session_id: new_id(),
            agent: agent.into(),
            prompt: prompt.into(),
            status: "queued".into(),
            created_at: timestamp,
            deadline_secs,
            cancel_requested: false,
            error: None,
            parent_run_id: Some(parent_id.into()),
            root_run_id: parent.root_run_id,
            depth: parent.depth + 1,
            deadline_at: Some(deadline_at),
            schedule_id: None,
            pinned_versions: None,
        };
        tx.execute(
            "INSERT INTO sessions (id,created_at,agent) VALUES (?1,?2,?3)",
            params![run.session_id, timestamp, agent],
        )?;
        tx.execute("INSERT INTO runs (id,session_id,agent,prompt,status,created_at,deadline_secs,parent_run_id,root_run_id,depth,deadline_at)
            VALUES (?1,?2,?3,?4,'queued',?5,?6,?7,?8,?9,?10)",
            params![run.id,run.session_id,agent,prompt,timestamp,deadline_secs,parent_id,run.root_run_id,run.depth,deadline_at])?;
        append_event(&tx, &run.id, json!({"kind":"status","status":"queued"}))?;
        append_event(
            &tx,
            parent_id,
            json!({"kind":"delegated","run_id":run.id,"agent":agent}),
        )?;
        tx.commit()?;
        self.notify(parent_id);
        self.notify(&run.id);
        Ok(run)
    }

    pub fn child_result(&self, parent: &str, id: &str) -> Result<Option<Value>> {
        let conn = self.0.lock().unwrap();
        let run = conn
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id=?1 AND parent_run_id=?2"),
                params![id, parent],
                read_run,
            )
            .optional()?
            .context("run is not a direct child")?;
        if !run.terminal() {
            return Ok(None);
        }
        let output: Option<String> = if run.status == "completed" {
            conn.query_row(
                "SELECT output FROM run_results WHERE run_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
        } else {
            None
        };
        let mut result = json!({"run_id":id,"agent":run.agent,"status":run.status,
            "output":output,"error":run.error,"truncated":false});
        // Truncate text before encoding, preserving a valid JSON envelope.
        while result.to_string().len() > 64 * 1024 {
            let output = result["output"].as_str().unwrap_or_default();
            if output.is_empty() {
                bail!("child result metadata exceeds size limit");
            }
            let mut end = output.len() * 3 / 4;
            while !output.is_char_boundary(end) {
                end -= 1;
            }
            result["output"] = Value::String(output[..end].to_owned());
            result["truncated"] = json!(true);
        }
        Ok(Some(result))
    }

    fn signal_cancelled(&self, ids: Vec<String>) {
        for id in ids {
            self.notify(&id);
            if let Some(token) = self.1.cancellations.lock().unwrap().get(&id) {
                token.cancel();
            }
        }
    }

    /// Ends a running run without saving a turn. A no-op for a run that is
    /// already terminal.
    pub fn finish(&self, id: &str, status: &str, error: Option<&str>) -> Result<()> {
        self.end(id, status, error, None)
    }

    /// Completes a running run and saves its turn's transcript in the same
    /// transaction, so a session's history only ever includes completed
    /// runs. A no-op, saving nothing, for a run that is already terminal.
    pub fn complete(&self, id: &str, transcript: &str) -> Result<()> {
        self.end(id, "completed", None, Some(transcript))
    }

    fn end(
        &self,
        id: &str,
        status: &str,
        error: Option<&str>,
        transcript: Option<&str>,
    ) -> Result<()> {
        if !is_terminal(status) {
            bail!("invalid terminal status");
        }
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let (session_id, cancelled) = tx.query_row(
            "SELECT session_id, cancel_requested FROM runs WHERE id=?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
        )?;
        // A turn that finished before execution was stopped stays completed,
        // even if cancellation was requested meanwhile.
        let status = if cancelled && status != "completed" {
            "cancelled"
        } else {
            status
        };
        let mut descendants = Vec::new();
        if tx.execute(
            "UPDATE runs SET status=?2, error=?3 WHERE id=?1 AND status='running'",
            params![id, status, error],
        )? == 1
        {
            if let Some(transcript) = transcript {
                let output = host::turn_output(transcript).ok();
                upsert_transcript(&tx, &session_id, transcript)?;
                if let Some(output) = output {
                    tx.execute(
                        "INSERT OR REPLACE INTO run_results VALUES (?1,?2)",
                        params![id, output],
                    )?;
                }
                append_event(&tx, id, json!({"kind":"turn_saved"}))?;
            }
            append_event(
                &tx,
                id,
                json!({"kind":"status", "status":status, "error":error}),
            )?;
        }
        descendants.extend(cancel_descendants(&tx, id)?);
        tx.commit()?;
        self.signal_cancelled(descendants);
        self.notify(id);
        self.1.cancellations.lock().unwrap().remove(id);
        Ok(())
    }

    pub fn cancel(&self, id: &str) -> Result<Run> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let mut run = tx
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id=?1"),
                [id],
                read_run,
            )
            .optional()?
            .context("run not found")?;
        if !run.terminal() && !run.cancel_requested {
            run.cancel_requested = true;
            if run.status == "queued" {
                run.status = "cancelled".into();
            }
            tx.execute(
                "UPDATE runs SET cancel_requested=1, status=?2 WHERE id=?1",
                params![id, run.status],
            )?;
            append_event(&tx, id, json!({"kind":"cancellation_requested"}))?;
            if run.terminal() {
                append_event(&tx, id, json!({"kind":"status", "status":"cancelled"}))?;
            }
        }
        let descendants = cancel_descendants(&tx, id)?;
        tx.commit()?;
        self.signal_cancelled(descendants);
        self.notify(&run.id);
        if run.cancel_requested
            && let Some(token) = self.1.cancellations.lock().unwrap().get(id)
        {
            token.cancel();
        }
        Ok(run)
    }

    pub fn transcript(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT messages FROM transcripts WHERE session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn save_transcript(&self, session_id: &str, messages: &str) -> Result<()> {
        upsert_transcript(&self.0.lock().unwrap(), session_id, messages)
    }

    /// Creates an active schedule that fires at `next_fire_at`, pinned to
    /// `pin`, and stores the definitions in `versions` so what it was
    /// approved for can be read back after the config changes.
    #[allow(clippy::too_many_arguments)]
    pub fn create_schedule(
        &self,
        id: Option<String>,
        agent: String,
        prompt: String,
        at: String,
        tz: String,
        next_fire_at: i64,
        deadline_secs: u64,
        pin: Pin,
        versions: &[host::version::AgentVersion],
    ) -> Result<Schedule> {
        if prompt.trim().is_empty() || prompt.len() > 256 * 1024 {
            bail!("prompt must contain 1–262144 bytes");
        }
        if !(1..=86400).contains(&deadline_secs) {
            bail!("deadline_secs must be between 1 and 86400");
        }
        let id = id.unwrap_or_else(new_id);
        host::check_schedule_id(&id)?;
        let schedule = Schedule {
            id,
            agent,
            prompt,
            at,
            tz,
            next_fire_at: Some(next_fire_at),
            deadline_secs,
            status: "active".into(),
            pinned_versions: pin,
            paused_reason: None,
            created_at: now(),
            changed_agents: Vec::new(),
        };
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        if tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM schedules WHERE id=?1)",
            [&schedule.id],
            |r| r.get::<_, bool>(0),
        )? {
            bail!("schedule '{}' already exists", schedule.id);
        }
        store_versions(&tx, versions)?;
        tx.execute(
            &format!("INSERT INTO schedules ({SCHEDULE_COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"),
            params![
                schedule.id,
                schedule.agent,
                schedule.prompt,
                schedule.at,
                schedule.tz,
                schedule.next_fire_at,
                schedule.deadline_secs,
                schedule.status,
                serde_json::to_string(&schedule.pinned_versions)?,
                schedule.paused_reason,
                schedule.created_at,
            ],
        )?;
        schedule_event(
            &tx,
            &schedule.id,
            json!({"kind":"created", "next_fire_at":next_fire_at, "versions":schedule.pinned_versions}),
        )?;
        tx.commit()?;
        Ok(schedule)
    }

    /// Every schedule that hasn't been deleted, oldest first.
    pub fn schedules(&self) -> Result<Vec<Schedule>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE status!='deleted'
             ORDER BY created_at, rowid"
        ))?;
        Ok(stmt
            .query_map([], read_schedule)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn schedule(&self, id: &str) -> Result<Option<Schedule>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE id=?1"),
                [id],
                read_schedule,
            )
            .optional()?)
    }

    /// What was done to a schedule and what it did, oldest first. Each
    /// record's `id` orders it among all schedules' records.
    pub fn schedule_history(&self, id: &str) -> Result<Vec<Event>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, created_at, data FROM schedule_events WHERE schedule_id=?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (seq, at, data) = row?;
            let mut data: Value = serde_json::from_str(&data)?;
            data["at"] = json!(at);
            Ok(Event {
                id: seq,
                run_id: String::new(),
                data,
            })
        })
        .collect()
    }

    /// Changes a schedule's status from one of `from` to `to`, recording
    /// `kind` in its history, and returns it. Pausing or deleting also
    /// cancels its occurrences that haven't started.
    fn transition(
        &self,
        id: &str,
        from: &[&str],
        to: &str,
        reason: Option<&str>,
        kind: &str,
    ) -> Result<Schedule> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let schedule = tx
            .query_row(
                &format!("SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE id=?1"),
                [id],
                read_schedule,
            )
            .optional()?
            .context("schedule not found")?;
        if !from.contains(&schedule.status.as_str()) {
            bail!("schedule '{id}' is {}", schedule.status);
        }
        let next_fire_at = if to == "deleted" {
            None
        } else {
            schedule.next_fire_at
        };
        tx.execute(
            "UPDATE schedules SET status=?2, paused_reason=?3, next_fire_at=?4 WHERE id=?1",
            params![id, to, reason, next_fire_at],
        )?;
        let cancelled = if matches!(to, "paused" | "deleted") {
            cancel_queued_occurrences(&tx, id)?
        } else {
            Vec::new()
        };
        schedule_event(
            &tx,
            id,
            json!({"kind":kind, "reason":reason, "cancelled_runs":cancelled}),
        )?;
        tx.commit()?;
        self.signal_cancelled(cancelled);
        drop(conn);
        Ok(self.schedule(id)?.expect("schedule exists"))
    }

    /// Pauses an active schedule, e.g. because its agent changed.
    pub fn pause_schedule(&self, id: &str, reason: &str) -> Result<Schedule> {
        self.transition(id, &["active"], "paused", Some(reason), "paused")
    }

    /// Resumes a paused schedule. The caller checks its pin still holds.
    pub fn resume_schedule(&self, id: &str) -> Result<Schedule> {
        self.transition(id, &["paused"], "active", None, "resumed")
    }

    pub fn delete_schedule(&self, id: &str) -> Result<Schedule> {
        self.transition(
            id,
            &["active", "paused", "done"],
            "deleted",
            None,
            "deleted",
        )
    }

    /// Re-pins an active or paused schedule to `pin`, storing the
    /// definitions in `versions`, and makes it active. Returns it and the
    /// pin it had before.
    pub fn approve_schedule(
        &self,
        id: &str,
        pin: Pin,
        versions: &[host::version::AgentVersion],
    ) -> Result<(Schedule, Pin)> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let schedule = tx
            .query_row(
                &format!("SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE id=?1"),
                [id],
                read_schedule,
            )
            .optional()?
            .context("schedule not found")?;
        if !matches!(schedule.status.as_str(), "active" | "paused") {
            bail!("schedule '{id}' is {}", schedule.status);
        }
        store_versions(&tx, versions)?;
        tx.execute(
            "UPDATE schedules SET status='active', paused_reason=NULL, pinned_versions=?2 WHERE id=?1",
            params![id, serde_json::to_string(&pin)?],
        )?;
        schedule_event(
            &tx,
            id,
            json!({"kind":"approved", "from":schedule.pinned_versions, "to":pin}),
        )?;
        tx.commit()?;
        drop(conn);
        Ok((
            self.schedule(id)?.expect("schedule exists"),
            schedule.pinned_versions,
        ))
    }

    /// Pauses every active schedule whose pin no longer matches `current`,
    /// what the daemon would run each agent with now. Returns them.
    pub fn pause_changed_schedules(
        &self,
        current: &dyn Fn(&str) -> Option<Pin>,
    ) -> Result<Vec<Schedule>> {
        let mut paused = Vec::new();
        for schedule in self.schedules()? {
            if schedule.status != "active" {
                continue;
            }
            let changed = host::version::changed(
                &schedule.pinned_versions,
                current(&schedule.agent).as_ref(),
            );
            if !changed.is_empty() {
                paused.push(self.pause_schedule(&schedule.id, &version_change_reason(&changed))?);
            }
        }
        Ok(paused)
    }

    /// Enqueues every active schedule due at `now` as a run in a fresh
    /// session, if `current` still gives its pin; otherwise pauses it.
    /// Each schedule fires in its own transaction, and an occurrence that
    /// was already enqueued is never enqueued again. Returns the new runs.
    pub fn fire_due(&self, now: i64, current: &dyn Fn(&str) -> Option<Pin>) -> Result<Vec<Run>> {
        let due = {
            let conn = self.0.lock().unwrap();
            let mut stmt = conn.prepare(&format!(
                "SELECT {SCHEDULE_COLUMNS} FROM schedules
                 WHERE status='active' AND next_fire_at<=?1 ORDER BY next_fire_at, rowid"
            ))?;
            stmt.query_map([now], read_schedule)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut runs = Vec::new();
        for schedule in due {
            let changed = host::version::changed(
                &schedule.pinned_versions,
                current(&schedule.agent).as_ref(),
            );
            if !changed.is_empty() {
                self.pause_schedule(&schedule.id, &version_change_reason(&changed))?;
                continue;
            }
            if let Some(run) = self.fire(&schedule)? {
                runs.push(run);
            }
        }
        Ok(runs)
    }

    /// Fails scheduled run `id`, which was refused before it started
    /// because its agent changed, and pauses its schedule with `reason`.
    /// The occurrence is given back, so approving the schedule again makes
    /// it fire again: a refused run never executed anything.
    pub fn refuse_scheduled_run(&self, id: &str, reason: &str) -> Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let (schedule, occurrence): (Option<String>, Option<String>) = tx.query_row(
            "SELECT schedule_id, occurrence_id FROM runs WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if tx.execute(
            "UPDATE runs SET status='failed', error=?2, occurrence_id=NULL
             WHERE id=?1 AND status='running'",
            params![id, reason],
        )? == 1
        {
            append_event(
                &tx,
                id,
                json!({"kind":"status", "status":"failed", "error":reason}),
            )?;
            if let Some(schedule) = schedule {
                // Schedule ids can't contain '@'.
                let scheduled_for = occurrence.and_then(|o| {
                    o.rsplit_once('@')
                        .and_then(|(_, at)| at.parse::<i64>().ok())
                });
                tx.execute(
                    "UPDATE schedules SET status='paused', paused_reason=?2,
                         next_fire_at=COALESCE(next_fire_at, ?3)
                     WHERE id=?1 AND status IN ('active','done')",
                    params![schedule, reason, scheduled_for],
                )?;
                schedule_event(
                    &tx,
                    &schedule,
                    json!({"kind":"refused", "run_id":id, "reason":reason}),
                )?;
            }
        }
        tx.commit()?;
        self.notify(id);
        self.1.cancellations.lock().unwrap().remove(id);
        Ok(())
    }

    fn fire(&self, schedule: &Schedule) -> Result<Option<Run>> {
        let scheduled_for = schedule.next_fire_at.context("due schedule has a time")?;
        let occurrence = format!("{}@{scheduled_for}", schedule.id);
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        // Re-read under the transaction: it may have been paused or deleted
        // since it was found due.
        let status: String = tx.query_row(
            "SELECT status FROM schedules WHERE id=?1",
            [&schedule.id],
            |r| r.get(0),
        )?;
        if status != "active" {
            return Ok(None);
        }
        let enqueued: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE occurrence_id=?1)",
            [&occurrence],
            |r| r.get(0),
        )?;
        let run = if enqueued {
            None
        } else {
            let timestamp = now();
            let id = new_id();
            let run = Run {
                id: id.clone(),
                session_id: new_id(),
                agent: schedule.agent.clone(),
                prompt: schedule.prompt.clone(),
                status: "queued".into(),
                created_at: timestamp,
                deadline_secs: schedule.deadline_secs,
                cancel_requested: false,
                error: None,
                parent_run_id: None,
                root_run_id: id,
                depth: 0,
                deadline_at: None,
                schedule_id: Some(schedule.id.clone()),
                pinned_versions: Some(schedule.pinned_versions.clone()),
            };
            tx.execute(
                "INSERT INTO sessions (id, created_at, agent) VALUES (?1, ?2, ?3)",
                params![run.session_id, timestamp, run.agent],
            )?;
            tx.execute(
                "INSERT INTO runs (id, session_id, agent, prompt, status, created_at, deadline_secs,
                     root_run_id, schedule_id, occurrence_id, pinned_versions)
                 VALUES (?1,?2,?3,?4,'queued',?5,?6,?1,?7,?8,?9)",
                params![
                    run.id,
                    run.session_id,
                    run.agent,
                    run.prompt,
                    timestamp,
                    run.deadline_secs,
                    schedule.id,
                    occurrence,
                    serde_json::to_string(&schedule.pinned_versions)?,
                ],
            )?;
            append_event(&tx, &run.id, json!({"kind":"status", "status":"queued"}))?;
            schedule_event(
                &tx,
                &schedule.id,
                json!({"kind":"fired", "run_id":run.id, "scheduled_for":scheduled_for}),
            )?;
            Some(run)
        };
        tx.execute(
            "UPDATE schedules SET status='done', next_fire_at=NULL WHERE id=?1",
            [&schedule.id],
        )?;
        tx.commit()?;
        if let Some(run) = &run {
            self.notify(&run.id);
        }
        Ok(run)
    }

    pub fn recover(&self) -> Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let ids = {
            let mut stmt = tx.prepare("SELECT id FROM runs WHERE status='running'")?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut descendants = Vec::new();
        for id in &ids {
            descendants.extend(cancel_descendants(&tx, id)?);
            tx.execute("UPDATE runs SET status='interrupted', error='daemon stopped during execution' WHERE id=?1", [id])?;
            append_event(
                &tx,
                id,
                json!({"kind":"status", "status":"interrupted", "error":"daemon stopped during execution"}),
            )?;
        }
        // Also clean up queued descendants whose parent was terminal before a crash.
        let orphan_parents = {
            let mut stmt = tx.prepare("SELECT DISTINCT parent_run_id FROM runs WHERE status='queued'
                AND parent_run_id IN (SELECT id FROM runs WHERE status NOT IN ('running','queued'))")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in orphan_parents {
            descendants.extend(cancel_descendants(&tx, &id)?);
        }
        tx.commit()?;
        self.signal_cancelled(descendants);
        for id in ids {
            self.notify(&id);
        }
        Ok(())
    }
}

fn cancel_descendants(conn: &Connection, parent: &str) -> Result<Vec<String>> {
    let ids = {
        let mut stmt = conn.prepare(
            "WITH RECURSIVE descendants(id) AS (
            SELECT id FROM runs WHERE parent_run_id=?1 UNION ALL
            SELECT runs.id FROM runs JOIN descendants ON runs.parent_run_id=descendants.id)
            SELECT runs.id FROM runs JOIN descendants USING(id)
            WHERE status IN ('queued','running') AND cancel_requested=0",
        )?;
        stmt.query_map([parent], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in &ids {
        conn.execute(
            "UPDATE runs SET cancel_requested=1,
            status=CASE WHEN status='queued' THEN 'cancelled' ELSE status END WHERE id=?1",
            [id],
        )?;
        append_event(conn, id, json!({"kind":"cancellation_requested"}))?;
        let status: String =
            conn.query_row("SELECT status FROM runs WHERE id=?1", [id], |r| r.get(0))?;
        if status == "cancelled" {
            append_event(conn, id, json!({"kind":"status","status":"cancelled"}))?;
        }
    }
    Ok(ids)
}
