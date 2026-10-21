use anyhow::{Result, bail};
use futures_util::future::BoxFuture;
use host::delegation::Delegation;
use runner::{
    db::{Database, Run},
    delegation::ExecutionContext,
    service::{Executor, worker},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn database() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("db")).unwrap();
    (dir, db)
}

fn root(db: &Database) -> Run {
    db.submit(None, "worker".into(), "task".into(), 30).unwrap();
    db.claim().unwrap().unwrap()
}

#[test]
fn child_relationships_ownership_final_results_and_cancellation_are_durable() {
    let (_dir, db) = database();
    let parent = root(&db);
    let first = db
        .delegate(&parent.id, "worker", "child", Some(10))
        .unwrap();
    assert_ne!(first.session_id, parent.session_id);
    assert_eq!(first.parent_run_id.as_deref(), Some(parent.id.as_str()));
    assert_eq!(first.root_run_id, parent.id);
    assert_eq!(first.depth, 1);
    assert!(first.deadline_at <= parent.deadline_at);
    assert!(db.child_result("stranger", &first.id).is_err());
    assert!(db.child_result(&parent.id, &first.id).unwrap().is_none());
    db.claim().unwrap().unwrap();
    db.event(
        &first.id,
        json!({"kind":"text","text":"intermediate commentary"}),
    )
    .unwrap();
    db.event(
        &first.id,
        json!({"kind":"turn_saved","output":"final answer"}),
    )
    .unwrap();
    db.finish(&first.id, "completed", None).unwrap();
    let result = db.child_result(&parent.id, &first.id).unwrap().unwrap();
    assert_eq!(result["output"], "final answer");
    assert_eq!(
        db.child_result(&parent.id, &first.id).unwrap().unwrap(),
        result
    );
    assert!(
        !db.events(&first.id, 0)
            .unwrap()
            .iter()
            .any(|event| event.data.get("output").is_some())
    );
    let pending = db.delegate(&parent.id, "worker", "child", None).unwrap();
    db.finish(&parent.id, "completed", None).unwrap();
    assert_eq!(db.run(&pending.id).unwrap().unwrap().status, "cancelled");
    assert!(db.delegate(&parent.id, "worker", "too late", None).is_err());
    assert_eq!(db.run(&first.id).unwrap().unwrap().status, "completed");
}

#[test]
fn limits_count_completed_children_and_results_remain_valid_bounded_json() {
    let (_dir, db) = database();
    let parent = root(&db);
    for _ in 0..16 {
        let child = db.delegate(&parent.id, "worker", "child", None).unwrap();
        db.claim().unwrap().unwrap();
        db.event(
            &child.id,
            json!({"kind":"turn_saved","output":"\"\n😀".repeat(30000)}),
        )
        .unwrap();
        db.finish(&child.id, "completed", None).unwrap();
        let result = db.child_result(&parent.id, &child.id).unwrap().unwrap();
        assert!(result.to_string().len() <= 64 * 1024);
        assert_eq!(result["truncated"], true);
        assert!(result["output"].is_string());
    }
    assert!(db.delegate(&parent.id, "worker", "extra", None).is_err());
}

#[test]
fn depth_and_total_tree_limits_are_enforced() {
    let (_dir, db) = database();
    let parent = root(&db);
    let mut chain = parent.clone();
    for _ in 0..3 {
        chain = db.delegate(&chain.id, "worker", "nested", None).unwrap();
        assert_eq!(db.claim().unwrap().unwrap().id, chain.id);
    }
    assert!(db.delegate(&chain.id, "worker", "too deep", None).is_err());
    // A separate root builds a broad tree; completed descendants still count.
    let other = root(&db);
    let mut count = 0;
    for _ in 0..16 {
        let child = db.delegate(&other.id, "worker", "branch", None).unwrap();
        db.claim().unwrap().unwrap();
        count += 1;
        for _ in 0..16 {
            if count == 64 {
                assert!(
                    db.delegate(&child.id, "worker", "over budget", None)
                        .is_err()
                );
                return;
            }
            let grandchild = db.delegate(&child.id, "worker", "leaf", None).unwrap();
            db.claim().unwrap().unwrap();
            db.finish(&grandchild.id, "completed", None).unwrap();
            count += 1;
        }
        db.finish(&child.id, "completed", None).unwrap();
    }
    panic!("tree did not reach its limit");
}

#[test]
fn recovery_interrupts_active_tree_cancels_orphans_and_preserves_queued_roots() {
    let (_dir, db) = database();
    let parent = root(&db);
    let active = db.delegate(&parent.id, "worker", "active", None).unwrap();
    db.claim().unwrap().unwrap();
    let queued = db.delegate(&active.id, "worker", "queued", None).unwrap();
    let ordinary = db
        .submit(None, "worker".into(), "queued root".into(), 30)
        .unwrap();
    db.recover().unwrap();
    assert_eq!(db.run(&parent.id).unwrap().unwrap().status, "interrupted");
    assert_eq!(db.run(&active.id).unwrap().unwrap().status, "interrupted");
    assert_eq!(db.run(&queued.id).unwrap().unwrap().status, "cancelled");
    assert_eq!(db.run(&ordinary.id).unwrap().unwrap().status, "queued");
}

