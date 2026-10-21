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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub created_at: i64,
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

fn now() -> i64 {
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
    })
}

const RUN_COLUMNS: &str =
    "id, session_id, agent, prompt, status, created_at, deadline_secs, cancel_requested, error";

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
        if version > 1 {
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
            PRAGMA user_version=1;",
        )?;
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
        let session = Session {
            id,
            created_at: now(),
        };
        self.0.lock().unwrap().execute(
            "INSERT INTO sessions VALUES (?1, ?2)",
            params![session.id, session.created_at],
        )?;
        Ok(session)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let conn = self.0.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, created_at FROM sessions ORDER BY created_at, rowid")?;
        Ok(stmt
            .query_map([], |row| {
                Ok(Session {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
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
        let run = Run {
            id: new_id(),
            session_id,
            agent,
            prompt,
            status: "queued".into(),
            created_at: now(),
            deadline_secs,
            cancel_requested: false,
            error: None,
        };
        tx.execute("INSERT INTO runs (id, session_id, agent, prompt, status, created_at, deadline_secs) VALUES (?1,?2,?3,?4,?5,?6,?7)",
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

    pub fn runs(&self) -> Result<Vec<Run>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs ORDER BY created_at, rowid"
        ))?;
        Ok(stmt
            .query_map([], read_run)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Claim the oldest runnable job. One writer per session, including queued turns.
    pub fn claim(&self) -> Result<Option<Run>> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let run = tx.query_row(&format!("SELECT {RUN_COLUMNS} FROM runs AS candidate
            WHERE status='queued' AND NOT EXISTS (
                SELECT 1 FROM runs AS active WHERE active.session_id=candidate.session_id AND active.status='running')
            ORDER BY created_at, rowid LIMIT 1"), [], read_run).optional()?;
        let Some(mut run) = run else {
            return Ok(None);
        };
        tx.execute("UPDATE runs SET status='running' WHERE id=?1", [&run.id])?;
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
        append_event(&self.0.lock().unwrap(), id, data)?;
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

    pub fn finish(&self, id: &str, status: &str, error: Option<&str>) -> Result<()> {
        if !is_terminal(status) {
            bail!("invalid terminal status");
        }
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let cancelled =
            tx.query_row("SELECT cancel_requested FROM runs WHERE id=?1", [id], |r| {
                r.get::<_, bool>(0)
            })?;
        // A completed turn is already in the session history; a late cancel must not hide it.
        let status = if cancelled && status != "completed" {
            "cancelled"
        } else {
            status
        };
        if tx.execute(
            "UPDATE runs SET status=?2, error=?3 WHERE id=?1 AND status='running'",
            params![id, status, error],
        )? == 1
        {
            append_event(
                &tx,
                id,
                json!({"kind":"status", "status":status, "error":error}),
            )?;
        }
        tx.commit()?;
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
        tx.commit()?;
        self.notify(&run.id);
        if run.cancel_requested
            && let Some(token) = self.1.cancellations.lock().unwrap().get(id)
        {
            token.cancel();
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
        for id in &ids {
            tx.execute("UPDATE runs SET status='interrupted', error='daemon stopped during execution' WHERE id=?1", [id])?;
            append_event(
                &tx,
                id,
                json!({"kind":"status", "status":"interrupted", "error":"daemon stopped during execution"}),
            )?;
        }
        tx.commit()?;
        for id in ids {
            self.notify(&id);
        }
        Ok(())
    }
}
