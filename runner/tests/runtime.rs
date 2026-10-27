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
        for name in ["test_provider", "test_tool"] {
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
[providers.fixture]
path = {}
model = "fixture"
allowed_hosts = ["fixture.invalid"]
headers = {{ x-api-key = {{ value = "host-only-fixture-key", hosts = ["fixture.invalid"] }} }}
[tools.fixture]
path = {}
dir = {}
timeout_secs = {tool_timeout}
fresh_per_call = true
[agents.default]
provider = "fixture"
tools = ["fixture"]
[agents.notools]
provider = "fixture"
model = "fixture-small"
system_prompt = "Answer without tools."
[agents.limited]
provider = "fixture"
tools = ["fixture"]
max_tokens = 99
max_model_calls = 1
[agents.auditor]
provider = "fixture"
inspect_components = true
read_audit_trails = true
"#,
                quote(target.join("test_provider.wasm")),
                quote(target.join("test_tool.wasm")),
                quote(tools),
            ),
        )
        .unwrap();
        Self { dir, config }
    }

    async fn runtime(&self) -> TestRuntime {
        TestRuntime {
            host: host::Runtime::new(self.config.to_str().unwrap())
                .await
                .unwrap(),
            transcripts: Arc::default(),
        }
    }
}

/// The runtime, keeping each session's transcript between turns in memory
/// the way the service keeps it in SQLite. Clones share both.
#[derive(Clone)]
struct TestRuntime {
    host: host::Runtime,
    transcripts: Arc<Mutex<std::collections::HashMap<String, String>>>,
}

impl TestRuntime {
    async fn run_turn(
        &self,
        agent: &str,
        session: &str,
        prompt: &str,
        events: host::EventSink,
    ) -> anyhow::Result<()> {
        let transcript = self.transcripts.lock().unwrap().get(session).cloned();
        let transcript = self
            .host
            .run_turn(agent, session, transcript.as_deref(), prompt, events)
            .await?;
        self.transcripts
            .lock()
            .unwrap()
            .insert(session.to_string(), transcript);
        Ok(())
    }

