// Exercise reconnect/cursor behavior through the real CLI against a controlled SSE server.
use axum::{Json, Router, extract::Query, routing::get};
use runner::db::Run;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn watcher_reconnects_from_its_cursor_and_ignores_duplicate_events() {
    let connections = Arc::new(AtomicUsize::new(0));
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let run_connections = connections.clone();
    let event_connections = connections.clone();
    let event_cursors = cursors.clone();
    let router = Router::new()
        .route(
            "/v1/agents",
            get(|| async { Json(Vec::<host::AgentInfo>::new()) }),
        )
        .route(
            "/v1/runs/fixture",
            get(move || {
                let connections = run_connections.clone();
                async move {
                    Json(Run {
                        id: "fixture".into(),
                        session_id: "session".into(),
                        agent: "default".into(),
                        prompt: "hello".into(),
                        status: if connections.load(Ordering::SeqCst) >= 2 {
                            "completed"
                        } else {
                            "running"
                        }
                        .into(),
                        created_at: 0,
                        deadline_secs: 30,
                        cancel_requested: false,
                        error: None,
                        parent_run_id: None,
                        root_run_id: "fixture".into(),
                        depth: 0,
                        deadline_at: None,
                    })
                }
            }),
        )
        .route(
            "/v1/runs/fixture/events",
            get(
                move |Query(query): Query<std::collections::HashMap<String, String>>| {
                    let connections = event_connections.clone();
                    let cursors = event_cursors.clone();
                    async move {
                        cursors
                            .lock()
                            .unwrap()
                            .push(query["after"].parse::<i64>().unwrap());
                        let attempt = connections.fetch_add(1, Ordering::SeqCst);
                        let frame = |id, text| {
                            format!(
                                "data: {}\n\n",
                                serde_json::json!({
                    "id":id,"run_id":"fixture","data":{"kind":"text","text":text}})
                            )
                        };
                        let body = if attempt == 0 {
                            frame(1, "first")
                        } else {
                            frame(1, "first") + &frame(2, "second")
                        };
                        (
                            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                            body,
                        )
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("connection.json"),
        serde_json::to_vec(
            &serde_json::json!({"port":listener.local_addr().unwrap().port(), "token":"test"}),
        )
        .unwrap(),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let path = dir.path().to_owned();
    let output = tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_runner"))
            .arg("--data-dir")
            .arg(path)
            .args(["runs", "watch", "fixture", "--json"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("watcher did not reconnect");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    server.abort();
    assert!(output.status.success(), "{output:?}");
    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .map(|event| event["id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(*cursors.lock().unwrap(), [0, 1]);
}
