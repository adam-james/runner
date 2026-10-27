use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::future::BoxFuture;
use reqwest::{Client, StatusCode};
use runner::{
    db::{Database, Event, Run},
    service::{Executor, LocalInstance, Service, worker},
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct FakeExecutor {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    sessions: Arc<Mutex<HashMap<String, usize>>>,
}

impl Executor for FakeExecutor {
    /// The transcript it returns is the previous one plus `prompt|`, so a
    /// session's saved transcript lists the turns that were committed.
    fn execute(
        &self,
        run: Run,
        transcript: Option<String>,
        events: host::EventSink,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<String>> {
        let active = self.active.clone();
        let peak = self.peak.clone();
        let sessions = self.sessions.clone();
        Box::pin(async move {
            struct Guard(Arc<AtomicUsize>);

            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            let _guard = Guard(active);
            peak.fetch_max(current, Ordering::SeqCst);
            let turn = {
                let mut sessions = sessions.lock().unwrap();
                let count = sessions.entry(run.session_id.clone()).or_default();
                *count += 1;
                *count
            };
            events(json!({"kind":"text", "text":format!("turn {turn}: ")}));
            let transcript = format!("{}{}|", transcript.unwrap_or_default(), run.prompt);
            if run.prompt == "wait" {
                futures_util::future::pending::<()>().await;
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
            if run.prompt == "fail" {
                bail!("private provider failure: SECRET");
            }
            if run.prompt == "api error" {
                return Err(host::TurnFailed("API error 401: invalid x-api-key".into()).into());
            }
            if run.prompt == "panic" {
                panic!("executor bug");
            }
            if run.prompt == "which agent" {
                events(json!({"kind":"text", "text":run.agent}));
                return Ok(transcript);
            }
            events(json!({"kind":"text", "text":run.prompt}));
            Ok(transcript)
        })
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    db: Database,
    executor: Arc<FakeExecutor>,
    client: Client,
    url: String,
    stop: CancellationToken,
    worker: tokio::task::JoinHandle<Result<()>>,
    http: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn new(concurrency: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("state.sqlite3")).unwrap();
        let executor = Arc::new(FakeExecutor::default());
        let stop = CancellationToken::new();
        let worker = tokio::spawn(worker(
            db.clone(),
            executor.clone(),
            concurrency,
            stop.clone(),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let service = Service::new(
            db.clone(),
            "test-token".into(),
            ["default", "researcher"]
                .map(|name| host::AgentInfo {
                    name: name.into(),
                    provider: "openai".into(),
                    model: "gpt-5".into(),
                    tools: vec!["web_search".into()],
                    delegates: vec![],
                    max_tokens: host::DEFAULT_MAX_TOKENS,
                    max_model_calls: None,
                    tool_access: Default::default(),
                })
                .into(),
        );
        let http = tokio::spawn(async move {
            axum::serve(listener, service.router()).await.unwrap();
        });
        Self {
            _dir: dir,
            db,
            executor,
            client: Client::builder().no_proxy().build().unwrap(),
            url,
            stop,
            worker,
            http,
        }
    }

    async fn submit(&self, session: Option<&str>, prompt: &str, deadline: u64) -> Run {
        self.client
            .post(format!("{}/v1/runs", self.url))
            .bearer_auth("test-token")
            .json(&json!({"session_id":session,"prompt":prompt,"deadline_secs":deadline}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn wait_status(&self, id: &str, status: &str) -> Run {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let run = self.db.run(id).unwrap().unwrap();
                if run.status == status {
                    break run;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("did not reach {status}: {:?}", self.db.run(id)))
    }

    async fn close(self) {
        self.stop.cancel();
        self.worker.await.unwrap().unwrap();
        self.http.abort();
    }
}

#[tokio::test]
async fn detached_runs_replay_and_reconnect_over_http() {
    let h = Harness::new(2).await;
    let run = h.submit(None, "hello héllo", 5).await;
    h.wait_status(&run.id, "completed").await;
    let all = h.db.events(&run.id, 0).unwrap();
    assert_eq!(all.first().unwrap().data["status"], "queued");
    assert_eq!(all.last().unwrap().data["status"], "completed");
    let output: String = all.iter().filter_map(|e| e.data["text"].as_str()).collect();
    assert_eq!(output, "turn 1: hello héllo");
    let cursor = all[1].id;
    let response = h
        .client
        .get(format!("{}/v1/runs/{}/events", h.url, run.id))
        .bearer_auth("test-token")
        .header("Last-Event-ID", cursor.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let text = response.text().await.unwrap();
    let replayed: Vec<Event> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        replayed.iter().map(|e| e.id).collect::<Vec<_>>(),
        all[2..].iter().map(|e| e.id).collect::<Vec<_>>()
    );
    let continuation = h.submit(Some(&run.session_id), "again", 5).await;
    h.wait_status(&continuation.id, "completed").await;
    let text: String =
        h.db.events(&continuation.id, 0)
            .unwrap()
            .iter()
            .filter_map(|e| e.data["text"].as_str())
            .collect();
    assert_eq!(text, "turn 2: again");
    h.close().await;
}

#[tokio::test]
async fn runs_execute_the_agent_definition_they_name() {
    let h = Harness::new(1).await;
    let agents: Value = h
        .client
        .get(format!("{}/v1/agents", h.url))
        .bearer_auth("test-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        agents,
        json!(
            ["default", "researcher"].map(|name| json!({"name":name, "provider":"openai",
            "model":"gpt-5", "tools":["web_search"], "max_tokens":16384,"delegates":[],
            "max_model_calls":null, "tool_access":{}}))
        )
    );
    let run: Run = h
        .client
        .post(format!("{}/v1/runs", h.url))
        .bearer_auth("test-token")
        .json(&json!({"prompt":"which agent","agent":"researcher"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(run.agent, "researcher");
    let other = h.submit(None, "which agent", 5).await;
    let listed: Vec<Run> = h
        .client
        .get(format!("{}/v1/runs?session_id={}", h.url, other.session_id))
        .bearer_auth("test-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(|r| &r.id).collect::<Vec<_>>(),
        [&other.id]
    );
    let rejected = h
        .client
        .post(format!("{}/v1/runs", h.url))
        .bearer_auth("test-token")
        .json(&json!({"session_id":run.session_id,"agent":"default","prompt":"switch"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(
        rejected
            .text()
            .await
            .unwrap()
            .contains("create a new session")
    );
    // Omitting the agent resumes the session owner.
    let followup = h.submit(Some(&run.session_id), "which agent", 5).await;
    assert_eq!(followup.agent, "researcher");
    for (run, agent) in [(&run, "researcher"), (&followup, "researcher")] {
        h.wait_status(&run.id, "completed").await;
        let text: String =
            h.db.events(&run.id, 0)
                .unwrap()
                .iter()
                .filter_map(|e| e.data["text"].as_str())
                .collect();
        assert!(text.ends_with(agent), "{text}");
    }
    h.close().await;
}

#[tokio::test]
async fn capacity_and_session_order_are_enforced() {
    let h = Harness::new(2).await;
    let session = h.db.create_session(Some("shared".into())).unwrap();
    let first = h.submit(Some(&session.id), "wait", 5).await;
    h.wait_status(&first.id, "running").await;
    let second = h.submit(Some(&session.id), "second", 5).await;
    let independent = h.submit(None, "independent", 5).await;
    h.wait_status(&independent.id, "completed").await;
    assert_eq!(h.db.run(&second.id).unwrap().unwrap().status, "queued");
    h.db.cancel(&first.id).unwrap();
    h.wait_status(&first.id, "cancelled").await;
    h.wait_status(&second.id, "completed").await;
    assert_eq!(h.executor.active.load(Ordering::SeqCst), 0);
    assert_eq!(h.executor.peak.load(Ordering::SeqCst), 2);
    h.close().await;
}

#[tokio::test]
async fn cancellation_deadlines_and_failures_release_execution() {
    let h = Harness::new(1).await;
    let active = h.submit(None, "wait", 5).await;
    h.wait_status(&active.id, "running").await;
    let queued = h.submit(None, "never execute", 5).await;
    assert_eq!(h.db.cancel(&queued.id).unwrap().status, "cancelled");
    assert_eq!(h.db.cancel(&queued.id).unwrap().status, "cancelled");
    let response = h
        .client
        .post(format!("{}/v1/runs/{}/cancel", h.url, active.id))
        .bearer_auth("test-token")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    h.wait_status(&active.id, "cancelled").await;
    assert_eq!(
        h.executor.active.load(Ordering::SeqCst),
        0,
        "terminal means execution was dropped"
    );
    let timed = h.submit(None, "wait", 1).await;
    let run = h.wait_status(&timed.id, "failed").await;
    assert_eq!(run.error.as_deref(), Some("run deadline exceeded"));
    assert_eq!(h.executor.active.load(Ordering::SeqCst), 0);
    let failed = h.submit(None, "api error", 5).await;
    let run = h.wait_status(&failed.id, "failed").await;
    assert_eq!(
        run.error.as_deref(),
        Some("API error 401: invalid x-api-key"),
        "the agent's message for the user is kept"
    );
    let failed = h.submit(None, "fail", 5).await;
    let run = h.wait_status(&failed.id, "failed").await;
    assert_eq!(run.error.as_deref(), Some("agent execution failed"));
    assert!(
        !serde_json::to_string(&h.db.events(&failed.id, 0).unwrap())
            .unwrap()
            .contains("SECRET")
    );
    h.close().await;
}

#[tokio::test]
async fn a_panicking_job_fails_alone_and_the_worker_keeps_running() {
    let h = Harness::new(2).await;
    let sibling = h.submit(None, "sibling", 5).await;
    let broken = h.submit(None, "panic", 5).await;
    let run = h.wait_status(&broken.id, "failed").await;
    assert_eq!(run.error.as_deref(), Some("internal error"));
    h.wait_status(&sibling.id, "completed").await;
    let later = h.submit(None, "later", 5).await;
    h.wait_status(&later.id, "completed").await;
    assert!(!h.worker.is_finished());
    h.close().await;
}

#[tokio::test]
async fn failed_terminal_writes_are_retried_and_release_the_session() {
    let h = Harness::new(2).await;
    // Fail every terminal-status write, including the worker's fallback.
    let sqlite = rusqlite::Connection::open(h._dir.path().join("state.sqlite3")).unwrap();
    sqlite
        .execute_batch(
            "CREATE TRIGGER fail_finish BEFORE UPDATE OF status ON runs
             WHEN NEW.status <> 'running' BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
    let session = h.db.create_session(Some("stuck".into())).unwrap();
    let broken = h.submit(Some(&session.id), "hello", 5).await;
    let later = h.submit(Some(&session.id), "later", 5).await;
    h.wait_status(&broken.id, "running").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.db.run(&broken.id).unwrap().unwrap().status, "running");
    assert_eq!(h.db.run(&later.id).unwrap().unwrap().status, "queued");
    sqlite.execute_batch("DROP TRIGGER fail_finish").unwrap();
    let run = h.wait_status(&broken.id, "failed").await;
    assert_eq!(run.error.as_deref(), Some("internal error"));
    h.wait_status(&later.id, "completed").await;
    h.close().await;
}

#[test]
fn a_late_cancel_does_not_hide_a_completed_turn() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("state.sqlite3")).unwrap();
    let completed = db
        .submit(None, "default".into(), "done".into(), 60)
        .unwrap();
    let failed = db
        .submit(None, "default".into(), "broken".into(), 60)
        .unwrap();
    db.claim().unwrap().unwrap();
    db.cancel(&completed.id).unwrap();
    db.finish(&completed.id, "completed", None).unwrap();
    assert_eq!(db.run(&completed.id).unwrap().unwrap().status, "completed");
    db.claim().unwrap().unwrap();
    db.cancel(&failed.id).unwrap();
    db.finish(&failed.id, "failed", Some("agent execution failed"))
        .unwrap();
    assert_eq!(db.run(&failed.id).unwrap().unwrap().status, "cancelled");
}

#[tokio::test]
async fn authenticated_api_rejects_invalid_requests() {
    let h = Harness::new(1).await;
    assert_eq!(
        h.client
            .get(format!("{}/v1/runs", h.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.client
            .get(format!("{}/v1/runs", h.url))
            .bearer_auth("test-token")
            .header("Origin", "http://example.com")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for input in [
        json!({"prompt":""}),
        json!({"prompt":"a","deadline_secs":0}),
        json!({"prompt":"a","session_id":"../escape"}),
        json!({"prompt":"a","session_id":"missing"}),
        json!({"prompt":"a","agent":"nope"}),
    ] {
        assert_eq!(
            h.client
                .post(format!("{}/v1/runs", h.url))
                .bearer_auth("test-token")
                .json(&input)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        h.client
            .get(format!("{}/v1/runs?session={}", h.url, "x"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST,
        "unknown filters are rejected"
    );
    assert!(h.db.runs(None).unwrap().is_empty());
    assert!(
        h.db.sessions().unwrap().is_empty(),
        "invalid submissions cannot create orphan sessions"
    );
    assert_eq!(
        h.client
            .get(format!("{}/v1/runs/missing", h.url))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test]
async fn shutdown_interrupts_active_work_and_preserves_queue() {
    let h = Harness::new(1).await;
    let active = h.submit(None, "wait", 5).await;
    h.wait_status(&active.id, "running").await;
    let queued = h.submit(None, "later", 5).await;
    h.stop.cancel();
    h.wait_status(&active.id, "interrupted").await;
    assert_eq!(h.db.run(&queued.id).unwrap().unwrap().status, "queued");
    assert_eq!(h.executor.active.load(Ordering::SeqCst), 0);
    h.close().await;
}

#[test]
fn a_version_1_database_gains_transcripts_and_keeps_its_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let run = Database::open(&path)
        .unwrap()
        .submit(None, "default".into(), "before upgrade".into(), 60)
        .unwrap();
    // What a daemon from before transcripts moved into SQLite left behind.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "ALTER TABLE sessions DROP COLUMN agent;
        DROP TABLE transcripts;
        DROP TABLE run_results;
        DROP INDEX runs_by_parent; DROP INDEX runs_by_root;
        ALTER TABLE runs DROP COLUMN parent_run_id;
        ALTER TABLE runs DROP COLUMN root_run_id;
        ALTER TABLE runs DROP COLUMN depth;
        ALTER TABLE runs DROP COLUMN deadline_at;
        PRAGMA user_version=1;",
    )
    .unwrap();
    drop(conn);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.run(&run.id).unwrap().unwrap().prompt, "before upgrade");
    assert_eq!(db.transcript(&run.session_id).unwrap(), None);
    db.save_transcript(&run.session_id, "[1]").unwrap();
    db.save_transcript(&run.session_id, "[2]").unwrap();
    assert_eq!(
        db.transcript(&run.session_id).unwrap().as_deref(),
        Some("[2]")
    );
}

#[test]
fn crash_recovery_is_durable_and_does_not_replay_running_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let db = Database::open(&path).unwrap();
    let active = db
        .submit(None, "default".into(), "in progress".into(), 60)
        .unwrap();
    let queued = db
        .submit(None, "default".into(), "pending".into(), 60)
        .unwrap();
    assert_eq!(db.claim().unwrap().unwrap().id, active.id);
    db.event(&active.id, json!({"kind":"text","text":"partial"}))
        .unwrap();
    drop(db);
    let db = Database::open(&path).unwrap();
    db.recover().unwrap();
    db.recover().unwrap();
    assert_eq!(db.run(&active.id).unwrap().unwrap().status, "interrupted");
    let events = db.events(&active.id, 0).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.data["status"] == "interrupted")
            .count(),
        1
    );
    assert_eq!(db.claim().unwrap().unwrap().id, queued.id);
    assert!(db.claim().unwrap().is_none());
}

#[test]
fn daemon_lock_and_descriptor_are_private() {
    let dir = tempfile::tempdir().unwrap();
    let first = LocalInstance::acquire(dir.path()).unwrap();
    assert!(LocalInstance::acquire(dir.path()).is_err());
    first
        .publish(&runner::service::ConnectionInfo {
            port: 1234,
            token: "private".into(),
        })
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("connection.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let value: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("connection.json")).unwrap())
            .unwrap();
    assert_eq!(value["port"], 1234);
    drop(first);
    assert!(!dir.path().join("connection.json").exists());
    assert!(LocalInstance::acquire(dir.path()).is_ok());
}

#[cfg(unix)]
#[test]
fn acquire_creates_a_private_directory_but_leaves_existing_ones_alone() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let created = dir.path().join("new/service");
    let _first = LocalInstance::acquire(&created).unwrap();
    assert_eq!(
        std::fs::metadata(&created).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let existing = dir.path().join("shared");
    std::fs::create_dir(&existing).unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _second = LocalInstance::acquire(&existing).unwrap();
    assert_eq!(
        std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[tokio::test]
async fn sqlite_write_contention_does_not_block_the_async_runtime() {
    let h = Harness::new(1).await;
    let path = h._dir.path().join("state.sqlite3");
    let (locked, ready) = std::sync::mpsc::channel();
    let lock = std::thread::spawn(move || {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        locked.send(()).unwrap();
        std::thread::sleep(Duration::from_secs(1));
        connection.execute_batch("ROLLBACK").unwrap();
    });
    ready.recv().unwrap();
    let client = h.client.clone();
    let url = h.url.clone();
    let request = tokio::spawn(async move {
        client
            .post(format!("{url}/v1/runs"))
            .bearer_auth("test-token")
            .json(&json!({"prompt":"hello"}))
            .send()
            .await
            .unwrap()
    });
    let start = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "SQLite stalled Tokio"
    );
    assert_eq!(request.await.unwrap().status(), StatusCode::ACCEPTED);
    lock.join().unwrap();
    h.close().await;
}

#[tokio::test]
async fn live_sse_watchers_receive_all_committed_events_and_close() {
    let h = Harness::new(1).await;
    let run = h.submit(None, "wait", 5).await;
    h.wait_status(&run.id, "running").await;
    let mut watchers = Vec::new();
    for _ in 0..3 {
        let response = h
            .client
            .get(format!("{}/v1/runs/{}/events", h.url, run.id))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap();
        watchers.push(tokio::spawn(async move { response.text().await.unwrap() }));
    }
    let id = run.id.clone();
    h.db.blocking(move |db| {
        for sequence in 0..300 {
            db.event(&id, json!({"kind":"test", "sequence":sequence}))?;
        }
        db.cancel(&id)?;
        Ok(())
    })
    .await
    .unwrap();
    h.wait_status(&run.id, "cancelled").await;
    let expected = h.db.events(&run.id, 0).unwrap();
    for watcher in watchers {
        let text = tokio::time::timeout(Duration::from_secs(5), watcher)
            .await
            .unwrap()
            .unwrap();
        let events: Vec<Event> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.data["kind"] == "test")
                .count(),
            300
        );
        assert_eq!(events.last().unwrap().data["status"], "cancelled");
        assert_eq!(events.first().unwrap().id, expected.first().unwrap().id);
        assert!(events.windows(2).all(|pair| pair[0].id < pair[1].id));
    }
    h.close().await;
}

#[tokio::test]
async fn cancellation_before_execution_is_remembered_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("state.sqlite3")).unwrap();
    let run = db
        .submit(None, "default".into(), "wait".into(), 60)
        .unwrap();
    db.claim().unwrap().unwrap();
    db.cancel(&run.id).unwrap();
    assert!(db.cancellation(&run.id).is_cancelled());
}

#[cfg(unix)]
#[test]
fn database_files_are_private_in_a_shared_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.path().join("state.sqlite3");
    let mode = |suffix: &str| {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        std::fs::metadata(file).unwrap().permissions().mode() & 0o777
    };
    // A database and a leftover sidecar from before permissions were enforced.
    std::fs::write(&path, b"").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let stale_shm = dir.path().join("state.sqlite3-shm");
    std::fs::write(&stale_shm, b"").unwrap();
    std::fs::set_permissions(&stale_shm, std::fs::Permissions::from_mode(0o644)).unwrap();
    let db = Database::open(&path).unwrap();
    db.submit(None, "default".into(), "secret".into(), 60)
        .unwrap();
    for suffix in ["", "-wal", "-shm"] {
        assert_eq!(mode(suffix), 0o600, "state.sqlite3{suffix}");
    }
    assert_eq!(
        std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[tokio::test]
async fn only_completed_runs_save_their_turn_in_the_same_commit() {
    let h = Harness::new(1).await;
    let session = h.db.create_session(Some("kept".into())).unwrap();
    let transcript = || h.db.transcript(&session.id).unwrap();
    let first = h.submit(Some(&session.id), "first", 5).await;
    h.wait_status(&first.id, "completed").await;
    assert_eq!(transcript().as_deref(), Some("first|"));
    // The turn's events, then turn_saved, then completion, all committed together.
    let kinds: Vec<_> =
        h.db.events(&first.id, 0)
            .unwrap()
            .into_iter()
            .map(|event| event.data)
            .collect();
    let saved = kinds
        .iter()
        .position(|e| e["kind"] == "turn_saved")
        .unwrap();
    assert_eq!(kinds[saved + 1]["status"], "completed");
    assert_eq!(saved + 2, kinds.len());
    assert!(kinds[..saved].iter().any(|e| e["kind"] == "text"));

    // A cancelled turn never reaches the commit, so the conversation is as it was.
    let cancelled = h.submit(Some(&session.id), "wait", 5).await;
    h.wait_status(&cancelled.id, "running").await;
    h.client
        .post(format!("{}/v1/runs/{}/cancel", h.url, cancelled.id))
        .bearer_auth("test-token")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    h.wait_status(&cancelled.id, "cancelled").await;
    // So do a turn that hits its deadline and one that fails.
    let timed_out = h.submit(Some(&session.id), "wait", 1).await;
    h.wait_status(&timed_out.id, "failed").await;
    let failed = h.submit(Some(&session.id), "fail", 5).await;
    h.wait_status(&failed.id, "failed").await;
    for run in [&cancelled, &timed_out, &failed] {
        assert!(
            !h.db
                .events(&run.id, 0)
                .unwrap()
                .iter()
                .any(|e| e.data["kind"] == "turn_saved")
        );
    }
    assert_eq!(transcript().as_deref(), Some("first|"));
    // The next completed turn continues from the last saved one.
    let second = h.submit(Some(&session.id), "second", 5).await;
    h.wait_status(&second.id, "completed").await;
    assert_eq!(transcript().as_deref(), Some("first|second|"));
    h.close().await;
}

#[test]
fn completing_a_run_that_already_ended_saves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("state.sqlite3")).unwrap();
    let run = db
        .submit(None, "default".into(), "queued".into(), 60)
        .unwrap();
    db.cancel(&run.id).unwrap();
    db.complete(&run.id, "late|").unwrap();
    assert_eq!(db.run(&run.id).unwrap().unwrap().status, "cancelled");
    assert_eq!(db.transcript(&run.session_id).unwrap(), None);
}

#[tokio::test]
async fn session_responses_report_current_work_counts() {
    let h = Harness::new(1).await;
    let session: runner::db::Session = h
        .client
        .post(format!("{}/v1/sessions", h.url))
        .bearer_auth("test-token")
        .json(&json!({"id":"counts"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!((session.running_runs, session.queued_runs), (0, 0));
    let first = h.submit(Some("counts"), "wait", 30).await;
    h.wait_status(&first.id, "running").await;
    let second = h.submit(Some("counts"), "wait", 30).await;
    let third = h.submit(Some("counts"), "unused", 30).await;
    let counts = || async {
        let sessions: Vec<runner::db::Session> = h
            .client
            .get(format!("{}/v1/sessions", h.url))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let session = sessions.iter().find(|s| s.id == "counts").unwrap();
        assert_eq!(session.title.as_deref(), Some("wait"));
        assert!(!session.delegated);
        (session.running_runs, session.queued_runs)
    };
    assert_eq!(counts().await, (1, 2));
    h.db.cancel(&third.id).unwrap();
    assert_eq!(counts().await, (1, 1));
    h.db.cancel(&first.id).unwrap();
    h.wait_status(&second.id, "running").await;
    assert_eq!(counts().await, (1, 0));
    h.db.cancel(&second.id).unwrap();
    h.wait_status(&second.id, "cancelled").await;
    assert_eq!(counts().await, (0, 0));
    let sessions = h.db.sessions().unwrap();
    let session = sessions.iter().find(|s| s.id == "counts").unwrap();
    assert_eq!(session.last_status.as_deref(), Some("cancelled"));
    assert!(session.updated_at >= third.created_at);
    let legacy: runner::db::Session =
        serde_json::from_value(json!({"id":"old", "created_at":0})).unwrap();
    assert_eq!((legacy.running_runs, legacy.queued_runs), (0, 0));
    h.close().await;
}

#[test]
fn sessions_bind_atomically_and_keep_their_agent_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let db = Database::open(&path).unwrap();
    let session = db.create_session(Some("locked".into())).unwrap();
    assert!(session.agent.is_none());
    assert!(
        db.submit(Some(session.id.clone()), "reader".into(), "".into(), 60)
            .is_err()
    );
    assert!(db.sessions().unwrap()[0].agent.is_none());
    let threads: Vec<_> = ["reader", "researcher"]
        .into_iter()
        .map(|agent| {
            let db = db.clone();
            std::thread::spawn(move || {
                db.submit(Some("locked".into()), agent.into(), "hello".into(), 60)
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let run = results.into_iter().find_map(Result::ok).unwrap();
    db.cancel(&run.id).unwrap();
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.sessions().unwrap()[0].agent.as_deref(),
        Some(run.agent.as_str())
    );
    let error = db
        .submit(Some("locked".into()), "other".into(), "hello".into(), 60)
        .unwrap_err();
    assert!(error.to_string().contains("create a new session"));
    assert_eq!(db.runs(Some("locked")).unwrap().len(), 1);
    db.submit(Some("locked".into()), run.agent, "again".into(), 60)
        .unwrap();
}

#[test]
fn mixed_sessions_migrate_to_the_latest_agent_and_fail_incompatible_queued_turns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let db = Database::open(&path).unwrap();
    let old = db
        .submit(None, "reader".into(), "first".into(), 60)
        .unwrap();
    let latest = db
        .submit(
            Some(old.session_id.clone()),
            "reader".into(),
            "second".into(),
            60,
        )
        .unwrap();
    db.create_session(Some("empty".into())).unwrap();
    drop(db);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("UPDATE runs SET agent='researcher', created_at=(SELECT created_at FROM runs WHERE id=?1) WHERE id=?2", rusqlite::params![old.id, latest.id]).unwrap();
    conn.execute_batch("ALTER TABLE sessions DROP COLUMN agent; PRAGMA user_version=4;")
        .unwrap();
    drop(conn);
    let db = Database::open(&path).unwrap();
    let sessions = db.sessions().unwrap();
    assert_eq!(
        sessions
            .iter()
            .find(|s| s.id == old.session_id)
            .unwrap()
            .agent
            .as_deref(),
        Some("researcher")
    );
    assert!(
        sessions
            .iter()
            .find(|s| s.id == "empty")
            .unwrap()
            .agent
            .is_none()
    );
    assert_eq!(db.run(&old.id).unwrap().unwrap().status, "failed");
    assert_eq!(db.claim().unwrap().unwrap().id, latest.id);
    assert!(
        db.submit(Some(old.session_id), "reader".into(), "again".into(), 60)
            .is_err()
    );
    drop(db);
    assert!(Database::open(&path).is_ok());
}