    fn agents(&self) -> Vec<host::AgentInfo> {
        self.host.agents()
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
    // Saving is the service's job, committed with the run's completion.
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "turn_saved")
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
async fn inspect_component_reads_configured_components_without_secrets() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime().await;
    let run = async |agent: &str, input: &str| {
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = events.clone();
        let sink: host::EventSink = Arc::new(move |value| captured.lock().unwrap().push(value));
        let prompt = format!("named-tool:inspect_component:{input}");
        runtime.run_turn(agent, agent, &prompt, sink).await.unwrap();
        let events = events.lock().unwrap().clone();
        let finished = events
            .iter()
            .find(|e| e["kind"] == "tool_finished")
            .unwrap()
            .clone();
        let text: String = events
            .iter()
            .filter(|e| e["kind"] == "text")
            .map(|e| e["text"].as_str().unwrap())
            .collect();
        (finished, text)
    };
    let (finished, text) = run("auditor", r#"{"component":"fixture","kind":"tool"}"#).await;
    assert_eq!(finished["ok"], true, "{finished}");
    assert!(text.starts_with(";; tool fixture:"), "{text}");
    assert!(text.contains(";; host enforcement:"), "{text}");
    assert!(
        text.contains("instances: a new one for every call"),
        "{text}"
    );
    assert!(
        text.contains("(export \"local:agent/tool-api@0.1.0\""),
        "{text}"
    );
    assert!(text.contains("(core module"), "{text}");
    let (_, text) = run(
        "auditor",
        r#"{"component":"fixture","kind":"provider","detail":"interface"}"#,
    )
    .await;
    assert!(
        text.contains("header x-api-key (value hidden) sent to: fixture.invalid"),
        "{text}"
    );
    assert!(!text.contains("host-only-fixture-key"), "{text}");
    assert!(!text.contains("(core module"), "{text}");
    // Only configured names, and only for agents granted the tool.
    let (finished, _) = run("auditor", r#"{"component":"../../etc/passwd"}"#).await;
    assert_eq!(finished["ok"], false);
    let (finished, _) = run("default", r#"{"component":"fixture"}"#).await;
    assert_eq!(finished["error"], "no such tool: inspect_component");
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn audit_trail_reads_other_runs_but_never_its_own() {
    let fixture = Fixture::new();
    let db = Database::open(&fixture.dir.path().join("state.sqlite3")).unwrap();
    let stop = CancellationToken::new();
    let runtime = fixture.runtime().await.host;
    let version = runtime.agent_version("default").unwrap().id;
    let task = tokio::spawn(worker(
        db.clone(),
        Arc::new(WasmExecutor(runtime)),
        1,
        stop.clone(),
    ));
    let finished = |id: &str| {
        db.events(id, 0)
            .unwrap()
            .into_iter()
            .find(|event| event.data["kind"] == "tool_finished")
            .unwrap()
            .data
    };
    let reviewed = db
        .submit(None, "default".into(), "hello".into(), 30)
        .unwrap();
    wait(&db, &reviewed.id, "completed").await;

    let prompt = format!(r#"named-tool:audit_trail:{{"run_id":"{}"}}"#, reviewed.id);
    let review = db.submit(None, "auditor".into(), prompt, 30).unwrap();
    wait(&db, &review.id, "completed").await;
    assert_eq!(finished(&review.id)["ok"], true);
    let text: String = db
        .events(&review.id, 0)
        .unwrap()
        .iter()
        .filter(|event| event.data["kind"] == "text")
        .map(|event| event.data["text"].as_str().unwrap().to_string())
        .collect();
    assert!(
        text.starts_with(&format!("Audit trail of run {}:", reviewed.id)),
        "{text}"
    );
    assert!(text.contains("\nTrail problems:\nnone\n"), "{text}");
    assert!(text.contains(r#""action":"provider.chat""#), "{text}");
    // What the run was allowed, from the version its trail names.
    assert!(
        text.contains(&format!(
            r#"{{"id":"{version}","agent":"default","definition":"#
        )),
        "{text}"
    );

    // Its own session is refused, and the call is audited as a host tool's.
    let session = db.create_session(None, "auditor".into()).unwrap().id;
    let prompt = format!(r#"named-tool:audit_trail:{{"session_id":"{session}"}}"#);
    let own = db
        .submit(Some(session), "auditor".into(), prompt, 30)
        .unwrap();
    wait(&db, &own.id, "completed").await;
    let error = finished(&own.id)["error"].to_string();
    assert!(error.contains("delegation tree"), "{error}");
    let call = db
        .audit(&own.id)
        .unwrap()
        .into_iter()
        .find(|record| record.data["action"] == "tool.call")
        .unwrap()
        .data;
    assert_eq!(call["component"]["kind"], "host");
    assert_eq!(call["component"]["name"], "audit_trail");

    // Only agents granted the tool have it.
    let prompt = format!(r#"named-tool:audit_trail:{{"run_id":"{}"}}"#, reviewed.id);
    let denied = db.submit(None, "default".into(), prompt, 30).unwrap();
    wait(&db, &denied.id, "completed").await;
    assert_eq!(finished(&denied.id)["error"], "no such tool: audit_trail");
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn agent_versions_record_what_each_run_was_allowed() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime().await.host;
    let version = runtime.agent_version("default").unwrap();
    // The same config and components always give the same version.
    assert_eq!(
        fixture.runtime().await.host.agent_version("default"),
        Some(version.clone())
    );
    assert!(version.id.starts_with("sha256:"));
    let definition = &version.definition;
    assert_eq!(definition["model"], "fixture");
    assert_eq!(definition["tools"][0]["name"], "fixture");
    assert!(
        definition["tools"][0]["digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(definition["tools"][0]["timeout_secs"], 1);
    assert_eq!(
        definition["provider"]["headers"],
        serde_json::json!([{"name":"x-api-key", "hosts":["fixture.invalid"]}])
    );
    let notools = runtime.agent_version("notools").unwrap();
    for text in [definition.to_string(), notools.definition.to_string()] {
        assert!(!text.contains("host-only-fixture-key"), "{text}");
        // Its system prompt only as a hash.
        assert!(!text.contains("Answer without tools"), "{text}");
    }
    assert_ne!(notools.id, version.id);

    let db = Database::open(&fixture.dir.path().join("state.sqlite3")).unwrap();
    let run_once = async |runtime: host::Runtime| {
        let stop = CancellationToken::new();
        let task = tokio::spawn(worker(
            db.clone(),
            Arc::new(WasmExecutor(runtime)),
            1,
            stop.clone(),
        ));
        let run = db
            .submit(None, "default".into(), "hello".into(), 30)
            .unwrap();
        wait(&db, &run.id, "completed").await;
        stop.cancel();
        task.await.unwrap().unwrap();
        run.id
    };
    let first = run_once(runtime).await;
    assert_eq!(
        db.run_agent_version(&first).unwrap(),
        Some(version.id.clone())
    );
    let started = &db.audit(&first).unwrap()[0].data;
    assert_eq!(started["action"], "turn.started");
    assert_eq!(started["agent_version"], version.id.as_str());
    let stored = db.agent_version(&version.id).unwrap().unwrap();
    assert_eq!(stored.agent, "default");
    assert_eq!(&stored.definition, definition);
    // Versions aren't part of the run's event stream.
    assert!(
        !db.events(&first, 0)
            .unwrap()
            .iter()
            .any(|event| event.data["kind"] == "agent_version")
    );

    // A config change makes a new version; the earlier run keeps its own.
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    let changed = config.replacen(
        "[agents.default]\nprovider = \"fixture\"\n",
        "[agents.default]\nprovider = \"fixture\"\nmax_model_calls = 7\n",
        1,
    );
    assert_ne!(changed, config);
    std::fs::write(&fixture.config, changed).unwrap();
    let runtime = fixture.runtime().await.host;
    let newer = runtime.agent_version("default").unwrap();
    assert_ne!(newer.id, version.id);
    assert_eq!(newer.definition["max_model_calls"], 7);
    let second = run_once(runtime).await;
    assert_eq!(db.run_agent_version(&second).unwrap(), Some(newer.id));
    assert_eq!(
        db.run_agent_version(&first).unwrap(),
        Some(version.id.clone())
    );
    assert_eq!(
        &db.agent_version(&version.id).unwrap().unwrap().definition,
        definition
    );
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn agent_definitions_only_get_the_tools_they_name() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime().await;
    let access: std::collections::BTreeMap<_, _> = [(
        "fixture".to_string(),
        format!(
            "no network access; reads {}",
            fixture.dir.path().join("tools").display()
        ),
    )]
    .into();
    let version = |agent: &str| runtime.host.agent_version(agent).unwrap().id;
    assert_eq!(
        runtime.agents(),
        [
            host::AgentInfo {
                name: "auditor".into(),
                provider: "fixture".into(),
                model: "fixture".into(),
                tools: vec!["audit_trail".into(), "inspect_component".into()],
                delegates: vec![],
                max_tokens: host::DEFAULT_MAX_TOKENS,
                max_model_calls: None,
                version: version("auditor"),
                tool_access: Default::default(),
            },
            host::AgentInfo {
                name: "default".into(),
                provider: "fixture".into(),
                model: "fixture".into(),
                tools: vec!["fixture".into()],
                delegates: vec![],
                max_tokens: host::DEFAULT_MAX_TOKENS,
                max_model_calls: None,
                version: version("default"),
                tool_access: access.clone(),
            },
            host::AgentInfo {
                name: "limited".into(),
                provider: "fixture".into(),
                model: "fixture".into(),
                tools: vec!["fixture".into()],
                delegates: vec![],
                max_tokens: 99,
                max_model_calls: Some(1),
                version: version("limited"),
                tool_access: access.clone(),
            },
            host::AgentInfo {
                name: "notools".into(),
                provider: "fixture".into(),
                model: "fixture-small".into(),
                tools: vec![],
                delegates: vec![],
                max_tokens: host::DEFAULT_MAX_TOKENS,
                max_model_calls: None,
                version: version("notools"),
                tool_access: Default::default(),
            },
        ]
    );
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
        Arc::new(WasmExecutor(runtime.host)),
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
    assert!(
        stderr.contains("\nprovider fixture · model fixture · tools fixture\n"),
        "{stderr}"
    );
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
    let created = Command::new(env!("CARGO_BIN_EXE_runner"))
        .arg("--data-dir")
        .arg(&data)
        .args(["sessions", "create", "preselected", "--agent", "notools"])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let session: runner::db::Session = serde_json::from_slice(&created.stdout).unwrap();
    assert_eq!(session.agent, "notools");
    assert!(client.runs(Some("preselected")).await.unwrap().is_empty());
    let resumed_empty = chat(&data, &["--session", "preselected"], "");
    assert!(resumed_empty.starts_with("runner · agent notools · session preselected · "));
    let named = chat(&data, &["--session", "a", "--agent", "notools"], "hi\n");
    assert!(
        named.starts_with("runner · agent notools · session a · "),
        "{named}"
    );
    // The agent's provider, model (here its own override) and tools follow.
    assert_eq!(
        named.lines().nth(1),
        Some("provider fixture · model fixture-small · tools none")
    );
    let default = chat(&data, &["--session", "b"], "");
    assert!(
        default.starts_with("runner · agent default · session b · "),
        "{default}"
    );
    assert_eq!(
        default.lines().nth(1),
        Some("provider fixture · model fixture · tools fixture")
    );
    // Then what each tool can reach.
    assert_eq!(
        default.lines().nth(2),
        Some(
            format!(
                "  fixture: no network access; reads {}",
                fixture.dir.path().join("tools").display()
            )
            .as_str()
        )
    );
    let run = client.runs(Some("a")).await.unwrap();
    assert_eq!(run[0].agent, "notools");
    let resumed = chat(&data, &["--session", "a"], "");
    assert!(resumed.starts_with("runner · agent notools · session a · "));
    for command in ["chat", "run"] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.arg("--data-dir")
            .arg(&data)
            .args([command, "--session", "a", "--agent", "default"]);
        if command == "run" {
            cmd.arg("switch");
        }
        let output = cmd.stdin(Stdio::null()).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("create a new session"));
    }
    assert_eq!(client.runs(Some("a")).await.unwrap().len(), 1);

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
        stderr.contains("unknown agent 'nope'; the daemon has: auditor, default, limited, notools"),
        "{stderr}"
    );
    // Rejected before the chat started, so no session was made for it.
    let sessions = client.sessions().await.unwrap();
    assert!(!sessions.iter().any(|session| session.id == "c"));
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn capabilities_come_from_imports_and_config_without_secrets() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime().await;
    let capabilities = runtime.host.capabilities();
    let names: Vec<_> = capabilities
        .iter()
        .map(|caps| (caps.kind, caps.name.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            (host::inspect::ComponentKind::Provider, "fixture"),
            (host::inspect::ComponentKind::Tool, "fixture"),
        ]
    );
    // The fixture provider answers without HTTP, so its allowed host and
    // header give it nothing; imports, not just config, decide that.
    let provider = &capabilities[0];
    assert!(
        provider
            .imports
            .contains(&"local:agent/messages@0.1.0".to_string()),
        "{:?}",
        provider.imports
    );
    assert!(!provider.has_network());
    assert_eq!(
        provider.summary(),
        "no network access (hosts are allowed, but it doesn't import wasi:http); \
         no filesystem access"
    );
    let tool = &capabilities[1];
    assert!(!tool.imports.is_empty());
    assert_eq!(tool.timeout_secs, Some(1));
    assert!(tool.fresh_per_call);

    let data = fixture.dir.path().join("service");
    let _daemon = start(&fixture, &data);
    connect(&data).await;
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_runner"))
            .arg("--data-dir")
            .arg(&data)
            .args(args)
            .output()
            .unwrap()
    };
    let output = cli(&["providers", "inspect", "fixture"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("provider fixture\n"), "{text}");
    assert!(
        text.contains("x-api-key → fixture.invalid (value kept by the host)"),
        "{text}"
    );
    let json = cli(&["providers", "inspect", "fixture", "--json"]);
    assert!(json.status.success());
    let json = String::from_utf8(json.stdout).unwrap();
    for output in [&text, &json] {
        assert!(!output.contains("host-only-fixture-key"), "{output}");
    }
    let list = String::from_utf8(cli(&["tools", "list"]).stdout).unwrap();
    assert!(
        list.starts_with("fixture  no network access; reads "),
        "{list}"
    );
    let tool = String::from_utf8(cli(&["tools", "inspect", "fixture"]).stdout).unwrap();
    assert!(
        tool.contains(&format!(
            "limits     timeout 1s · memory {} MiB (default) · fresh instance per call",
            host::DEFAULT_MAX_MEMORY_MB
        )),
        "{tool}"
    );
    let unknown = cli(&["tools", "inspect", "nope"]);
    assert!(!unknown.status.success());
    assert!(
        String::from_utf8(unknown.stderr)
            .unwrap()
            .contains("unknown tool 'nope'; the daemon has: fixture")
    );
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
async fn agent_limits_cap_reply_tokens_and_model_calls() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime().await;
    let turn = async |agent: &str, session: &str, prompt: &str| {
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = events.clone();
        let sink: host::EventSink = Arc::new(move |value| captured.lock().unwrap().push(value));
        let result = runtime.run_turn(agent, session, prompt, sink).await;
        (result, events.lock().unwrap().clone())
    };
    let text =
        |events: &[Value]| -> String { events.iter().filter_map(|e| e["text"].as_str()).collect() };
    // The provider is asked for the agent's own token limit, and usage reports it.
    let (result, events) = turn("limited", "tokens", "max-tokens").await;
    result.unwrap();
    assert_eq!(text(&events), "99");
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "usage" && e["max_tokens"] == 99)
    );
    let (result, events) = turn("default", "default-tokens", "max-tokens").await;
    result.unwrap();
    assert_eq!(text(&events), host::DEFAULT_MAX_TOKENS.to_string());

    // A tool call needs a second model call, which `limited` doesn't allow.
    let (result, events) = turn("limited", "calls", "tool:probe").await;
    let error = result.unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<host::TurnFailed>()
            .map(|e| e.0.as_str()),
        Some("stopped after 1 model call without a final reply, so this turn wasn't saved")
    );
    // The requested tool still ran; the turn just wasn't saved.
    assert!(events.iter().any(|e| e["kind"] == "tool_finished"));
    assert!(!events.iter().any(|e| e["kind"] == "turn_saved"));
    let (result, _) = turn("default", "calls-default", "tool:probe").await;
    result.unwrap();
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn failed_turns_explain_why_without_leaking_credentials() {
    let fixture = Fixture::new();
    let db = Database::open(&fixture.dir.path().join("state.sqlite3")).unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(
        db.clone(),
        Arc::new(WasmExecutor(fixture.runtime().await.host)),
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
        Arc::new(WasmExecutor(fixture.runtime().await.host)),
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
async fn an_unreadable_transcript_fails_the_turn_without_details() {
    let fixture = Fixture::new();
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = events.clone();
    let sink: host::EventSink = Arc::new(move |event| captured.lock().unwrap().push(event));
    let error = fixture
        .runtime()
        .await
        .host
        .run_turn(
            "default",
            "corrupt",
            Some("not a transcript"),
            "hello",
            sink,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<host::TurnFailed>()
            .map(|e| e.0.as_str()),
        Some("could not load this session's history"),
        "{error:#}"
    );
    // Given up before the model was called.
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn conversations_are_kept_in_the_service_database() {
    let fixture = Fixture::new();
    let db = Database::open(&fixture.dir.path().join("state.sqlite3")).unwrap();
    let session = db
        .create_session(Some("kept".into()), "default".into())
        .unwrap()
        .id;
    let text = |run: &str| -> String {
        db.events(run, 0)
            .unwrap()
            .iter()
            .filter_map(|e| e.data["text"].as_str().map(str::to_string))
            .collect()
    };
    for (prompt, reply) in [("hello", "turn 1: hello"), ("again", "turn 2: again")] {
        // A new runtime each turn, as after a daemon restart: the history
        // comes from the database, not from memory.
        let stop = CancellationToken::new();
        let task = tokio::spawn(worker(
            db.clone(),
            Arc::new(WasmExecutor(fixture.runtime().await.host)),
            1,
            stop.clone(),
        ));
        let run = db
            .submit(Some(session.clone()), "default".into(), prompt.into(), 5)
            .unwrap();
        wait(&db, &run.id, "completed").await;
        assert_eq!(text(&run.id), reply);
        stop.cancel();
        task.await.unwrap().unwrap();
    }
    let transcript: Value =
        serde_json::from_str(&db.transcript(&session).unwrap().unwrap()).unwrap();
    assert_eq!(transcript.as_array().unwrap().len(), 4);
    assert_eq!(
        transcript[2],
        json!({"role":"User","content":[{"Text":"again"}]})
    );
    assert_eq!(db.transcript("never-used").unwrap(), None);
}

async fn turn_output(runtime: &TestRuntime, session: &str, prompt: &str) -> String {
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
        .split_once("[agents.default]")
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

mod support;

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn activity_logs_attribute_cached_components_and_concurrent_turns_without_payloads() {
    use tracing::Instrument;
    let fixture = Fixture::with_tool_timeout(10);
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("fresh_per_call = true", "fresh_per_call = false");
    std::fs::write(&fixture.config, config).unwrap();
    let runtime = fixture.runtime().await;
    let capture = support::Capture::default();
    // Provider tasks run independently; a process-wide test subscriber captures
    // their records as well as parallel turns on other runtime threads.
    tracing::subscriber::set_global_default(capture.subscriber()).unwrap();
    for run in ["first", "second"] {
        turn_output(&runtime, "cached", "tool:counter")
            .instrument(tracing::info_span!("run", run))
            .await;
    }
    tokio::join!(
        turn_output(&runtime, "parallel-a", "tool:counter"),
        turn_output(&runtime, "parallel-b", "tool:counter"),
    );
    turn_output(&runtime, "private", "tool:ARGUMENT_SECRET").await;
    turn_output(&runtime, "private", "PROMPT_SECRET").await;
    turn_output(&runtime, "private", "named-tool:missing:ARGUMENT_SECRET").await;
    let _ = tokio::time::timeout(
        Duration::from_millis(200),
        runtime.run_turn("default", "cancelled", "tool:spin", Arc::new(|_| {})),
    )
    .await;
    let logs = capture.text();
    for expected in [
        "run=\"first\"",
        "run=\"second\"",
        "session=\"parallel-a\"",
        "session=\"parallel-b\"",
        "tool=\"fixture\"",
        "outcome=\"ok\"",
        "outcome=\"error\"",
        "outcome=\"interrupted\"",
        "test_provider.wasm",
        "test_tool.wasm",
    ] {
        assert!(logs.contains(expected), "{expected}: {logs}");
    }
    // Both calls used the same cached component, with the active run as parent.
    for run in ["first", "second"] {
        assert!(
            logs.lines()
                .any(|line| line.contains(&format!("run=\"{run}\""))
                    && line.contains("tool=\"fixture\"")
                    && line.contains("activity finished")),
            "{logs}"
        );
    }
    for secret in ["ARGUMENT_SECRET", "PROMPT_SECRET", "host-only-fixture-key"] {
        assert!(!logs.contains(secret), "{logs}");
    }
}

/// A chat whose input remains open; output reads are bounded so regressions fail
/// instead of leaving a subprocess stuck waiting for input.
struct LiveChat {
    child: Option<Child>,
    input: Option<std::process::ChildStdin>,
    output: std::sync::mpsc::Receiver<u8>,
    text: Vec<u8>,
}

impl LiveChat {
    fn new(data: &std::path::Path, args: &[&str]) -> Self {
        use std::io::Read;
        let mut child = Command::new(env!("CARGO_BIN_EXE_runner"))
            .arg("--data-dir")
            .arg(data)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let mut stdout = child.stdout.take().unwrap();
        let (send, output) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut byte = [0];
            while stdout.read_exact(&mut byte).is_ok() {
                if send.send(byte[0]).is_err() {
                    break;
                }
            }
        });
        Self {
            child: Some(child),
            input,
            output,
            text: Vec::new(),
        }
    }

    fn until(&mut self, needle: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&self.text).contains(needle) {
            let byte = self
                .output
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "waiting for {needle:?}: {error}; output: {}",
                        String::from_utf8_lossy(&self.text)
                    )
                });
            self.text.push(byte);
        }
    }

    fn finish(self) -> (String, String) {
        self.finish_status(true)
    }

    fn finish_status(mut self, success: bool) -> (String, String) {
        self.input.take();
        let child = self.child.as_mut().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "chat did not exit: {}",
                String::from_utf8_lossy(&self.text)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = self.child.take().unwrap().wait_with_output().unwrap();
        assert_eq!(output.status.success(), success, "{:?}", output);
        for byte in self.output.iter() {
            self.text.push(byte);
        }
        (
            String::from_utf8(std::mem::take(&mut self.text)).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }
}

impl Drop for LiveChat {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn running(client: &Client, id: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while client.run(id).await.unwrap().status != "running" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn chat_catches_up_pending_and_new_turns_before_accepting_input() {
    let fixture = Fixture::new();
    let data = fixture.dir.path().join("service");
    let _daemon = start(&fixture, &data);
    let client = connect(&data).await;
    client
        .create_session(Some("catch-up".into()), "default".into())
        .await
        .unwrap();
    let history = client
        .submit(Some("catch-up".into()), None, "history".into(), 30)
        .await
        .unwrap();
    client.watch(&history.id, 0, false).await.unwrap();
    let active = client
        .submit(Some("catch-up".into()), None, "wait".into(), 30)
        .await
        .unwrap();
    running(&client, &active.id).await;
    let failed = client
        .submit(Some("catch-up".into()), None, "fail".into(), 30)
        .await
        .unwrap();
    let queued = client
        .submit(Some("catch-up".into()), None, "wait".into(), 30)
        .await
        .unwrap();
    let mut chat = LiveChat::new(&data, &["chat", "--session", "catch-up"]);
    chat.until(&format!("run {} ", active.id));
    // Input written now must not submit a new turn until catch-up finishes.
    use std::io::Write;
    writeln!(chat.input.as_mut().unwrap(), "new-input").unwrap();
    assert_eq!(client.runs(Some("catch-up")).await.unwrap().len(), 4);
    client.cancel(&active.id).await.unwrap();
    chat.until(&format!("run {} ", queued.id));
    let added = client
        .submit(Some("catch-up".into()), None, "after-catch-up".into(), 30)
        .await
        .unwrap();
    client.cancel(&queued.id).await.unwrap();
    chat.until("turn 3: new-input");
    let (text, errors) = chat.finish();
    let history_pos = text.find("> history").unwrap();
    let active_pos = text.find(&format!("run {} ", active.id)).unwrap();
    let failed_pos = text.find(&format!("run {} ", failed.id)).unwrap();
    let queued_pos = text.find(&format!("run {} ", queued.id)).unwrap();
    let added_pos = text.find(&format!("run {} ", added.id)).unwrap();
    let input_pos = text.find("turn 3: new-input").unwrap();
    assert!(
        history_pos < active_pos
            && active_pos < failed_pos
            && failed_pos < queued_pos
            && queued_pos < added_pos
            && added_pos < input_pos,
        "{text}"
    );
    assert_eq!(text.matches("> history").count(), 1, "{text}");
    assert_eq!(text.matches("turn 2: after-catch-up").count(), 1, "{text}");
    assert!(errors.contains("run cancelled"), "{errors}");
    assert!(errors.contains("fixture failure"), "{errors}");
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn ctrl_c_detaches_chat_and_watch_successfully_without_cancelling_work() {
    let fixture = Fixture::new();
    let data = fixture.dir.path().join("service");
    let _daemon = start(&fixture, &data);
    let client = connect(&data).await;
    client
        .create_session(Some("detach".into()), "default".into())
        .await
        .unwrap();
    let active = client
        .submit(Some("detach".into()), None, "wait".into(), 30)
        .await
        .unwrap();
    running(&client, &active.id).await;
    let queued = client
        .submit(Some("detach".into()), None, "hello".into(), 30)
        .await
        .unwrap();
    for (args, marker, id) in [
        (
            vec!["chat", "--session", "detach"],
            format!("run {} ", active.id),
            active.id.as_str(),
        ),
        (
            vec!["runs", "watch", &queued.id],
            "Queued".into(),
            queued.id.as_str(),
        ),
        (
            vec!["run", "--session", "detach", "another"],
            "Queued".into(),
            "",
        ),
    ] {
        let mut chat = LiveChat::new(&data, &args);
        chat.until(&marker);
        let status = Command::new("kill")
            .args(["-INT", &chat.child.as_ref().unwrap().id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let (text, errors) = chat.finish();
        assert!(errors.contains("detached; run "), "{errors}");
        assert!(errors.contains(id), "{errors}");
        assert!(errors.contains("runner chat --session detach"), "{errors}");
        assert!(!errors.contains("Error:"), "{errors}");
        if args[0] == "chat" {
            assert!(
                text.ends_with("resume with: runner chat --session detach\n"),
                "{text}"
            );
        }
    }
    assert_eq!(client.run(&active.id).await.unwrap().status, "running");
    assert_eq!(client.run(&queued.id).await.unwrap().status, "queued");
    for run in client.runs(Some("detach")).await.unwrap() {
        assert!(!run.cancel_requested);
        client.cancel(&run.id).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn connection_failure_during_catch_up_does_not_offer_an_idle_prompt() {
    let fixture = Fixture::new();
    let data = fixture.dir.path().join("service");
    let daemon = start(&fixture, &data);
    let client = connect(&data).await;
    client
        .create_session(Some("lost".into()), "default".into())
        .await
        .unwrap();
    let active = client
        .submit(Some("lost".into()), None, "wait".into(), 30)
        .await
        .unwrap();
    running(&client, &active.id).await;
    let mut chat = LiveChat::new(&data, &["chat", "--session", "lost"]);
    chat.until(&format!("run {} ", active.id));
    drop(daemon);
    let (text, errors) = chat.finish_status(false);
    // Only the pending turn's historical prompt was printed.
    assert_eq!(text.matches("\n> ").count(), 1, "{text}");
    assert!(errors.contains("Error:"), "{errors}");
    assert!(
        text.ends_with("resume with: runner chat --session lost\n"),
        "{text}"
    );
}

fn delegation_fixture() -> Fixture {
    let fixture = Fixture::new();
    let mut config = std::fs::read_to_string(&fixture.config).unwrap();
    config.push_str(
        r#"
[agents.coordinator]
provider = "fixture"
delegates = ["leaf", "lead"]
[agents.lead]
provider = "fixture"
delegates = ["leaf"]
[agents.leaf]
provider = "fixture"
system_prompt = "Leaf specialist instructions."
"#,
    );
    std::fs::write(&fixture.config, config).unwrap();
    fixture
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn configured_specialists_delegate_in_parallel_and_nested_at_concurrency_one() {
    let fixture = delegation_fixture();
    let runtime = fixture.runtime().await;
    let db = Database::open(&fixture.dir.path().join("delegation.sqlite3")).unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(
        db.clone(),
        Arc::new(WasmExecutor(runtime.host.clone())),
        1,
        stop.clone(),
    ));
    for (prompt, count) in [
        ("delegate:leaf:hello", 2),
        ("delegate:lead:delegate:leaf:hello", 3),
        ("parallel:leaf", 3),
        ("delegate:leaf:system-prompt", 2),
    ] {
        let root = db
            .submit(None, "coordinator".into(), prompt.into(), 10)
            .unwrap();
        let root = wait(&db, &root.id, "completed").await;
        let tree: Vec<_> = db
            .runs(None)
            .unwrap()
            .into_iter()
            .filter(|run| run.root_run_id == root.id)
            .collect();
        assert_eq!(tree.len(), count, "{tree:?}");
        assert!(tree.iter().all(|run| run.status == "completed"), "{tree:?}");
        for child in tree.iter().filter(|run| run.agent == "leaf") {
            let result = db
                .child_result(child.parent_run_id.as_deref().unwrap(), &child.id)
                .unwrap()
                .unwrap();
            let expected = if prompt.ends_with("system-prompt") {
                "Leaf specialist instructions."
            } else if prompt == "parallel:leaf" {
                if child.prompt == "first task" {
                    "turn 1: first task"
                } else {
                    "turn 1: second task"
                }
            } else {
                "turn 1: hello"
            };
            assert_eq!(result["output"], expected);
            assert_ne!(child.session_id, root.session_id);
            assert!(child.deadline_at <= root.deadline_at);
        }
        let starts: Vec<_> = db
            .events(&root.id, 0)
            .unwrap()
            .into_iter()
            .filter(|event| event.data["kind"] == "tool_started")
            .map(|event| event.data["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            starts,
            if prompt == "parallel:leaf" {
                vec!["delegate", "delegate", "wait", "wait"]
            } else {
                vec!["delegate", "wait"]
            }
        );
    }
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn guest_requested_delegation_cannot_bypass_host_permissions_or_service_context() {
    let fixture = delegation_fixture();
    let runtime = fixture.runtime().await;
    let db = Database::open(&fixture.dir.path().join("delegation.sqlite3")).unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(
        db.clone(),
        Arc::new(WasmExecutor(runtime.host.clone())),
        1,
        stop.clone(),
    ));
    for (agent, prompt) in [
        ("default", "delegate:leaf:hello"),
        ("coordinator", "delegate:notools:hello"),
        ("coordinator", r#"named-tool:wait:{"run_id":"unrelated"}"#),
    ] {
        let root = db.submit(None, agent.into(), prompt.into(), 10).unwrap();
        wait(&db, &root.id, "completed").await;
        assert_eq!(
            db.runs(None)
                .unwrap()
                .iter()
                .filter(|run| run.root_run_id == root.id)
                .count(),
            1
        );
        let text: String = db
            .events(&root.id, 0)
            .unwrap()
            .iter()
            .filter_map(|event| event.data["text"].as_str())
            .collect();
        assert!(text.contains("error:"), "{text}");
    }
    let failed_parent = db
        .submit(None, "coordinator".into(), "delegate:leaf:fail".into(), 10)
        .unwrap();
    wait(&db, &failed_parent.id, "completed").await;
    let failed_child = db
        .runs(None)
        .unwrap()
        .into_iter()
        .find(|run| run.parent_run_id.as_deref() == Some(&failed_parent.id))
        .unwrap();
    let result = db
        .child_result(&failed_parent.id, &failed_child.id)
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "failed");
    assert!(result["output"].is_null());
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("fixture failure")
    );
    let captured = Arc::new(Mutex::new(String::new()));
    let sink = captured.clone();
    runtime
        .run_turn(
            "coordinator",
            "standalone",
            "delegate:leaf:hello",
            Arc::new(move |event| {
                if let Some(text) = event["text"].as_str() {
                    sink.lock().unwrap().push_str(text);
                }
            }),
        )
        .await
        .unwrap();
    assert!(
        captured
            .lock()
            .unwrap()
            .contains("requires the runner service")
    );
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn real_delegation_supervises_unawaited_and_cancelled_child_runs() {
    let fixture = delegation_fixture();
    let runtime = fixture.runtime().await;
    let db = Database::open(&fixture.dir.path().join("delegation.sqlite3")).unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(
        db.clone(),
        Arc::new(WasmExecutor(runtime.host.clone())),
        1,
        stop.clone(),
    ));
    let unawaited = db
        .submit(None, "coordinator".into(), "spawn:leaf:wait".into(), 10)
        .unwrap();
    wait(&db, &unawaited.id, "completed").await;
    let child = db
        .runs(None)
        .unwrap()
        .into_iter()
        .find(|run| run.parent_run_id.as_deref() == Some(&unawaited.id))
        .unwrap();
    assert_eq!(child.status, "cancelled");
    let parent = db
        .submit(None, "coordinator".into(), "delegate:leaf:wait".into(), 10)
        .unwrap();
    let child = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(child) = db.runs(None).unwrap().into_iter().find(|run| {
                run.parent_run_id.as_deref() == Some(&parent.id) && run.status == "running"
            }) {
                break child;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    db.cancel(&parent.id).unwrap();
    wait(&db, &parent.id, "cancelled").await;
    wait(&db, &child.id, "cancelled").await;
    let next = db
        .submit(None, "coordinator".into(), "delegate:leaf:hello".into(), 10)
        .unwrap();
    wait(&db, &next.id, "completed").await;
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn runs_keep_a_metadata_only_audit_trail_shown_by_the_cli() {
    let fixture = Fixture::new();
    let data = fixture.dir.path().join("service");
    let _daemon = start(&fixture, &data);
    let client = connect(&data).await;
    let mut runs = Vec::new();
    for prompt in [
        "tool:probe",
        "tool:trap",
        "tool:spin",
        "named-tool:missing:{}",
    ] {
        let run = client.submit(None, None, prompt.into(), 30).await.unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while !client.run(&run.id).await.unwrap().terminal() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        runs.push(run.id);
    }
    let audit_of = |of: &str, id: &str, json: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_runner"));
        command.arg("--data-dir").arg(&data).args([of, "audit", id]);
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        (
            output.status.success(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    };
    let audit = |id: &str, json: bool| {
        let (ok, records, warnings) = audit_of("runs", id, json);
        assert!(ok);
        (records, warnings)
    };
    let tool_call = |id: &str| -> Value {
        let (records, _) = audit(id, true);
        records
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|record| record["action"] == "tool.call")
            .unwrap()
    };

    let (records, warnings) = audit(&runs[0], true);
    assert_eq!(warnings, "", "a finished run's trail is complete");
    let records: Vec<Value> = records
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let actions: Vec<_> = records
        .iter()
        .map(|r| r["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions.first(), Some(&"turn.started"));
    assert_eq!(actions.last(), Some(&"turn.finished"));
    assert!(actions.contains(&"provider.chat"));
    let sandbox = records
        .iter()
        .find(|r| r["action"] == "sandbox.create" && r["component"]["kind"] == "tool")
        .unwrap();
    assert_eq!(sandbox["dir_writable"], false);
    assert!(
        sandbox["component"]["digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    let call = records.iter().find(|r| r["action"] == "tool.call").unwrap();
    assert_eq!(call["component"]["name"], "fixture");
    assert_eq!(
        (call["decision"].as_str(), call["outcome"].as_str()),
        (Some("allowed"), Some("ok"))
    );
    assert!(
        records
            .iter()
            .all(|r| r["agent"] == "default" && r["v"] == 1)
    );

    assert_eq!(tool_call(&runs[1])["outcome"], "trap");
    assert_eq!(tool_call(&runs[2])["outcome"], "timeout");
    let missing = tool_call(&runs[3]);
    assert_eq!(missing["decision"], "denied");
    assert_eq!(missing["outcome"], "unknown_tool");
    assert_eq!(missing["component"]["kind"], "unknown");

    // The table names actions and components, and never payloads or secrets.
    let (table, _) = audit(&runs[0], false);
    assert!(
        table
            .lines()
            .any(|l| l.contains("tool.call") && l.contains("tool fixture"))
    );
    for id in &runs {
        let (records, _) = audit(id, true);
        assert!(!records.contains("host-only-fixture-key"));
        assert!(!records.contains("tool:probe"));
        assert!(!records.contains("inside_read"), "tool output stays out");
        // Nor do audit records leak into the run's own event stream.
        assert!(
            client
                .events(id)
                .await
                .unwrap()
                .iter()
                .all(|event| event.data["kind"] != "audit")
        );
    }

    // A session's trail is each of its runs' trails, a run at a time.
    let session = client
        .create_session(Some("audited".into()), "default".into())
        .await
        .unwrap()
        .id;
    let mut session_runs = Vec::new();
    for prompt in ["tool:probe", "tool:trap"] {
        let run = client
            .submit(Some(session.clone()), None, prompt.into(), 30)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while !client.run(&run.id).await.unwrap().terminal() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        session_runs.push(run.id);
    }
    let (ok, records, warnings) = audit_of("sessions", &session, true);
    assert!(ok);
    assert_eq!(warnings, "", "both runs' trails are complete");
    let expected: String = session_runs.iter().map(|id| audit(id, true).0).collect();
    assert_eq!(records, expected);
    let (_, table, _) = audit_of("sessions", &session, false);
    for id in &session_runs {
        assert!(table.lines().any(|l| l == format!("run {id}")));
    }
    let (ok, _, _) = audit_of("sessions", "no-such-session", true);
    assert!(!ok);
}

#[tokio::test]
#[ignore = "requires WASM fixture build"]
async fn components_without_a_memory_cap_get_the_default_one() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime().await;
    let finished = |session: &'static str, prompt: String| {
        let runtime = runtime.clone();
        async move {
            let events = Arc::new(Mutex::new(Vec::<Value>::new()));
            let captured = events.clone();
            let sink: host::EventSink = Arc::new(move |value| captured.lock().unwrap().push(value));
            runtime
                .run_turn("default", session, &prompt, sink)
                .await
                .unwrap();
            let events = events.lock().unwrap();
            events
                .iter()
                .find(|e| e["kind"] == "tool_finished")
                .unwrap()
                .clone()
        }
    };
    // The fixture tool sets no `max_memory_mb`.
    assert_eq!(finished("small", "tool:alloc:64".into()).await["ok"], true);
    let over = host::DEFAULT_MAX_MEMORY_MB + 64;
    assert_eq!(
        finished("large", format!("tool:alloc:{over}")).await["ok"],
        false
    );
}
