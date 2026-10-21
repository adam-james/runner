use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::{File, OpenOptions};
use std::future::IntoFuture;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path as ApiPath, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse},
    routing::{get, post},
};
use futures_util::{FutureExt, future::BoxFuture};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::db::{Database, Run};

pub trait Executor: Send + Sync + 'static {
    fn execute(
        &self,
        run: Run,
        events: host::EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<()>>;
}

pub struct WasmExecutor(pub host::Runtime);

impl Executor for WasmExecutor {
    fn execute(
        &self,
        run: Run,
        events: host::EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<()>> {
        let runtime = self.0.clone();
        Box::pin(async move {
            // Dropping the losing branch's future stops the guest at its next epoch yield.
            tokio::select! {
                result = runtime.run_turn(&run.session_id, &run.prompt, events) => result.map_err(Into::into),
                _ = cancel.cancelled() => bail!("run cancelled"),
            }
        })
    }
}

#[derive(Clone)]
pub struct Service {
    pub db: Database,
    token: Arc<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub port: u16,
    pub token: String,
}

/// The lock is the authority; a descriptor left by a crash is safe to replace.
pub struct LocalInstance {
    _lock: File,
    descriptor: PathBuf,
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

impl LocalInstance {
    pub fn acquire(dir: &Path) -> Result<Self> {
        // Only directories we create are made private; never chmod a user-supplied one like $HOME.
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)?;
        let file = private_options().open(dir.join("daemon.lock"))?;
        file.try_lock()
            .context("a daemon is already using this data directory")?;
        Ok(Self {
            _lock: file,
            descriptor: dir.join("connection.json"),
        })
    }

    pub fn publish(&self, info: &ConnectionInfo) -> Result<()> {
        let tmp = self.descriptor.with_extension("tmp");
        let mut file = private_options().truncate(true).open(&tmp)?;
        file.write_all(&serde_json::to_vec(info)?)?;
        file.sync_all()?;
        std::fs::rename(tmp, &self.descriptor)?;
        Ok(())
    }
}

impl Drop for LocalInstance {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.descriptor);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submit {
    pub prompt: String,
    pub session_id: Option<String>,
    #[serde(default = "default_deadline")]
    pub deadline_secs: u64,
}

pub fn default_deadline() -> u64 {
    600
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSession {
    id: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    after: Option<i64>,
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}

fn internal(_: anyhow::Error) -> ApiError {
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "state operation failed".into(),
    )
}

fn bad(error: anyhow::Error) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, error.to_string())
}

impl Service {
    pub fn new(db: Database, token: String) -> Self {
        Self {
            db,
            token: Arc::new(token),
        }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/sessions", get(list_sessions).post(create_session))
            .route("/v1/runs", get(list_runs).post(submit))
            .route("/v1/runs/{id}", get(inspect))
            .route("/v1/runs/{id}/cancel", post(cancel))
            .route("/v1/runs/{id}/events", get(events))
            .layer(middleware::from_fn_with_state(self.clone(), authenticate))
            .with_state(self.clone())
    }
}

async fn authenticate(
    State(service): State<Service>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    // Browser origins are unnecessary for native clients. No permissive CORS.
    if request.headers().contains_key("origin")
        || request
            .headers()
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            != Some(&format!("Bearer {}", service.token))
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        )
            .into_response();
    }
    next.run(request).await
}

async fn list_sessions(
    State(s): State<Service>,
) -> Result<Json<Vec<crate::db::Session>>, ApiError> {
    Ok(Json(
        s.db.blocking(|db| db.sessions()).await.map_err(internal)?,
    ))
}

async fn create_session(
    State(s): State<Service>,
    Json(input): Json<CreateSession>,
) -> Result<impl IntoResponse, ApiError> {
    Ok((
        StatusCode::CREATED,
        Json(
            s.db.blocking(move |db| db.create_session(input.id))
                .await
                .map_err(bad)?,
        ),
    ))
}