#[test]
fn version_one_database_migrates_existing_runs_to_roots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY,created_at INTEGER NOT NULL);
        CREATE TABLE runs (id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),
        agent TEXT NOT NULL,prompt TEXT NOT NULL,status TEXT NOT NULL,created_at INTEGER NOT NULL,
        deadline_secs INTEGER NOT NULL,cancel_requested INTEGER NOT NULL DEFAULT 0,error TEXT);
        INSERT INTO sessions VALUES ('session',0);
        INSERT INTO runs VALUES ('old','session','worker','prompt','queued',0,30,0,NULL);
        PRAGMA user_version=1;",
    )
    .unwrap();
    drop(conn);
    let db = Database::open(&path).unwrap();
    let old = db.run("old").unwrap().unwrap();
    assert_eq!(old.root_run_id, "old");
    assert_eq!(old.depth, 0);
    assert!(old.parent_run_id.is_none());
    drop(db);
    assert!(Database::open(&path).is_ok());
}

#[test]
fn version_two_database_keeps_existing_transcripts_during_delegation_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Database::open(&path).unwrap();
    db.save_transcript("session", "[]").unwrap();
    drop(db);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE run_results;
        DROP INDEX runs_by_parent; DROP INDEX runs_by_root;
        ALTER TABLE runs DROP COLUMN parent_run_id;
        ALTER TABLE runs DROP COLUMN root_run_id;
        ALTER TABLE runs DROP COLUMN depth;
        ALTER TABLE runs DROP COLUMN deadline_at;
        PRAGMA user_version=2;",
    )
    .unwrap();
    drop(conn);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.transcript("session").unwrap().as_deref(), Some("[]"));
    let root = db.submit(None, "worker".into(), "task".into(), 30).unwrap();
    db.claim().unwrap();
    assert!(db.delegate(&root.id, "worker", "child", None).is_ok());
}

