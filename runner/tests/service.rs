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
    /// The version each agent runs with now, as if the config had changed
    /// since the service started; unset agents keep the service's.
    versions: Arc<Mutex<HashMap<String, String>>>,
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

    /// What the harness's agents pin to: their own version, empty unless
    /// changed, and no delegates.
    fn pin(&self, agent: &str) -> Option<host::version::Pin> {
        let version = self.versions.lock().unwrap().get(agent).cloned();
        Some([(agent.to_string(), version.unwrap_or_default())].into())
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
                    version: String::new(),
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
async fn agent_versions_are_stored_once_and_served_by_id() {
    let h = Harness::new(1).await;
    let run =
        h.db.submit(None, "default".into(), "task".into(), 60)
            .unwrap();
    let definition = json!({"schema":1, "agent":"default", "tools":[]});
    for _ in 0..2 {
        h.db.record_agent_version(&run.id, "sha256:01", "default", &definition)
            .unwrap();
    }
    assert_eq!(
        h.db.run_agent_version(&run.id).unwrap().as_deref(),
        Some("sha256:01")
    );
    let get = |id: &str| {
        h.client
            .get(format!("{}/v1/agent-versions/{id}", h.url))
            .bearer_auth("test-token")
            .send()
    };
    let version: Value = get("sha256:01").await.unwrap().json().await.unwrap();
    assert_eq!(version["agent"], "default");
    assert_eq!(version["definition"], definition);
    assert_eq!(
        get("sha256:02").await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
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
            "max_model_calls":null, "version":"", "tool_access":{}}))
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
    let session =
        h.db.create_session(Some("shared".into()), "default".into())
            .unwrap();
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
    let session =
        h.db.create_session(Some("stuck".into()), "default".into())
            .unwrap();
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
    let session =
        h.db.create_session(Some("kept".into()), "default".into())
            .unwrap();
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
        .json(&json!({"id":"counts","agent":"default"}))
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
        serde_json::from_value(json!({"id":"old", "created_at":0,"agent":"default"})).unwrap();
    assert_eq!((legacy.running_runs, legacy.queued_runs), (0, 0));
    h.close().await;
}