async fn list_runs(State(s): State<Service>) -> Result<Json<Vec<Run>>, ApiError> {
    Ok(Json(s.db.blocking(|db| db.runs()).await.map_err(internal)?))
}

async fn submit(
    State(s): State<Service>,
    Json(input): Json<Submit>,
) -> Result<impl IntoResponse, ApiError> {
    let run =
        s.db.blocking(move |db| db.submit(input.session_id, input.prompt, input.deadline_secs))
            .await
            .map_err(bad)?;
    Ok((StatusCode::ACCEPTED, Json(run)))
}

async fn inspect(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Run>, ApiError> {
    Ok(Json(
        s.db.blocking({
            let id = id.clone();
            move |db| db.run(&id)
        })
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "run not found".into()))?,
    ))
}

async fn cancel(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Run>, ApiError> {
    if s.db
        .blocking({
            let id = id.clone();
            move |db| db.run(&id)
        })
        .await
        .map_err(internal)?
        .is_none()
    {
        return Err(ApiError(StatusCode::NOT_FOUND, "run not found".into()));
    }
    Ok(Json(
        s.db.blocking(move |db| db.cancel(&id))
            .await
            .map_err(internal)?,
    ))
}

async fn events(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
    Query(cursor): Query<Cursor>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if s.db
        .blocking({
            let id = id.clone();
            move |db| db.run(&id)
        })
        .await
        .map_err(internal)?
        .is_none()
    {
        return Err(ApiError(StatusCode::NOT_FOUND, "run not found".into()));
    }
    let header_cursor = headers
        .get("last-event-id")
        .map(|h| h.to_str().ok().and_then(|s| s.parse::<i64>().ok()));
    if matches!(header_cursor, Some(None)) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "invalid Last-Event-ID".into(),
        ));
    }
    let mut after = cursor.after.or(header_cursor.flatten()).unwrap_or(0);
    if after < 0 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "cursor must be nonnegative".into(),
        ));
    }
    let changes = s.db.changes(&id);
    let stream = async_stream::stream! {
        loop {
            // Register before reading so commits between the query and wait cannot be lost.
            let notified = changes.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            // Read terminal state first: a concurrent completion must never hide its last events.
            let terminal = match s.db.blocking({ let id = id.clone(); move |db| db.run(&id) }).await { Ok(Some(run)) => run.terminal(), _ => break };
            let batch = match s.db.blocking({ let id = id.clone(); move |db| db.events(&id, after) }).await { Ok(events) => events, Err(_) => break };
            let empty = batch.is_empty();
            for event in batch {
                after = event.id;
                yield Ok::<_, Infallible>(sse::Event::default().id(event.id.to_string())
                    .event("run").data(serde_json::to_string(&event).expect("event is serializable")));
            }
            if terminal && empty { break; }
            if !empty { continue; }
            notified.await;
        }
    };
    Ok(Sse::new(stream).keep_alive(sse::KeepAlive::default()))
}

struct BufferedEvents {
    db: Database,
    id: String,
    text: String,
    last_flush: Instant,
    failed: bool,
}

impl BufferedEvents {
    fn flush(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        if self
            .db
            .event(&self.id, json!({"kind":"text", "text":text}))
            .is_err()
        {
            self.failed = true;
        }
        self.last_flush = Instant::now();
    }

    fn push(&mut self, value: Value) {
        if value["kind"] == "text" {
            self.text
                .push_str(value["text"].as_str().unwrap_or_default());
            if self.text.len() >= 8192 || self.last_flush.elapsed() >= Duration::from_millis(50) {
                self.flush();
            }
        } else {
            self.flush();
            if self.db.event(&self.id, value).is_err() {
                self.failed = true;
            }
        }
    }
}

