use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct Database(Arc<Mutex<Connection>>, Arc<Signals>);

struct Signals {
    changes: Mutex<HashMap<String, Weak<Notify>>>,
    cancellations: Mutex<HashMap<String, CancellationToken>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
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
    })
}

const RUN_COLUMNS: &str = "id, session_id, agent, prompt, status, created_at, deadline_secs, cancel_requested, error, parent_run_id, root_run_id, depth, deadline_at";

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
        if version > 3 {
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
        Ok(Self(
            Arc::new(Mutex::new(conn)),
            Arc::new(Signals {
                changes: Mutex::new(HashMap::new()),
                cancellations: Mutex::new(HashMap::new()),
            }),
        ))
    }

    pub fn create_session(&self, id: Option<String>) -> Result<Session> {
        let id = id.unwrap_or_else(new_id);
        host::check_session_id(&id)?;
        let created_at = now();
        let session = Session {
            id,
            created_at,
            updated_at: created_at,
            ..Session::default()
        };
        self.0.lock().unwrap().execute(
            "INSERT INTO sessions VALUES (?1, ?2)",
            params![session.id, session.created_at],
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
                EXISTS (SELECT 1 FROM runs WHERE session_id=s.id AND parent_run_id IS NOT NULL)
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
                tx.execute("INSERT INTO sessions VALUES (?1, ?2)", params![id, now()])?;
                id
            }
        };
        let id = new_id();
        let run = Run {
            id: id.clone(),
            parent_run_id: None,
            root_run_id: id,
            depth: 0,
            deadline_at: None,
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
        };
        tx.execute(
            "INSERT INTO sessions VALUES (?1,?2)",
            params![run.session_id, timestamp],
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