#[test]
fn sessions_are_bound_at_creation_and_keep_their_agent_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let db = Database::open(&path).unwrap();
    let session = db
        .create_session(Some("locked".into()), "reader".into())
        .unwrap();
    assert_eq!(session.agent, "reader");
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert!(
        conn.execute("UPDATE sessions SET agent=NULL WHERE id='locked'", [])
            .is_err()
    );
    drop(conn);
    assert!(
        db.submit(
            Some(session.id.clone()),
            "researcher".into(),
            "hello".into(),
            60
        )
        .is_err()
    );
    assert!(db.runs(Some("locked")).unwrap().is_empty());
    let run = db
        .submit(Some(session.id), "reader".into(), "hello".into(), 60)
        .unwrap();
    db.cancel(&run.id).unwrap();
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.sessions().unwrap()[0].agent, "reader");
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
    db.create_session(Some("empty".into()), "default".into())
        .unwrap();
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
            .agent,
        "researcher"
    );
    assert_eq!(
        sessions.iter().find(|s| s.id == "empty").unwrap().agent,
        "default"
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

#[tokio::test]
async fn session_creation_requires_a_known_agent_before_any_runs() {
    let h = Harness::new(1).await;
    for (body, status) in [
        (
            json!({"id":"missing"}),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"id":"null", "agent":null}),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"id":"unknown", "agent":"missing"}),
            reqwest::StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = h
            .client
            .post(format!("{}/v1/sessions", h.url))
            .bearer_auth("test-token")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    assert!(h.db.sessions().unwrap().is_empty());
    let response = h
        .client
        .post(format!("{}/v1/sessions", h.url))
        .bearer_auth("test-token")
        .json(&json!({"id":"research", "agent":"researcher"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let session: runner::db::Session = response.json().await.unwrap();
    assert_eq!(session.agent, "researcher");
    assert!(
        h.db.submit(
            Some(session.id.clone()),
            "default".into(),
            "switch".into(),
            60
        )
        .is_err()
    );
    assert!(h.db.runs(Some(&session.id)).unwrap().is_empty());
    let run = h.submit(Some(&session.id), "hello", 5).await;
    assert_eq!(run.agent, "researcher");
    h.close().await;
}

impl Harness {
    async fn api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth("test-token");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or_default())
    }

    /// Creates a schedule over the API due an hour from now, then makes it
    /// due already, as if that hour had passed.
    async fn due_schedule(&self, id: &str, prompt: &str) -> Value {
        let at = (chrono::Utc::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        let (status, schedule) = self
            .api(
                reqwest::Method::POST,
                "/v1/schedules",
                Some(json!({"id":id, "agent":"default", "prompt":prompt, "at":at, "tz":"UTC"})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{schedule}");
        let conn = rusqlite::Connection::open(self._dir.path().join("state.sqlite3")).unwrap();
        conn.execute("UPDATE schedules SET next_fire_at=1 WHERE id=?1", [id])
            .unwrap();
        schedule
    }
}

/// What the harness's executor pins `agent` to: see `FakeExecutor::pin`.
fn pin_of(executor: &FakeExecutor) -> impl Fn(&str) -> Option<host::version::Pin> + '_ {
    |agent| executor.pin(agent)
}

#[tokio::test]
async fn a_one_off_schedule_fires_once_as_a_run_in_a_fresh_session() {
    let h = Harness::new(1).await;
    let schedule = h.due_schedule("brief", "morning brief").await;
    assert_eq!(schedule["status"], "active");
    assert_eq!(schedule["pinned_versions"], json!({"default":""}));
    assert_eq!(schedule["changed_agents"], json!([]));
    let runs = h.db.fire_due(db_now(), &pin_of(&h.executor)).unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.schedule_id.as_deref(), Some("brief"));
    assert_eq!(run.prompt, "morning brief");
    // Firing again, e.g. on the next tick or after a restart, enqueues nothing.
    assert!(
        h.db.fire_due(db_now(), &pin_of(&h.executor))
            .unwrap()
            .is_empty()
    );
    let run = h.wait_status(&run.id, "completed").await;
    let session =
        h.db.sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.id == run.session_id)
            .unwrap();
    assert_eq!(
        (
            session.agent.as_str(),
            session.running_runs,
            session.queued_runs
        ),
        ("default", 0, 0)
    );
    assert_eq!(h.db.runs(Some(&run.session_id)).unwrap().len(), 1);
    let (_, schedule) = h
        .api(reqwest::Method::GET, "/v1/schedules/brief", None)
        .await;
    assert_eq!(schedule["status"], "done");
    assert_eq!(schedule["next_fire_at"], Value::Null);
    let (_, history) = h
        .api(reqwest::Method::GET, "/v1/schedules/brief/history", None)
        .await;
    let kinds: Vec<_> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["kind"].clone())
        .collect();
    assert_eq!(kinds, ["created", "fired"]);
    assert_eq!(history[1]["data"]["run_id"], json!(run.id));
    h.close().await;
}

fn db_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn a_changed_agent_pauses_its_schedule_until_it_is_approved_again() {
    let h = Harness::new(1).await;
    h.due_schedule("brief", "morning brief").await;
    // An edit to the agent, e.g. a new tool, gives it a new version.
    h.executor
        .versions
        .lock()
        .unwrap()
        .insert("default".into(), "sha256:new".into());
    assert!(
        h.db.fire_due(db_now(), &pin_of(&h.executor))
            .unwrap()
            .is_empty()
    );
    let schedule = h.db.schedule("brief").unwrap().unwrap();
    assert_eq!(schedule.status, "paused");
    assert_eq!(
        schedule.paused_reason.as_deref(),
        Some("agent versions changed since approval: default")
    );
    assert!(h.db.runs(None).unwrap().is_empty());
    h.close().await;
}

