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
[tools.fixture]
path = {}
dir = {}
timeout_secs = {tool_timeout}
fresh_per_call = true
[session_store]
path = {}
dir = {}
dir_writable = true
[agents.default]
tools = ["fixture"]
[agents.notools]
model = "fixture"
system_prompt = "Answer without tools."
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

    async fn runtime(&self) -> host::Runtime {
        host::Runtime::new(self.config.to_str().unwrap())
            .await
            .unwrap()
    }
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn real_components_stream_save_resume_and_enforce_capabilities() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime().await;
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = events.clone();
    let sink: host::EventSink = Arc::new(move |value| captured.lock().unwrap().push(value));
    runtime
        .run_turn("default", "conversation", "hello", sink.clone())
        .await
        .unwrap();
    runtime
        .run_turn("default", "conversation", "again", sink.clone())
        .await
        .unwrap();
    runtime
        .run_turn("default", "other", "separate", sink.clone())
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
    runtime
        .run_turn("default", "probe", "tool:probe", sink)
        .await
        .unwrap();
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

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn agent_definitions_only_get_the_tools_they_name() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime().await;
    assert_eq!(runtime.agents(), ["default", "notools"]);
    let run = async |agent: &str| {
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = events.clone();
        let sink: host::EventSink = Arc::new(move |value| captured.lock().unwrap().push(value));
        runtime
            .run_turn(agent, agent, "tool:probe", sink)
            .await
            .unwrap();
        events.lock().unwrap().clone()
    };
    let default = run("default").await;
    assert!(
        default
            .iter()
            .any(|e| e["kind"] == "tool_finished" && e["ok"] == true)
    );
    let notools = run("notools").await;
    let finished = notools
        .iter()
        .find(|e| e["kind"] == "tool_finished")
        .unwrap();
    assert_eq!(finished["error"], "no such tool: fixture");
    // A definition's system prompt replaces the agent component's own.
    let system = async |agent: &str| {
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = events.clone();
        let sink: host::EventSink = Arc::new(move |value| captured.lock().unwrap().push(value));
        runtime
            .run_turn(agent, &format!("{agent}-system"), "system-prompt", sink)
            .await
            .unwrap();
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| e["text"].as_str())
            .collect::<String>()
    };
    assert_eq!(system("notools").await, "Answer without tools.");
    assert_eq!(system("default").await, "You are a helpful assistant.");
    let error = runtime
        .run_turn("nope", "s", "hello", Arc::new(|_| {}))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("unknown agent 'nope'"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires WASM fixture build"]
async fn real_provider_and_tools_are_interruptible_and_do_not_block_other_runs() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime().await;
    let db = Database::open(&fixture.dir.path().join("state.sqlite3")).unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(
        db.clone(),
        Arc::new(WasmExecutor(runtime)),
        2,
        stop.clone(),
    ));
    for prompt in ["cpu", "wait", "tool:spin", "tool:sleep"] {
        let busy = db
            .submit(None, "default".into(), prompt.into(), 10)
            .unwrap();
        wait(&db, &busy.id, "running").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let independent = db
            .submit(None, "default".into(), "independent".into(), 5)
            .unwrap();
        wait(&db, &independent.id, "completed").await;
        db.cancel(&busy.id).unwrap();
        wait(&db, &busy.id, "cancelled").await;
    }
    for prompt in ["cpu", "wait"] {
        let timed = db.submit(None, "default".into(), prompt.into(), 1).unwrap();
        assert_eq!(
            wait(&db, &timed.id, "failed").await.error.as_deref(),
            Some("run deadline exceeded")
        );
    }
    // A tool timeout becomes a tool result; the next call uses a fresh store.
    for prompt in ["tool:spin", "tool:sleep", "tool:hello"] {
        let run = db.submit(None, "default".into(), prompt.into(), 5).unwrap();
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
                && client.runs(None).await.is_ok()
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
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("agent default · session "), "{stderr}");
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
    let interrupted = client.submit(None, None, "wait".into(), 60).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.run(&interrupted.id).await.unwrap().status != "running" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let queued = client
        .submit(None, None, "after restart".into(), 5)
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

