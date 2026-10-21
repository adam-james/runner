use std::convert::Infallible;
use std::fs::{File, OpenOptions};
use std::future::IntoFuture;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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
use futures_util::future::BoxFuture;
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
        let runtime = self.0.execution();
        Box::pin(async move {
            struct CancelOnDrop(host::Runtime);
            impl Drop for CancelOnDrop {
                fn drop(&mut self) {
                    self.0.cancel();
                }
            }
            let _guard = CancelOnDrop(runtime.clone());
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
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
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
    Ok(Json(s.db.sessions().map_err(internal)?))
}
async fn create_session(
    State(s): State<Service>,
    Json(input): Json<CreateSession>,
) -> Result<impl IntoResponse, ApiError> {
    Ok((
        StatusCode::CREATED,
        Json(s.db.create_session(input.id).map_err(bad)?),
    ))
}
async fn list_runs(State(s): State<Service>) -> Result<Json<Vec<Run>>, ApiError> {
    Ok(Json(s.db.runs().map_err(internal)?))
}
async fn submit(
    State(s): State<Service>,
    Json(input): Json<Submit>,
) -> Result<impl IntoResponse, ApiError> {
    let run =
        s.db.submit(input.session_id, input.prompt, input.deadline_secs)
            .map_err(bad)?;
    Ok((StatusCode::ACCEPTED, Json(run)))
}
async fn inspect(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Run>, ApiError> {
    Ok(Json(s.db.run(&id).map_err(internal)?.ok_or_else(|| {
        ApiError(StatusCode::NOT_FOUND, "run not found".into())
    })?))
}
async fn cancel(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Run>, ApiError> {
    if s.db.run(&id).map_err(internal)?.is_none() {
        return Err(ApiError(StatusCode::NOT_FOUND, "run not found".into()));
    }
    Ok(Json(s.db.cancel(&id).map_err(internal)?))
}

async fn events(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
    Query(cursor): Query<Cursor>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if s.db.run(&id).map_err(internal)?.is_none() {
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
    let stream = async_stream::stream! {
        loop {
            // Read terminal state first: a concurrent completion must never hide its last events.
            let terminal = match s.db.run(&id) { Ok(Some(run)) => run.terminal(), _ => break };
            let batch = match s.db.events(&id, after) { Ok(events) => events, Err(_) => break };
            let empty = batch.is_empty();
            for event in batch {
                after = event.id;
                yield Ok::<_, Infallible>(sse::Event::default().id(event.id.to_string())
                    .event("run").data(serde_json::to_string(&event).expect("event is serializable")));
            }
            if terminal && empty { break; }
            if !empty { continue; }
            tokio::time::sleep(Duration::from_millis(50)).await;
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
    let buffer = Arc::new(Mutex::new(BufferedEvents {
        db: db.clone(),
        id: run.id.clone(),
        text: String::new(),
        last_flush: Instant::now(),
        failed: false,
    }));
    let writer = buffer.clone();
    let sink: host::EventSink = Arc::new(move |value| writer.lock().unwrap().push(value));
    let cancel = CancellationToken::new();
    let mut future = executor.execute(run.clone(), sink, cancel.clone());
    let deadline = tokio::time::sleep(Duration::from_secs(run.deadline_secs));
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    let (status, error) = loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break ("interrupted", Some("daemon stopped during execution")),
            _ = &mut deadline => break ("failed", Some("run deadline exceeded")),
            _ = poll.tick() => {
                let mut writer = buffer.lock().unwrap();
                if writer.last_flush.elapsed() >= Duration::from_millis(50) { writer.flush(); }
                if writer.failed { break ("failed", Some("could not persist run events")); }
                drop(writer);
                if db.run(&run.id)?.is_some_and(|r| r.cancel_requested) { break ("cancelled", None); }
            }
            result = &mut future => break match result {
                Ok(()) => ("completed", None),
                // Component/provider errors can contain reflected request headers or secrets.
                Err(_) => ("failed", Some("agent execution failed")),
            },
        }
    };
    cancel.cancel();
    // Drop all guest execution before publishing a terminal status.
    drop(future);
    let mut writer = buffer.lock().unwrap();
    writer.flush();
    let failed = writer.failed;
    drop(writer);
    db.finish(
        &run.id,
        if failed { "failed" } else { status },
        if failed {
            Some("could not persist run events")
        } else {
            error
        },
    )?;
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
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            completed = jobs.join_next(), if !jobs.is_empty() => {
                completed.context("worker task disappeared")???;
            }
            _ = tick.tick() => {
                while jobs.len() < concurrency {
                    let Some(run) = db.claim()? else { break; };
                    jobs.spawn(execute(db.clone(), run, executor.clone(), shutdown.clone()));
                }
            }
        }
    }
    while let Some(result) = jobs.join_next().await {
        result??;
    }
    Ok(())
}

pub async fn serve(config: &str, data_dir: &Path, concurrency: usize) -> Result<()> {
    if concurrency == 0 {
        bail!("concurrency must be positive");
    }
    let instance = LocalInstance::acquire(data_dir)?;
    let runtime = host::Runtime::new(config)?;
    let db = Database::open(&data_dir.join("state.sqlite3"))?;
    db.recover()?;
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
