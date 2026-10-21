//! Build fixtures first; see README's verification commands.
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use runner::{
    client::Client,
    db::{Database, Run},
    service::{WasmExecutor, worker},
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Fixture {
    dir: tempfile::TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_tool_timeout(1)
    }

    fn with_tool_timeout(tool_timeout: u64) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target"));
        let target = if target.is_absolute() {
            target
        } else {
            root.join(target)
        }
        .join("wasm32-wasip2/debug");
        for name in ["agent", "test_provider", "test_tool", "fs_store"] {
            assert!(
                target.join(format!("{name}.wasm")).exists(),
                "build WASM fixtures first; see README"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(tools.join("allowed.txt"), "allowed").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", tools.join("escape")).unwrap();
        let config = dir.path().join("runner.toml");
        let quote = |path: PathBuf| serde_json::to_string(path.to_str().unwrap()).unwrap();
        std::fs::write(
            &config,
            format!(
                r#"
agent = {}
[provider]
path = {}
model = "fixture"
allowed_hosts = ["fixture.invalid"]
headers = {{ x-api-key = {{ value = "host-only-fixture-key", hosts = ["fixture.invalid"] }} }}
[[tools]]
path = {}
dir = {}
timeout_secs = {tool_timeout}
fresh_per_call = true
[session_store]
path = {}
dir = {}
dir_writable = true
"#,
                quote(target.join("agent.wasm")),
                quote(target.join("test_provider.wasm")),
                quote(target.join("test_tool.wasm")),
                quote(tools),
                quote(target.join("fs_store.wasm")),
                quote(dir.path().join("sessions"))
            ),
        )
        .unwrap();
        Self { dir, config }
    }

    fn runtime(&self) -> host::Runtime {
        host::Runtime::new(self.config.to_str().unwrap()).unwrap()
    }
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn real_components_stream_save_resume_and_enforce_capabilities() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = events.clone();
    let sink: host::EventSink = Arc::new(move |value| captured.lock().unwrap().push(value));
    runtime
        .run_turn("conversation", "hello", sink.clone())
        .await
        .unwrap();
    runtime
        .run_turn("conversation", "again", sink.clone())
        .await
        .unwrap();
    runtime
        .run_turn("other", "separate", sink.clone())
        .await
        .unwrap();
    let output: String = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert_eq!(output, "turn 1: helloturn 2: againturn 1: separate");
    assert!(events.lock().unwrap().iter().any(|e| e["kind"] == "usage"));
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "turn_saved")
            .count(),
        3
    );
    events.lock().unwrap().clear();
    runtime.run_turn("probe", "tool:probe", sink).await.unwrap();
    let events = events.lock().unwrap();
    let text: String = events.iter().filter_map(|e| e["text"].as_str()).collect();
    let probe: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        probe,
        json!({"env_count":0,"inside_read":true,"outside_read":false,
        "parent_read":false,"symlink_read":false,"write":false,"socket":false})
    );
    assert!(events.iter().any(|e| e["kind"] == "tool_started"));
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "tool_finished" && e["ok"] == true)
    );
    assert!(
        !serde_json::to_string(&*events)
            .unwrap()
            .contains("host-only-fixture-key")
    );
    assert!(!fixture.dir.path().join("tools/changed.txt").exists());
}