#[derive(Default)]
struct Probe {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Executor for Probe {
    fn execute(
        &self,
        _: Run,
        _: host::EventSink,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { bail!("context required") })
    }
    fn execute_with_context(
        &self,
        run: Run,
        events: host::EventSink,
        _: CancellationToken,
        context: Arc<ExecutionContext>,
    ) -> BoxFuture<'static, Result<()>> {
        let active = self.active.clone();
        let peak = self.peak.clone();
        Box::pin(async move {
            let make = |prompt: String| {
                let context = context.clone();
                async move {
                    let result = context
                        .delegate("worker".into(), prompt, None)
                        .await
                        .map_err(anyhow::Error::msg)?;
                    Ok::<_, anyhow::Error>(
                        serde_json::from_str::<Value>(&result)?["run_id"]
                            .as_str()
                            .unwrap()
                            .to_owned(),
                    )
                }
            };
            let output = if run.prompt == "parallel" {
                let first = make("leaf".into()).await?;
                let second = make("leaf".into()).await?;
                let result = context
                    .wait(first.clone())
                    .await
                    .map_err(anyhow::Error::msg)?;
                assert_eq!(
                    context.wait(first).await.map_err(anyhow::Error::msg)?,
                    result
                );
                context.wait(second).await.map_err(anyhow::Error::msg)?
            } else if let Some(depth) = run.prompt.strip_prefix("chain:") {
                let depth: u32 = depth.parse()?;
                if depth == 0 {
                    "leaf answer".into()
                } else {
                    let child = make(format!("chain:{}", depth - 1)).await?;
                    context.wait(child).await.map_err(anyhow::Error::msg)?
                }
            } else if run.prompt == "await" || run.prompt == "spawn" {
                let child = make("hold".into()).await?;
                if run.prompt == "await" {
                    context.wait(child).await.map_err(anyhow::Error::msg)?
                } else {
                    "spawned".into()
                }
            } else if run.prompt == "hold" {
                futures_util::future::pending().await
            } else {
                struct Guard(Arc<AtomicUsize>);
                impl Drop for Guard {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                let _guard = Guard(active);
                peak.fetch_max(count, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                "leaf answer".into()
            };
            events(json!({"kind":"turn_saved","output":output}));
            Ok(())
        })
    }
}

async fn terminal(db: &Database, id: &str) -> Run {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let run = db.run(id).unwrap().unwrap();
            if run.terminal() {
                break run;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("run stuck: {:?}", db.runs(None)))
}

#[tokio::test]
async fn nested_and_parallel_children_complete_at_concurrency_one() {
    let (_dir, db) = database();
    let probe = Arc::new(Probe::default());
    let stop = CancellationToken::new();
    let worker = tokio::spawn(worker(db.clone(), probe.clone(), 1, stop.clone()));
    for prompt in ["parallel", "chain:3"] {
        let root = db.submit(None, "worker".into(), prompt.into(), 10).unwrap();
        assert_eq!(terminal(&db, &root.id).await.status, "completed");
        let runs = db.runs(None).unwrap();
        let tree: Vec<_> = runs
            .iter()
            .filter(|run| run.root_run_id == root.id)
            .collect();
        assert_eq!(tree.len(), if prompt == "parallel" { 3 } else { 4 });
        assert!(tree.iter().all(|run| run.status == "completed"), "{tree:?}");
        let sessions: std::collections::HashSet<_> =
            tree.iter().map(|run| &run.session_id).collect();
        assert_eq!(sessions.len(), tree.len());
    }
    assert_eq!(probe.peak.load(Ordering::SeqCst), 1);
    stop.cancel();
    worker.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancellation_and_parent_completion_supervise_children_and_release_capacity() {
    let (_dir, db) = database();
    let stop = CancellationToken::new();
    let worker = tokio::spawn(worker(
        db.clone(),
        Arc::new(Probe::default()),
        1,
        stop.clone(),
    ));
    let parent = db
        .submit(None, "worker".into(), "await".into(), 10)
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
    assert_eq!(terminal(&db, &parent.id).await.status, "cancelled");
    assert_eq!(terminal(&db, &child.id).await.status, "cancelled");
    let unawaited = db
        .submit(None, "worker".into(), "spawn".into(), 10)
        .unwrap();
    assert_eq!(terminal(&db, &unawaited.id).await.status, "completed");
    let child = db
        .runs(None)
        .unwrap()
        .into_iter()
        .find(|run| run.parent_run_id.as_deref() == Some(&unawaited.id))
        .unwrap();
    assert_eq!(terminal(&db, &child.id).await.status, "cancelled");
    let next = db.submit(None, "worker".into(), "leaf".into(), 10).unwrap();
    assert_eq!(terminal(&db, &next.id).await.status, "completed");
    stop.cancel();
    worker.await.unwrap().unwrap();
}

#[tokio::test]
async fn expired_queued_children_never_execute_and_waiting_parents_keep_their_deadline() {
    let (_dir, db) = database();
    let parent = root(&db);
    let child = db.delegate(&parent.id, "worker", "leaf", Some(1)).unwrap();
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let probe = Arc::new(Probe::default());
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(db.clone(), probe.clone(), 1, stop.clone()));
    let finished = terminal(&db, &child.id).await;
    assert_eq!(finished.status, "failed");
    assert_eq!(finished.error.as_deref(), Some("run deadline exceeded"));
    assert_eq!(probe.peak.load(Ordering::SeqCst), 0);
    db.finish(&parent.id, "completed", None).unwrap();
    let waiting = db.submit(None, "worker".into(), "await".into(), 1).unwrap();
    let start = std::time::Instant::now();
    assert_eq!(terminal(&db, &waiting.id).await.status, "failed");
    assert!(start.elapsed() < Duration::from_secs(3));
    let waiting = db.run(&waiting.id).unwrap().unwrap();
    for child in db
        .runs(None)
        .unwrap()
        .into_iter()
        .filter(|run| run.parent_run_id.as_deref() == Some(&waiting.id))
    {
        assert!(child.deadline_at <= waiting.deadline_at);
        assert!(terminal(&db, &child.id).await.terminal());
    }
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn parallel_trees_share_execution_capacity_without_unbounded_root_admission() {
    let (_dir, db) = database();
    let probe = Arc::new(Probe::default());
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(db.clone(), probe.clone(), 2, stop.clone()));
    let mut roots = Vec::new();
    for _ in 0..4 {
        roots.push(
            db.submit(None, "worker".into(), "parallel".into(), 10)
                .unwrap(),
        );
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let runs = db.runs(None).unwrap();
            assert!(
                runs.iter()
                    .filter(|run| run.parent_run_id.is_none() && run.status == "running")
                    .count()
                    <= 2
            );
            if roots.iter().all(|root| {
                runs.iter()
                    .find(|run| run.id == root.id)
                    .unwrap()
                    .terminal()
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(probe.peak.load(Ordering::SeqCst), 2);
    assert!(
        db.runs(None)
            .unwrap()
            .iter()
            .all(|run| run.status == "completed")
    );
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancelling_a_waiting_parent_does_not_release_its_session_to_another_turn_early() {
    let (_dir, db) = database();
    let probe = Arc::new(Probe::default());
    let stop = CancellationToken::new();
    let task = tokio::spawn(worker(db.clone(), probe, 2, stop.clone()));
    let parent = db
        .submit(None, "worker".into(), "await".into(), 10)
        .unwrap();
    let later = db
        .submit(
            Some(parent.session_id.clone()),
            "worker".into(),
            "leaf".into(),
            10,
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if db
                .events(&parent.id, 0)
                .unwrap()
                .iter()
                .any(|event| event.data["kind"] == "delegation_waiting")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(db.run(&later.id).unwrap().unwrap().status, "queued");
    db.cancel(&parent.id).unwrap();
    assert_eq!(terminal(&db, &parent.id).await.status, "cancelled");
    assert_eq!(terminal(&db, &later.id).await.status, "completed");
    stop.cancel();
    task.await.unwrap().unwrap();
}