async fn execute(
    db: Database,
    run: Run,
    executor: Arc<dyn Executor>,
    shutdown: CancellationToken,
) -> Result<()> {
    let (send, receive) = std::sync::mpsc::channel();
    let writer_db = db.clone();
    let writer_id = run.id.clone();
    let failed = CancellationToken::new();
    let writer_failed = failed.clone();
    // One blocking consumer preserves event order and batches text without fsync on Tokio.
    let writer = tokio::task::spawn_blocking(move || {
        let mut buffer = BufferedEvents {
            db: writer_db,
            id: writer_id,
            text: String::new(),
            last_flush: Instant::now(),
            failed: false,
        };
        loop {
            let value = if buffer.text.is_empty() {
                receive
                    .recv()
                    .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)
            } else {
                receive.recv_timeout(
                    Duration::from_millis(50).saturating_sub(buffer.last_flush.elapsed()),
                )
            };
            match value {
                Ok(value) => buffer.push(value),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => buffer.flush(),
            }
            if buffer.failed {
                writer_failed.cancel();
            }
        }
        buffer.flush();
        buffer.failed
    });
    let saved = Arc::new(AtomicBool::new(false));
    let turn_saved = saved.clone();
    let sink: host::EventSink = Arc::new(move |value| {
        if value["kind"] == "turn_saved" {
            // Observe the save synchronously, before the blocking writer persists its event.
            turn_saved.store(true, Ordering::Release);
        }
        let _ = send.send(value);
    });
    let cancel = CancellationToken::new();
    let requested = db.cancellation(&run.id);
    let execution_run = run.clone();
    let execution_cancel = cancel.clone();
    // Contain executor panics here so queued events drain before the terminal status.
    let mut future = Box::pin(
        std::panic::AssertUnwindSafe(async move {
            executor
                .execute(execution_run, sink, execution_cancel)
                .await
        })
        .catch_unwind(),
    );
    let deadline = tokio::time::sleep(Duration::from_secs(run.deadline_secs));
    tokio::pin!(deadline);
    let (status, error): (&str, Option<String>) = {
        tokio::select! {
            biased;
            // A finished turn has already been saved, so it wins over cancel, deadline and shutdown.
            result = &mut future => match result {
                Err(_) => ("failed", Some("internal error".into())),
                Ok(Ok(())) => ("completed", None),
                Ok(Err(error)) => ("failed", Some(match error.downcast_ref::<host::TurnFailed>() {
                    // The agent's own message for the user, with credentials redacted by the host.
                    Some(failed) => failed.0.clone(),
                    // Other component/provider errors can contain reflected request headers or secrets.
                    None => "agent execution failed".into(),
                })),
            },
            _ = shutdown.cancelled() => ("interrupted", Some("daemon stopped during execution".into())),
            _ = &mut deadline => ("failed", Some("run deadline exceeded".into())),
            _ = requested.cancelled() => ("cancelled", None),
            _ = failed.cancelled() => ("failed", Some("could not persist run events".into())),
        }
    };
    cancel.cancel();
    // Drop all guest execution before publishing a terminal status.
    drop(future);
    let failed = writer.await?;
    // Cancellation can win the select after session.save succeeds but before the
    // agent returns. Its transcript is already committed, so keep the run completed.
    let (status, error) = if saved.load(Ordering::Acquire) {
        ("completed", None)
    } else {
        (status, error)
    };
    db.blocking(move |db| {
        db.finish(
            &run.id,
            if failed { "failed" } else { status },
            if failed {
                Some("could not persist run events")
            } else {
                error.as_deref()
            },
        )
    })
    .await?;
    Ok(())
}