#[tokio::test]
async fn resuming_needs_unchanged_agents_and_approving_re_pins() {
    let h = Harness::new(1).await;
    h.due_schedule("brief", "morning brief").await;
    let harness = &h;
    let post = |action: &str| {
        let path = format!("/v1/schedules/brief/{action}");
        async move { harness.api(reqwest::Method::POST, &path, None).await }
    };
    let (status, paused) = post("pause").await;
    assert_eq!(
        (status, paused["status"].as_str()),
        (StatusCode::OK, Some("paused"))
    );
    assert_eq!(post("pause").await.0, StatusCode::CONFLICT);
    // Paused schedules don't fire, even when due.
    assert!(
        h.db.fire_due(db_now(), &pin_of(&h.executor))
            .unwrap()
            .is_empty()
    );
    assert_eq!(post("resume").await.1["status"], "active");
    assert_eq!(post("pause").await.0, StatusCode::OK);

    // The service's own agents changing is what resume and approve check;
    // stand in for a config edit by repinning the stored schedule.
    let conn = rusqlite::Connection::open(h._dir.path().join("state.sqlite3")).unwrap();
    conn.execute(
        "UPDATE schedules SET pinned_versions='{\"default\":\"sha256:old\"}' WHERE id='brief'",
        [],
    )
    .unwrap();
    let (status, error) = post("resume").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        error["error"].as_str().unwrap().contains("approve"),
        "{error}"
    );
    let (_, listed) = h.api(reqwest::Method::GET, "/v1/schedules", None).await;
    assert_eq!(listed[0]["changed_agents"], json!(["default"]));

    let (status, approval) = post("approve").await;
    assert_eq!(status, StatusCode::OK, "{approval}");
    assert_eq!(approval["schedule"]["status"], "active");
    assert_eq!(
        approval["schedule"]["pinned_versions"],
        json!({"default":""})
    );
    assert_eq!(approval["schedule"]["changed_agents"], json!([]));
    assert_eq!(
        approval["changes"],
        json!([{"agent":"default", "from":"sha256:old", "to":"", "fields":null}])
    );
    let (_, history) = h
        .api(reqwest::Method::GET, "/v1/schedules/brief/history", None)
        .await;
    let kinds: Vec<_> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["kind"].clone())
        .collect();
    assert_eq!(
        kinds,
        ["created", "paused", "resumed", "paused", "approved"]
    );
    assert_eq!(
        h.db.fire_due(db_now(), &pin_of(&h.executor)).unwrap().len(),
        1
    );
    h.close().await;
}

#[tokio::test]
async fn a_run_queued_under_an_old_version_is_refused_and_fires_again_once_approved() {
    let h = Harness::new(1).await;
    h.due_schedule("brief", "morning brief").await;
    // Hold the worker's only slot so the occurrence stays queued.
    let blocker = h.submit(None, "wait", 60).await;
    h.wait_status(&blocker.id, "running").await;
    let run =
        h.db.fire_due(db_now(), &pin_of(&h.executor))
            .unwrap()
            .remove(0);
    // A restart under an edited config, between enqueueing and starting.
    h.executor
        .versions
        .lock()
        .unwrap()
        .insert("default".into(), "sha256:new".into());
    h.db.cancel(&blocker.id).unwrap();
    let refused = h.wait_status(&run.id, "failed").await;
    assert_eq!(
        refused.error.as_deref(),
        Some("agent versions changed since approval: default")
    );
    assert_eq!(
        *h.executor
            .sessions
            .lock()
            .unwrap()
            .get(&run.session_id)
            .unwrap_or(&0),
        0
    );
    let schedule = h.db.schedule("brief").unwrap().unwrap();
    assert_eq!(schedule.status, "paused");
    assert_eq!(schedule.next_fire_at, Some(1));
    // Approved for what it runs now, it fires its one occurrence again.
    h.executor.versions.lock().unwrap().clear();
    let conn = rusqlite::Connection::open(h._dir.path().join("state.sqlite3")).unwrap();
    conn.execute(
        "UPDATE schedules SET pinned_versions='{\"default\":\"sha256:new\"}' WHERE id='brief'",
        [],
    )
    .unwrap();
    let (status, _) = h
        .api(reqwest::Method::POST, "/v1/schedules/brief/approve", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let again =
        h.db.fire_due(db_now(), &pin_of(&h.executor))
            .unwrap()
            .remove(0);
    h.wait_status(&again.id, "completed").await;
    let (_, history) = h
        .api(reqwest::Method::GET, "/v1/schedules/brief/history", None)
        .await;
    let kinds: Vec<_> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["kind"].clone())
        .collect();
    assert_eq!(kinds, ["created", "fired", "refused", "approved", "fired"]);
    h.close().await;
}