async fn wait(db: &Database, id: &str, state: &str) -> Run {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let run = db.run(id).unwrap().unwrap();
            if run.status == state {
                break run;
            }
            if run.terminal() {
                panic!("unexpected terminal run: {run:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("waiting for {state}: {:?}", db.run(id)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires WASM fixture build"]
async fn real_provider_and_tools_are_interruptible_and_do_not_block_other_runs() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let db = Database::open(&fixture.dir.path().join("state.sqlite3")).unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(
        db.clone(),
        Arc::new(WasmExecutor(runtime)),
        2,
        stop.clone(),
    ));
    for prompt in ["cpu", "wait", "tool:spin", "tool:sleep"] {
        let busy = db.submit(None, prompt.into(), 10).unwrap();
        wait(&db, &busy.id, "running").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let independent = db.submit(None, "independent".into(), 5).unwrap();
        wait(&db, &independent.id, "completed").await;
        db.cancel(&busy.id).unwrap();
        wait(&db, &busy.id, "cancelled").await;
    }
    for prompt in ["cpu", "wait"] {
        let timed = db.submit(None, prompt.into(), 1).unwrap();
        assert_eq!(
            wait(&db, &timed.id, "failed").await.error.as_deref(),
            Some("run deadline exceeded")
        );
    }
    // A tool timeout becomes a tool result; the next call uses a fresh store.
    for prompt in ["tool:spin", "tool:sleep", "tool:hello"] {
        let run = db.submit(None, prompt.into(), 5).unwrap();
        wait(&db, &run.id, "completed").await;
        let text: String = db
            .events(&run.id, 0)
            .unwrap()
            .iter()
            .filter_map(|e| e.data["text"].as_str())
            .collect();
        if prompt == "tool:hello" {
            assert_eq!(text, "hello");
        } else {
            assert!(text.contains("timed out after 1s"), "{text}");
        }
    }
    stop.cancel();
    task.await.unwrap().unwrap();
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(fixture: &Fixture, data: &std::path::Path) -> Daemon {
    Daemon(
        Command::new(env!("CARGO_BIN_EXE_runner"))
            .arg("--data-dir")
            .arg(data)
            .arg("serve")
            .arg("--config")
            .arg(&fixture.config)
            .arg("--concurrency")
            .arg("1")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}

async fn connect(data: &std::path::Path) -> Client {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(client) = Client::connect(data)
                && client.runs().await.is_ok()
            {
                break client;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("daemon started")
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn daemon_cli_and_crash_recovery_work_end_to_end() {
    let fixture = Fixture::new();
    let data = fixture.dir.path().join("service");
    let daemon = start(&fixture, &data);
    let client = connect(&data).await;
    let output = Command::new(env!("CARGO_BIN_EXE_runner"))
        .arg("--data-dir")
        .arg(&data)
        .args(["run", "hello", "--detach"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let id = String::from_utf8(output.stdout).unwrap().trim().to_string();
    client.watch(&id, 0, false).await.unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_runner"))
        .arg("--data-dir")
        .arg(&data)
        .args(["runs", "watch", &id, "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("completed")
    );
    let interrupted = client.submit(None, "wait".into(), 60).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.run(&interrupted.id).await.unwrap().status != "running" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let queued = client
        .submit(None, "after restart".into(), 5)
        .await
        .unwrap();
    drop(daemon); // SIGKILL, deliberately bypassing graceful shutdown.
    let _restarted = start(&fixture, &data);
    let client = connect(&data).await;
    assert_eq!(
        client.run(&interrupted.id).await.unwrap().status,
        "interrupted"
    );
    client.watch(&queued.id, 0, false).await.unwrap();
    assert_eq!(client.run(&id).await.unwrap().status, "completed");
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn failed_turns_explain_why_without_leaking_credentials() {
    let fixture = Fixture::new();
    let db = Database::open(&fixture.dir.path().join("state.sqlite3")).unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(
        db.clone(),
        Arc::new(WasmExecutor(fixture.runtime())),
        1,
        stop.clone(),
    ));
    for (prompt, error) in [
        ("api-error", "API error 401: invalid x-api-key [redacted]"),
        (
            "cutoff",
            "reply cut off: hit the 16384 token limit, so this turn wasn't saved",
        ),
        ("fail", "internal error: fixture failure"),
    ] {
        let run = db.submit(None, prompt.into(), 5).unwrap();
        assert_eq!(
            wait(&db, &run.id, "failed").await.error.as_deref(),
            Some(error)
        );
        assert!(
            !db.events(&run.id, 0)
                .unwrap()
                .iter()
                .any(|event| event.data["kind"] == "turn_saved")
        );
    }
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn run_deadline_preempts_a_longer_cpu_bound_tool_timeout() {
    let fixture = Fixture::with_tool_timeout(60);
    let db = Database::open(&fixture.dir.path().join("state.sqlite3")).unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(
        db.clone(),
        Arc::new(WasmExecutor(fixture.runtime())),
        1,
        stop.clone(),
    ));
    let run = db.submit(None, "tool:spin".into(), 1).unwrap();
    assert_eq!(
        wait(&db, &run.id, "failed").await.error.as_deref(),
        Some("run deadline exceeded")
    );
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn a_failed_session_save_does_not_report_a_saved_turn() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.dir.path().join("sessions")).unwrap();
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("dir_writable = true", "dir_writable = false");
    std::fs::write(&fixture.config, config).unwrap();
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = events.clone();
    let sink: host::EventSink = Arc::new(move |event| captured.lock().unwrap().push(event));
    let error = fixture
        .runtime()
        .run_turn("read-only", "hello", sink)
        .await
        .unwrap_err();
    assert!(
        error.downcast_ref::<host::TurnFailed>().is_some(),
        "{error:#}"
    );
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| event["kind"] == "text"));
    assert!(!events.iter().any(|event| event["kind"] == "turn_saved"));
    assert!(!fixture.dir.path().join("sessions/read-only.json").exists());
}

async fn turn_output(runtime: &host::Runtime, session: &str, prompt: &str) -> String {
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = events.clone();
    let sink: host::EventSink = Arc::new(move |event| captured.lock().unwrap().push(event));
    runtime.run_turn(session, prompt, sink).await.unwrap();
    events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| event["text"].as_str())
        .collect()
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn component_state_survives_turns_and_runtime_clones_but_isolates_sessions() {
    let fixture = Fixture::new();
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("fresh_per_call = true", "fresh_per_call = false");
    std::fs::write(&fixture.config, config).unwrap();
    let runtime = fixture.runtime();
    assert_eq!(turn_output(&runtime, "tools", "tool:counter").await, "1");
    assert_eq!(
        turn_output(&runtime.clone(), "tools", "tool:counter").await,
        "2"
    );
    assert_eq!(
        turn_output(&runtime, "other-tools", "tool:counter").await,
        "1"
    );
    assert_eq!(
        turn_output(&runtime, "provider", "provider-counter").await,
        "1"
    );
    assert_eq!(
        turn_output(&runtime.clone(), "provider", "provider-counter").await,
        "2"
    );
    assert_eq!(
        turn_output(&runtime, "other-provider", "provider-counter").await,
        "1"
    );
    // Application errors return normally from the component and preserve its state.
    assert!(
        runtime
            .run_turn("tools", "api-error", Arc::new(|_| {}))
            .await
            .is_err()
    );
    assert_eq!(turn_output(&runtime, "tools", "tool:counter").await, "3");
    assert!(
        runtime
            .run_turn("provider", "api-error", Arc::new(|_| {}))
            .await
            .is_err()
    );
    assert_eq!(
        turn_output(&runtime, "provider", "provider-counter").await,
        "4"
    );
    // The runtime itself serializes same-session callers, not just the queue worker.
    let (a, b) = tokio::join!(
        turn_output(&runtime, "tools", "tool:counter"),
        turn_output(&runtime, "tools", "tool:counter"),
    );
    let mut outputs = [a, b];
    outputs.sort();
    assert_eq!(outputs, ["4", "5"]);
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn fresh_per_call_tools_still_reset_while_the_provider_is_reused() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    assert_eq!(turn_output(&runtime, "fresh", "tool:counter").await, "1");
    assert_eq!(turn_output(&runtime, "fresh", "tool:counter").await, "1");
    // Each tool turn makes two provider requests, all in the same provider instance.
    assert_eq!(
        turn_output(&runtime, "fresh", "provider-counter").await,
        "5"
    );
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn interrupted_components_are_recreated_before_reuse() {
    let fixture = Fixture::new();
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("fresh_per_call = true", "fresh_per_call = false");
    std::fs::write(&fixture.config, config).unwrap();
    let runtime = fixture.runtime();
    assert_eq!(turn_output(&runtime, "recover", "tool:counter").await, "1");
    assert!(
        turn_output(&runtime, "recover", "tool:spin")
            .await
            .contains("timed out")
    );
    assert_eq!(turn_output(&runtime, "recover", "tool:counter").await, "1");
    let trapped = turn_output(&runtime, "recover", "tool:trap").await;
    assert!(trapped.starts_with("error:"), "{trapped}");
    assert_eq!(turn_output(&runtime, "recover", "tool:counter").await, "1");
    assert!(
        runtime
            .run_turn("recover", "provider-trap", Arc::new(|_| {}))
            .await
            .is_err()
    );
    // A provider trap resets that provider, preserving the unaffected tool.
    assert_eq!(
        turn_output(&runtime, "recover", "provider-counter").await,
        "1"
    );
    assert_eq!(turn_output(&runtime, "recover", "tool:counter").await, "2");
    // Dropping an in-flight turn discards its stores, including the busy provider.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            runtime.run_turn("recover", "wait", Arc::new(|_| {}))
        )
        .await
        .is_err()
    );
    assert_eq!(
        turn_output(&runtime, "recover", "provider-counter").await,
        "1"
    );
    assert_eq!(turn_output(&runtime, "recover", "tool:counter").await, "1");
}