/// Exactly one worker manages claims; jobs execute concurrently with bounded capacity.
pub async fn worker(
    db: Database,
    executor: Arc<dyn Executor>,
    concurrency: usize,
    shutdown: CancellationToken,
) -> Result<()> {
    if concurrency == 0 {
        bail!("concurrency must be positive");
    }
    let mut jobs = JoinSet::new();
    let mut runs = HashMap::new();
    // Runs whose terminal status could not be written; each holds its session until it is.
    let mut unfinished = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            Some(completed) = jobs.join_next_with_id(), if !jobs.is_empty() => {
                unfinished.extend(contain(&db, &mut runs, completed).await);
            }
            _ = tick.tick() => {
                retry_unfinished(&db, &mut unfinished).await;
                while jobs.len() < concurrency {
                    let run = match db.blocking(|db| db.claim()).await {
                        Ok(Some(run)) => run,
                        Ok(None) => break,
                        // Transient (e.g. SQLITE_BUSY); retry on the next tick.
                        Err(error) => {
                            eprintln!("could not claim run: {error:#}");
                            break;
                        }
                    };
                    let id = run.id.clone();
                    let job = jobs.spawn(execute(db.clone(), run, executor.clone(), shutdown.clone()));
                    runs.insert(job.id(), id);
                }
            }
        }
    }
    while let Some(completed) = jobs.join_next_with_id().await {
        unfinished.extend(contain(&db, &mut runs, completed).await);
    }
    // Anything still unfinished is marked interrupted by `recover` on the next start.
    retry_unfinished(&db, &mut unfinished).await;
    Ok(())
}

async fn mark_failed(db: &Database, run: &str) -> Result<()> {
    let run = run.to_string();
    // No-op if execute already published a terminal status.
    db.blocking(move |db| db.finish(&run, "failed", Some("internal error")))
        .await
}

async fn retry_unfinished(db: &Database, unfinished: &mut Vec<String>) {
    let mut remaining = Vec::new();
    for run in unfinished.drain(..) {
        if mark_failed(db, &run).await.is_err() {
            remaining.push(run);
        }
    }
    *unfinished = remaining;
}

/// One job's error or panic must not stop the daemon or abort its sibling runs.
/// Returns the run if its terminal status still needs to be written.
async fn contain(
    db: &Database,
    runs: &mut HashMap<tokio::task::Id, String>,
    completed: Result<(tokio::task::Id, Result<()>), tokio::task::JoinError>,
) -> Option<String> {
    let (id, error) = match completed {
        Ok((id, Ok(()))) => {
            runs.remove(&id);
            return None;
        }
        Ok((id, Err(error))) => (id, format!("{error:#}")),
        Err(error) => (error.id(), error.to_string()),
    };
    let run = runs.remove(&id)?;
    eprintln!("run {run} failed internally: {error}");
    if let Err(error) = mark_failed(db, &run).await {
        eprintln!("could not mark run {run} failed, will retry: {error:#}");
        return Some(run);
    }
    None
}

pub async fn serve(config: &str, data_dir: &Path, concurrency: usize) -> Result<()> {
    if concurrency == 0 {
        bail!("concurrency must be positive");
    }
    let instance = LocalInstance::acquire(data_dir)?;
    let runtime = host::Runtime::new(config)?;
    let path = data_dir.join("state.sqlite3");
    let db = tokio::task::spawn_blocking(move || {
        let db = Database::open(&path)?;
        db.recover()?;
        Ok::<_, anyhow::Error>(db)
    })
    .await??;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let token = format!("{}{}", crate::db::new_id(), crate::db::new_id());
    let info = ConnectionInfo {
        port: listener.local_addr()?.port(),
        token: token.clone(),
    };
    instance.publish(&info)?;
    let service = Service::new(db.clone(), token);
    let shutdown = CancellationToken::new();
    let worker_stop = shutdown.clone();
    let http_stop = shutdown.clone();
    let worker = worker(
        db,
        Arc::new(WasmExecutor(runtime)),
        concurrency,
        worker_stop,
    );
    let http =
        axum::serve(listener, service.router()).with_graceful_shutdown(http_stop.cancelled_owned());
    eprintln!("runner listening on 127.0.0.1:{}", info.port);
    tokio::pin!(worker);
    let http = http.into_future();
    tokio::pin!(http);
    tokio::select! {
        result = &mut worker => { shutdown.cancel(); result?; }
        result = &mut http => { shutdown.cancel(); result?; worker.await?; }
        result = stop_signal() => { shutdown.cancel(); result?; worker.await?; }
    }
    // Dropping the HTTP server also closes any attached event streams.
    drop(instance);
    Ok(())
}

async fn stop_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