#[tokio::test]
async fn pausing_or_deleting_a_schedule_cancels_its_queued_occurrence() {
    let h = Harness::new(1).await;
    let blocker = h.submit(None, "wait", 60).await;
    h.wait_status(&blocker.id, "running").await;
    h.due_schedule("brief", "morning brief").await;
    let run =
        h.db.fire_due(db_now(), &pin_of(&h.executor))
            .unwrap()
            .remove(0);
    // A fired one-off is done, but can still be deleted.
    let (status, deleted) = h
        .api(reqwest::Method::DELETE, "/v1/schedules/brief", None)
        .await;
    assert_eq!(
        (status, deleted["status"].as_str()),
        (StatusCode::OK, Some("deleted"))
    );
    assert_eq!(h.db.run(&run.id).unwrap().unwrap().status, "cancelled");
    let (_, listed) = h.api(reqwest::Method::GET, "/v1/schedules", None).await;
    assert_eq!(listed, json!([]));
    assert_eq!(
        h.api(reqwest::Method::POST, "/v1/schedules/brief/approve", None)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.api(reqwest::Method::GET, "/v1/schedules/nope", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    h.db.cancel(&blocker.id).unwrap();
    h.close().await;
}

impl Harness {
    /// Creates a schedule over the API that recurs every minute, in UTC.
    async fn recurring_schedule(&self, id: &str, prompt: &str) -> Value {
        let (status, schedule) = self
            .api(
                reqwest::Method::POST,
                "/v1/schedules",
                Some(json!({"id":id, "agent":"default", "prompt":prompt, "cron":"* * * * *", "tz":"UTC"})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{schedule}");
        schedule
    }

    /// What `id`'s history records, as `(kind, reason)` pairs.
    async fn schedule_kinds(&self, id: &str) -> Vec<(String, Option<String>)> {
        let (_, history) = self
            .api(
                reqwest::Method::GET,
                &format!("/v1/schedules/{id}/history"),
                None,
            )
            .await;
        history
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["data"]["kind"].as_str().unwrap().to_owned(),
                    e["data"]["reason"].as_str().map(String::from),
                )
            })
            .collect()
    }
}

#[tokio::test]
async fn a_recurring_schedule_fires_each_occurrence_and_stays_active() {
    let h = Harness::new(1).await;
    let schedule = h.recurring_schedule("brief", "minutely brief").await;
    assert_eq!(schedule["cron"], "* * * * *");
    let first = schedule["next_fire_at"].as_i64().unwrap();
    assert!(first > db_now() && first % 60_000 == 0, "{schedule}");
    assert!(
        schedule["at"].as_str().unwrap().ends_with(":00"),
        "{schedule}"
    );
    for (n, due) in [first, first + 60_000].into_iter().enumerate() {
        // Not due a millisecond early.
        assert!(
            h.db.fire_due(due - 1, &pin_of(&h.executor))
                .unwrap()
                .is_empty()
        );
        let run = h.db.fire_due(due, &pin_of(&h.executor)).unwrap().remove(0);
        assert_eq!(run.prompt, "minutely brief");
        let schedule = h.db.schedule("brief").unwrap().unwrap();
        assert_eq!(
            (schedule.status.as_str(), schedule.next_fire_at),
            ("active", Some(due + 60_000)),
            "occurrence {n}"
        );
        h.wait_status(&run.id, "completed").await;
    }
    // Each occurrence ran in a session of its own.
    let sessions: std::collections::HashSet<_> =
        h.db.runs(None)
            .unwrap()
            .into_iter()
            .map(|r| r.session_id)
            .collect();
    assert_eq!(sessions.len(), 2);
    assert_eq!(
        h.schedule_kinds("brief").await,
        [
            ("created".into(), None),
            ("fired".into(), None),
            ("fired".into(), None)
        ]
    );
    h.close().await;
}

#[tokio::test]
async fn a_recurring_schedule_skips_missed_occurrences_and_ones_its_last_run_overlaps() {
    let h = Harness::new(1).await;
    let blocker = h.submit(None, "wait", 60).await;
    h.wait_status(&blocker.id, "running").await;
    let first = h.recurring_schedule("brief", "brief").await["next_fire_at"]
        .as_i64()
        .unwrap();
    // A few seconds late is still on time.
    let run =
        h.db.fire_due(first + 5_000, &pin_of(&h.executor))
            .unwrap()
            .remove(0);
    // Its run is still queued behind the blocker when the next one is due.
    assert!(
        h.db.fire_due(first + 60_000, &pin_of(&h.executor))
            .unwrap()
            .is_empty()
    );
    h.db.cancel(&blocker.id).unwrap();
    h.wait_status(&run.id, "completed").await;
    // The daemon was stopped for ten minutes: nothing is made up, and the
    // schedule picks up at the next occurrence after it starts.
    let restarted = first + 60_000 * 11 + 30_000;
    assert!(
        h.db.fire_due(restarted, &pin_of(&h.executor))
            .unwrap()
            .is_empty()
    );
    let schedule = h.db.schedule("brief").unwrap().unwrap();
    assert_eq!(schedule.next_fire_at, Some(first + 60_000 * 12));
    assert_eq!(schedule.status, "active");
    assert_eq!(
        h.schedule_kinds("brief").await,
        [
            ("created".into(), None),
            ("fired".into(), None),
            ("skipped".into(), Some("previous run unfinished".into())),
            ("skipped".into(), Some("missed".into())),
        ]
    );
    assert_eq!(h.db.runs(None).unwrap().len(), 2);
    h.close().await;
}

#[tokio::test]
async fn a_refused_recurring_run_moves_its_schedule_to_the_next_occurrence() {
    let h = Harness::new(1).await;
    let first = h.recurring_schedule("brief", "brief").await["next_fire_at"]
        .as_i64()
        .unwrap();
    let blocker = h.submit(None, "wait", 60).await;
    h.wait_status(&blocker.id, "running").await;
    let run =
        h.db.fire_due(first, &pin_of(&h.executor))
            .unwrap()
            .remove(0);
    // A restart under an edited config, between enqueueing and starting.
    h.executor
        .versions
        .lock()
        .unwrap()
        .insert("default".into(), "sha256:new".into());
    h.db.cancel(&blocker.id).unwrap();
    h.wait_status(&run.id, "failed").await;
    let schedule = h.db.schedule("brief").unwrap().unwrap();
    assert_eq!(schedule.status, "paused");
    assert_eq!(schedule.next_fire_at, Some(first + 60_000));
    h.close().await;
}

#[tokio::test]
async fn schedules_need_a_known_agent_an_explicit_zone_and_a_future_time() {
    let h = Harness::new(1).await;
    let create = |body: Value| h.api(reqwest::Method::POST, "/v1/schedules", Some(body));
    let ok =
        json!({"agent":"default", "prompt":"p", "at":"2999-01-01T07:00", "tz":"Europe/Warsaw"});
    let with = |key: &str, value: Value| {
        let mut body = ok.clone();
        body[key] = value;
        body
    };
    for (body, needle) in [
        (with("agent", json!("nobody")), "unknown agent"),
        (with("tz", json!("Mars/Olympus")), "unknown time zone"),
        (with("at", json!("2000-01-01T07:00")), "already passed"),
        (with("at", json!("soon")), "invalid time"),
        (with("prompt", json!(" ")), "prompt"),
        (with("id", json!("../x")), "invalid schedule id"),
        (with("cron", json!("0 7 * * *")), "give one of"),
        (
            json!({"agent":"default", "prompt":"p", "cron":"0 0 7 * * *", "tz":"UTC"}),
            "invalid cron expression",
        ),
        (
            json!({"agent":"default", "prompt":"p", "cron":"0 0 30 2 *", "tz":"UTC"}),
            "never fires",
        ),
    ] {
        let (status, error) = create(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
        assert!(error["error"].as_str().unwrap().contains(needle), "{error}");
    }
    let mut no_zone = ok.clone();
    no_zone.as_object_mut().unwrap().remove("tz");
    assert_eq!(create(no_zone).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, schedule) = create(with("id", json!("brief"))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(schedule["at"], "2999-01-01T07:00:00");
    assert_eq!(
        create(with("id", json!("brief"))).await.0,
        StatusCode::BAD_REQUEST
    );
    h.close().await;
}

#[test]
fn a_version_6_database_gains_schedules_and_keeps_its_agent_versions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let db = Database::open(&path).unwrap();
    let run = db
        .submit(None, "default".into(), "before".into(), 60)
        .unwrap();
    db.record_agent_version(&run.id, "sha256:01", "default", &json!({"agent":"default"}))
        .unwrap();
    drop(db);
    // What a daemon from before schedules left behind.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP INDEX runs_by_occurrence;
         ALTER TABLE runs DROP COLUMN occurrence_id;
         ALTER TABLE runs DROP COLUMN pinned_versions;
         ALTER TABLE runs DROP COLUMN schedule_id;
         DROP TABLE schedule_events; DROP TABLE schedules;
         ALTER TABLE agent_versions RENAME COLUMN first_seen_at TO first_used_at;
         PRAGMA user_version=6;",
    )
    .unwrap();
    drop(conn);
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.agent_version("sha256:01").unwrap().unwrap().agent,
        "default"
    );
    assert_eq!(db.run(&run.id).unwrap().unwrap().schedule_id, None);
    assert!(db.schedules().unwrap().is_empty());
    drop(db);
    assert!(Database::open(&path).is_ok());
}