/// `runner chat` with `input` piped in, so its output is plain text.
fn chat(data: &std::path::Path, args: &[&str], input: &str) -> String {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_runner"))
        .arg("--data-dir")
        .arg(data)
        .arg("chat")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn reconnecting_to_a_chat_replays_its_saved_turns() {
    let fixture = Fixture::new();
    let data = fixture.dir.path().join("service");
    let _daemon = start(&fixture, &data);
    connect(&data).await;
    let first = chat(
        &data,
        &["--session", "trip"],
        "hello\ntool:probe\nfail\nagain\n",
    );
    assert!(first.contains("turn 1: hello"), "{first}");
    // Ending a chat says how to resume it, with the whole session ID.
    assert!(
        first.ends_with("\nresume with: runner chat --session trip\n"),
        "{first}"
    );
    let resumed = chat(&data, &["--session", "trip"], "");
    let history = resumed
        .split_once("resumed session · 3 earlier turns\n")
        .unwrap_or_else(|| panic!("{resumed}"))
        .1;
    // Every saved turn in order, with its prompt; no live timings.
    let hello = history.find("> hello\nturn 1: hello").unwrap();
    let probe = history.find("> tool:probe\n").unwrap();
    let tool = history.find("⚙ fixture(probe) ✓").unwrap();
    let again = history.find("> again\nturn 3: again").unwrap();
    assert!(hello < probe && probe < tool && tool < again, "{history}");
    // The failed turn was never saved, so the model doesn't remember it either.
    assert!(!history.contains("fail"), "{history}");
    assert!(!history.contains("Thought for"), "{history}");
    // A generated session ID is printed whole on exit too.
    let fresh = chat(&data, &[], "hi\n");
    let id = fresh
        .split(" · session ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    assert_eq!(id.len(), 36, "{fresh}");
    assert!(
        fresh.ends_with(&format!("resume with: runner chat --session {id}\n")),
        "{fresh}"
    );
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn chat_names_its_agent_and_rejects_unknown_ones() {
    let fixture = Fixture::new();
    let data = fixture.dir.path().join("service");
    let _daemon = start(&fixture, &data);
    let client = connect(&data).await;
    let named = chat(&data, &["--session", "a", "--agent", "notools"], "hi\n");
    assert!(
        named.starts_with("runner · agent notools · session a · "),
        "{named}"
    );
    let default = chat(&data, &["--session", "b"], "");
    assert!(
        default.starts_with("runner · agent default · session b · "),
        "{default}"
    );
    let run = client.runs(Some("a")).await.unwrap();
    assert_eq!(run[0].agent, "notools");

    let output = Command::new(env!("CARGO_BIN_EXE_runner"))
        .arg("--data-dir")
        .arg(&data)
        .args(["chat", "--session", "c", "--agent", "nope"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("unknown agent 'nope'; the daemon has: default, notools"),
        "{stderr}"
    );
    // Rejected before the chat started, so no session was made for it.
    let sessions = client.sessions().await.unwrap();
    assert!(!sessions.iter().any(|session| session.id == "c"));
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn ctrl_c_at_the_prompt_ends_the_chat() {
    let fixture = Fixture::new();
    let data = fixture.dir.path().join("service");
    let _daemon = start(&fixture, &data);
    connect(&data).await;
    // Stdin stays open with nothing typed, like a terminal waiting at the prompt.
    let mut child = Command::new(env!("CARGO_BIN_EXE_runner"))
        .arg("--data-dir")
        .arg(&data)
        .args(["chat", "--session", "interrupted"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take();
    // Wait for the prompt, so the chat is reading stdin and handling Ctrl-C.
    let mut stdout = child.stdout.take().unwrap();
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("> ") {
        let mut byte = [0];
        std::io::Read::read_exact(&mut stdout, &mut byte).unwrap();
        seen.push(byte[0]);
    }
    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let exited = tokio::task::spawn_blocking(move || {
        for _ in 0..50 {
            if let Some(status) = child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        child.kill().unwrap();
        None
    })
    .await
    .unwrap();
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest).unwrap();
    assert!(
        exited.is_some(),
        "chat didn't exit within 5s of Ctrl-C: {rest}"
    );
    assert!(
        rest.contains("resume with: runner chat --session interrupted"),
        "{rest}"
    );
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn failed_turns_explain_why_without_leaking_credentials() {
    let fixture = Fixture::new();
    let db = Database::open(&fixture.dir.path().join("state.sqlite3")).unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(
        db.clone(),
        Arc::new(WasmExecutor(fixture.runtime().await)),
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
        let run = db.submit(None, "default".into(), prompt.into(), 5).unwrap();
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
        Arc::new(WasmExecutor(fixture.runtime().await)),
        1,
        stop.clone(),
    ));
    let run = db
        .submit(None, "default".into(), "tool:spin".into(), 1)
        .unwrap();
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
        .await
        .run_turn("default", "read-only", "hello", sink)
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
    runtime
        .run_turn("default", session, prompt, sink)
        .await
        .unwrap();
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
    let runtime = fixture.runtime().await;
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
            .run_turn("default", "tools", "api-error", Arc::new(|_| {}))
            .await
            .is_err()
    );
    assert_eq!(turn_output(&runtime, "tools", "tool:counter").await, "3");
    assert!(
        runtime
            .run_turn("default", "provider", "api-error", Arc::new(|_| {}))
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
    // A turn with another definition doesn't reuse, or keep, this one's instances.
    runtime
        .run_turn("notools", "tools", "hello", Arc::new(|_| {}))
        .await
        .unwrap();
    assert_eq!(turn_output(&runtime, "tools", "tool:counter").await, "1");
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn fresh_per_call_tools_still_reset_while_the_provider_is_reused() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime().await;
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
    let runtime = fixture.runtime().await;
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
            .run_turn("default", "recover", "provider-trap", Arc::new(|_| {}))
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
            runtime.run_turn("default", "recover", "wait", Arc::new(|_| {}))
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

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn configured_tool_names_control_model_definitions_dispatch_and_events() {
    let fixture = Fixture::new();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    let tool_block = config
        .split_once("[tools.fixture]")
        .unwrap()
        .1
        .split_once("[session_store]")
        .unwrap()
        .0;
    let tool = tool_block.replace("fresh_per_call = true", "fresh_per_call = false");
    let second_dir = fixture.dir.path().join("second-tool");
    std::fs::create_dir(&second_dir).unwrap();
    std::fs::write(second_dir.join("allowed.txt"), "second").unwrap();
    let second = tool.replace(
        &serde_json::to_string(fixture.dir.path().join("tools").to_str().unwrap()).unwrap(),
        &serde_json::to_string(second_dir.to_str().unwrap()).unwrap(),
    );
    let config = config
        .replace(
            &format!("[tools.fixture]{tool_block}"),
            &format!("[tools.counter_a]{tool}[tools.counter_b]{second}"),
        )
        .replace(
            "tools = [\"fixture\"]",
            "tools = [\"counter_a\", \"counter_b\"]",
        );
    std::fs::write(&fixture.config, config).unwrap();
    let runtime = fixture.runtime().await;
    // Both components still describe themselves as "fixture"; config keys win.
    assert_eq!(
        turn_output(&runtime, "aliases", "tool-names").await,
        "counter_a,counter_b"
    );
    for (prompt, expected) in [
        ("named-tool:counter_a:counter", "1"),
        ("named-tool:counter_a:counter", "2"),
        ("named-tool:counter_b:counter", "1"),
        ("named-tool:counter_a:read-allowed", "allowed"),
        ("named-tool:counter_b:read-allowed", "second"),
    ] {
        assert_eq!(turn_output(&runtime, "aliases", prompt).await, expected);
    }
    assert!(
        turn_output(&runtime, "aliases", "tool:counter")
            .await
            .contains("no such tool: fixture")
    );
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = events.clone();
    runtime
        .run_turn(
            "default",
            "aliases",
            "named-tool:counter_b:counter",
            Arc::new(move |event| captured.lock().unwrap().push(event)),
        )
        .await
        .unwrap();
    let events = events.lock().unwrap();
    for kind in ["tool_started", "tool_finished"] {
        let event = events.iter().find(|event| event["kind"] == kind).unwrap();
        assert_eq!(event["name"], "counter_b");
    }
}