#[test]
fn a_version_7_database_keeps_its_one_off_schedules() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let db = Database::open(&path).unwrap();
    db.create_schedule(
        Some("brief".into()),
        "default".into(),
        "p".into(),
        "2999-01-01T07:00:00".into(),
        "UTC".into(),
        None,
        1,
        60,
        Default::default(),
        &[],
    )
    .unwrap();
    drop(db);
    // What a daemon from before recurring schedules left behind.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("ALTER TABLE schedules DROP COLUMN cron; PRAGMA user_version=7;")
        .unwrap();
    drop(conn);
    let db = Database::open(&path).unwrap();
    let schedule = db.schedule("brief").unwrap().unwrap();
    assert_eq!((schedule.cron, schedule.next_fire_at), (None, Some(1)));
    drop(db);
    assert!(Database::open(&path).is_ok());
}

#[tokio::test]
async fn the_scheduler_pauses_changed_schedules_at_startup_and_fires_due_ones() {
    let h = Harness::new(1).await;
    h.due_schedule("brief", "morning brief").await;
    h.due_schedule("changed", "other").await;
    let conn = rusqlite::Connection::open(h._dir.path().join("state.sqlite3")).unwrap();
    // Due an hour from now, so only the startup check can pause it.
    conn.execute(
        "UPDATE schedules SET next_fire_at=?1, pinned_versions='{\"default\":\"sha256:old\"}' WHERE id='changed'",
        [db_now() + 3_600_000],
    )
    .unwrap();
    let stop = CancellationToken::new();
    let scheduler = tokio::spawn(runner::schedule::scheduler(
        h.db.clone(),
        h.executor.clone(),
        stop.clone(),
    ));
    let run = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(run) =
                h.db.runs(None)
                    .unwrap()
                    .into_iter()
                    .find(|r| r.schedule_id.is_some())
            {
                break run;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(run.schedule_id.as_deref(), Some("brief"));
    h.wait_status(&run.id, "completed").await;
    assert_eq!(h.db.schedule("changed").unwrap().unwrap().status, "paused");
    stop.cancel();
    scheduler.await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn a_schedule_can_fire_a_delay_from_now_instead_of_at_a_time() {
    let h = Harness::new(1).await;
    let create = |body: Value| h.api(reqwest::Method::POST, "/v1/schedules", Some(body));
    let before = db_now();
    let (status, schedule) = create(
        json!({"id":"soon", "agent":"default", "prompt":"p", "in_secs":2, "tz":"Europe/Warsaw"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{schedule}");
    let fires = schedule["next_fire_at"].as_i64().unwrap();
    assert!(
        (before + 2000..db_now() + 2000).contains(&fires),
        "{schedule}"
    );
    assert!(
        h.db.fire_due(db_now(), &pin_of(&h.executor))
            .unwrap()
            .is_empty()
    );
    assert_eq!(h.db.fire_due(fires, &pin_of(&h.executor)).unwrap().len(), 1);
    for body in [
        json!({"agent":"default", "prompt":"p", "tz":"UTC"}),
        json!({"agent":"default", "prompt":"p", "tz":"UTC", "in_secs":2, "at":"2999-01-01T07:00"}),
        json!({"agent":"default", "prompt":"p", "tz":"UTC", "in_secs":0}),
    ] {
        let (status, error) = create(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    }
    h.close().await;
}

#[test]
fn a_runs_tree_holds_it_and_every_run_it_delegated_to_but_not_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("state.sqlite3")).unwrap();
    db.submit(None, "default".into(), "root".into(), 60)
        .unwrap();
    let root = db.claim().unwrap().unwrap();
    let child = db.delegate(&root.id, "default", "child", None).unwrap();
    // Only a running run can delegate.
    assert_eq!(db.claim().unwrap().unwrap().id, child.id);
    let grandchild = db
        .delegate(&child.id, "default", "grandchild", None)
        .unwrap();
    let unrelated = db
        .submit(None, "default".into(), "other".into(), 60)
        .unwrap();
    for (run, seq) in [
        (&child.id, 1),
        (&root.id, 1),
        (&grandchild.id, 1),
        (&unrelated.id, 1),
        (&root.id, 2),
    ] {
        db.audit_event(run, json!({"seq":seq, "action":"turn.started"}))
            .unwrap();
    }
    let (runs, records) = db.tree_audit(&root.id).unwrap().unwrap();
    let ids: Vec<_> = runs.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, [&root.id, &child.id, &grandchild.id]);
    // A run at a time, ordered by each one's first record.
    let order: Vec<_> = records.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(order, [&child.id, &root.id, &root.id, &grandchild.id]);
    let (runs, records) = db.tree_audit(&child.id).unwrap().unwrap();
    assert_eq!((runs.len(), records.len()), (2, 2));
    assert!(db.tree_audit("nope").unwrap().is_none());
}

#[tokio::test]
async fn a_runs_summary_is_served_from_its_audit_records() {
    let h = Harness::new(1).await;
    let run = h.submit(None, "hello", 60).await;
    let run = h.wait_status(&run.id, "completed").await;
    for data in [
        json!({"seq":1, "ts_ms":100, "action":"turn.started"}),
        json!({"seq":2, "ts_ms":150, "action":"http.request",
            "component":{"kind":"provider","name":"test"}, "decision":"denied",
            "outcome":"not_allowed_host", "host":"evil.example"}),
        json!({"seq":3, "ts_ms":400, "action":"turn.finished", "outcome":"completed",
            "records":3, "refused":0}),
    ] {
        h.db.audit_event(&run.id, data).unwrap();
    }
    let (status, summary) = h
        .api(
            reqwest::Method::GET,
            &format!("/v1/runs/{}/summary", run.id),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    assert_eq!(summary["status"], "completed");
    assert_eq!(summary["duration_ms"], 300);
    assert_eq!(
        summary["hosts"],
        json!([{"kind":"provider", "component":"test", "host":"evil.example",
            "allowed":0, "denied":1}])
    );
    assert_eq!(summary["denials"]["total"], 1);
    assert_eq!(summary["trail_problems"], json!([]));
    assert_eq!(
        h.api(reqwest::Method::GET, "/v1/runs/nope/summary", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    h.close().await;
}
