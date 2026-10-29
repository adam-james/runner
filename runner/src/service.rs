use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::{File, OpenOptions};
use std::future::IntoFuture;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::Instrument;

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

use crate::db::{Database, Event, Run, Schedule};
use crate::delegation::ExecutionContext;
use tokio::sync::Semaphore;

pub trait Executor: Send + Sync + 'static {
    /// Runs `run`'s turn on `transcript`, its session's conversation so far,
    /// and returns the conversation with the turn added.
    fn execute(
        &self,
        run: Run,
        transcript: Option<String>,
        events: host::EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<String>>;

    fn execute_with_context(
        &self,
        run: Run,
        transcript: Option<String>,
        events: host::EventSink,
        cancel: CancellationToken,
        _context: Arc<ExecutionContext>,
    ) -> BoxFuture<'static, Result<String>> {
        self.execute(run, transcript, events, cancel)
    }

    /// The agent versions a run of `agent` would use, the agent's own and
    /// its delegates'; see `host::version::pin`. A scheduled run starts only
    /// if they're what its schedule was approved for, so `None` refuses
    /// every one.
    fn pin(&self, _agent: &str) -> Option<host::version::Pin> {
        None
    }
}

struct ChildSession {
    runtime: host::Runtime,
    session: Option<String>,
}
impl Drop for ChildSession {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            self.runtime.forget_session(session);
        }
    }
}

pub struct WasmExecutor(pub host::Runtime);

impl Executor for WasmExecutor {
    fn execute(
        &self,
        run: Run,
        transcript: Option<String>,
        events: host::EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<String>> {
        let runtime = self.0.clone();
        Box::pin(async move {
            // Dropping the losing branch's future stops the guest at its next epoch yield.
            tokio::select! {
                result = runtime.run_turn(&run.agent, &run.session_id, transcript.as_deref(), &run.prompt, events) => result.map_err(Into::into),
                _ = cancel.cancelled() => bail!("run cancelled"),
            }
        })
    }

    fn execute_with_context(
        &self,
        run: Run,
        transcript: Option<String>,
        events: host::EventSink,
        cancel: CancellationToken,
        context: Arc<ExecutionContext>,
    ) -> BoxFuture<'static, Result<String>> {
        let runtime = self.0.clone();
        Box::pin(async move {
            let _session = ChildSession {
                runtime: runtime.clone(),
                session: run.parent_run_id.as_ref().map(|_| run.session_id.clone()),
            };
            tokio::select! {
                result = runtime.run_turn_with_delegation(&run.agent, &run.session_id, transcript.as_deref(), &run.prompt, events, Some(context)) => result.map_err(Into::into),
                _ = cancel.cancelled() => bail!("run cancelled"),
            }
        })
    }

    fn pin(&self, agent: &str) -> Option<host::version::Pin> {
        self.0.pin(agent)
    }
}

#[derive(Clone)]
pub struct Service {
    pub db: Database,
    token: Arc<String>,
    /// The agent definitions runs may name; fixed for the daemon's lifetime.
    agents: Arc<Vec<host::AgentInfo>>,
    /// What each configured provider and tool can do; also fixed.
    components: Arc<Vec<host::inspect::Capabilities>>,
    /// Each agent definition's current version, by agent name; stored when
    /// a schedule is approved for it.
    versions: Arc<std::collections::BTreeMap<String, host::version::AgentVersion>>,
    /// When a client last listed notifications saying it posts them itself.
    posting: Arc<crate::notify::ClientPosting>,
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
    /// An agent definition's name; session owner or `default` if omitted.
    pub agent: Option<String>,
    #[serde(default = "default_deadline")]
    pub deadline_secs: u64,
    /// When this run notifies, overriding the default (`problems`); see
    /// `crate::notify::LEVELS`.
    #[serde(default)]
    pub notify: Option<String>,
}

pub fn default_deadline() -> u64 {
    600
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSession {
    id: Option<String>,
    agent: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSchedule {
    id: Option<String>,
    agent: String,
    prompt: String,
    /// A local time in `tz`, e.g. `2026-10-29T07:00`; or `in_secs` or `cron`.
    at: Option<String>,
    /// Seconds from now, e.g. for testing; or `at` or `cron`.
    in_secs: Option<u64>,
    /// A cron expression read in `tz`, e.g. `0 7 * * MON-FRI`, to recur; or
    /// `at` or `in_secs`.
    cron: Option<String>,
    /// An IANA time zone name, e.g. `Europe/Warsaw`; required, so a
    /// schedule never depends on the daemon's own zone.
    tz: String,
    #[serde(default = "default_deadline")]
    deadline_secs: u64,
    /// When its runs notify: see `crate::notify::LEVELS`.
    #[serde(default = "crate::notify::default_level")]
    notify: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetNotify {
    notify: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationFilter {
    /// Only unread notifications.
    #[serde(default)]
    unread: bool,
    #[serde(default = "default_notification_limit")]
    limit: usize,
    /// The client posts new notifications itself, so the daemon doesn't.
    #[serde(default)]
    posting: bool,
}

fn default_notification_limit() -> usize {
    100
}

/// An agent whose version changed, as `approve` reports it.
#[derive(Serialize, Deserialize)]
pub struct VersionChange {
    pub agent: String,
    pub from: Option<String>,
    pub to: Option<String>,
    /// The snapshot's top-level fields that differ, e.g. `tools` or
    /// `model`; `None` if either definition isn't stored.
    pub fields: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
pub struct Approval {
    pub schedule: Schedule,
    pub changes: Vec<VersionChange>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunFilter {
    session_id: Option<String>,
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

fn conflict(error: anyhow::Error) -> ApiError {
    ApiError(StatusCode::CONFLICT, error.to_string())
}

impl Service {
    pub fn new(db: Database, token: String, agents: Vec<host::AgentInfo>) -> Self {
        Self {
            db,
            token: Arc::new(token),
            agents: Arc::new(agents),
            components: Arc::default(),
            versions: Arc::default(),
            posting: Arc::default(),
        }
    }

    /// Record clients that post notifications themselves in `posting`, for
    /// the notifier.
    pub fn with_posting(mut self, posting: Arc<crate::notify::ClientPosting>) -> Self {
        self.posting = posting;
        self
    }

    /// Serve `components` from `/v1/components`.
    pub fn with_components(mut self, components: Vec<host::inspect::Capabilities>) -> Self {
        self.components = Arc::new(components);
        self
    }

    /// The definitions behind `agents`' versions, to store when a schedule
    /// is approved.
    pub fn with_versions(
        mut self,
        versions: std::collections::BTreeMap<String, host::version::AgentVersion>,
    ) -> Self {
        self.versions = Arc::new(versions);
        self
    }

    fn pin(&self, agent: &str) -> Option<host::version::Pin> {
        host::version::pin(&self.agents, agent)
    }

    /// The definitions of `pin`'s versions that this daemon has.
    fn definitions(&self, pin: &host::version::Pin) -> Vec<host::version::AgentVersion> {
        pin.iter()
            .filter_map(|(agent, id)| self.versions.get(agent).filter(|v| &v.id == id))
            .cloned()
            .collect()
    }

    /// `schedule` with the agents that changed since it was approved.
    fn with_changes(&self, mut schedule: Schedule) -> Schedule {
        schedule.changed_agents = host::version::changed(
            &schedule.pinned_versions,
            self.pin(&schedule.agent).as_ref(),
        );
        schedule
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/agents", get(list_agents))
            .route("/v1/agent-versions/{id}", get(agent_version))
            .route("/v1/components", get(list_components))
            .route("/v1/sessions", get(list_sessions).post(create_session))
            .route("/v1/sessions/{id}/audit", get(session_audit))
            .route("/v1/runs", get(list_runs).post(submit))
            .route("/v1/runs/{id}", get(inspect))
            .route("/v1/runs/{id}/cancel", post(cancel))
            .route("/v1/runs/{id}/events", get(events))
            .route("/v1/runs/{id}/audit", get(audit))
            .route("/v1/runs/{id}/summary", get(summary))
            .route("/v1/schedules", get(list_schedules).post(create_schedule))
            .route(
                "/v1/schedules/{id}",
                get(inspect_schedule).delete(delete_schedule),
            )
            .route("/v1/schedules/{id}/history", get(schedule_history))
            .route("/v1/schedules/{id}/pause", post(pause_schedule))
            .route("/v1/schedules/{id}/resume", post(resume_schedule))
            .route("/v1/schedules/{id}/approve", post(approve_schedule))
            .route("/v1/schedules/{id}/notify", post(set_schedule_notify))
            .route("/v1/notifications", get(list_notifications))
            .route("/v1/notifications/read", post(read_all_notifications))
            .route("/v1/notifications/{id}/read", post(read_notification))
            .layer(middleware::from_fn_with_state(self.clone(), authenticate))
            .layer(
                tower_http::trace::TraceLayer::new_for_http()
                    .make_span_with(|request: &axum::extract::Request| {
                        let route = request
                            .extensions()
                            .get::<axum::extract::MatchedPath>()
                            .map_or("unmatched", |path| path.as_str());
                        tracing::info_span!("api", method = ?request.method(), route)
                    })
                    .on_request(())
                    .on_failure(())
                    .on_response(
                        |response: &axum::http::Response<axum::body::Body>,
                         latency: std::time::Duration,
                         span: &tracing::Span| {
                            tracing::info!(parent: span, status = response.status().as_u16(),
                        elapsed_ms = latency.as_millis() as u64, "response headers");
                        },
                    ),
            )
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

async fn list_agents(State(s): State<Service>) -> Json<Vec<host::AgentInfo>> {
    Json(s.agents.to_vec())
}

/// What runs of an agent version were allowed, under the same token as
/// their audit records.
async fn agent_version(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<crate::db::AgentVersion>, ApiError> {
    s.db.blocking(move |db| db.agent_version(&id))
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "agent version not found".into()))
}

async fn list_components(State(s): State<Service>) -> Json<Vec<host::inspect::Capabilities>> {
    Json(s.components.to_vec())
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
    if !s.agents.iter().any(|known| known.name == input.agent) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown agent '{}'", input.agent),
        ));
    }
    Ok((
        StatusCode::CREATED,
        Json(
            s.db.blocking(move |db| db.create_session(input.id, input.agent))
                .await
                .map_err(bad)?,
        ),
    ))
}

async fn list_runs(
    State(s): State<Service>,
    Query(filter): Query<RunFilter>,
) -> Result<Json<Vec<Run>>, ApiError> {
    Ok(Json(
        s.db.blocking(move |db| db.runs(filter.session_id.as_deref()))
            .await
            .map_err(internal)?,
    ))
}

async fn submit(
    State(s): State<Service>,
    Json(input): Json<Submit>,
) -> Result<impl IntoResponse, ApiError> {
    let session_id = input.session_id.clone();
    let owner =
        s.db.blocking(move |db| {
            Ok(db
                .sessions()?
                .into_iter()
                .find(|session| Some(&session.id) == session_id.as_ref())
                .map(|session| session.agent))
        })
        .await
        .map_err(internal)?;
    let agent = input
        .agent
        .or(owner)
        .unwrap_or_else(|| host::DEFAULT_AGENT.to_string());
    if !s.agents.iter().any(|known| known.name == agent) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown agent '{agent}'"),
        ));
    }
    let run =
        s.db.blocking(move |db| {
            db.submit_notifying(
                input.session_id,
                agent,
                input.prompt,
                input.deadline_secs,
                input.notify,
            )
        })
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

/// A run's security audit records. Readable with the same local API token as
/// everything else; there's no separate audit role yet.
async fn audit(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Vec<Event>>, ApiError> {
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
        s.db.blocking(move |db| db.audit(&id))
            .await
            .map_err(internal)?,
    ))
}

/// What a run and the runs it delegated to did, from their audit records;
/// see `crate::summary`.
async fn summary(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<crate::summary::Summary>, ApiError> {
    s.db.blocking(move |db| db.summary(&id))
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "run not found".into()))
}

/// The audit records of every run in a session, under the same token as
/// a run's.
async fn session_audit(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Vec<Event>>, ApiError> {
    s.db.blocking(move |db| db.session_audit(&id))
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "session not found".into()))
}

async fn list_schedules(State(s): State<Service>) -> Result<Json<Vec<Schedule>>, ApiError> {
    let schedules = s.db.blocking(|db| db.schedules()).await.map_err(internal)?;
    Ok(Json(
        schedules
            .into_iter()
            .map(|schedule| s.with_changes(schedule))
            .collect(),
    ))
}

async fn create_schedule(
    State(s): State<Service>,
    Json(input): Json<CreateSchedule>,
) -> Result<impl IntoResponse, ApiError> {
    let pin = s.pin(&input.agent).ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown agent '{}'", input.agent),
        )
    })?;
    let cron = input.cron.as_deref().map(str::trim).map(String::from);
    let (at, next_fire_at) = match (&input.at, input.in_secs, &cron) {
        (Some(at), None, None) => crate::schedule::fire_time(at, &input.tz),
        (None, Some(secs), None) => crate::schedule::fire_after(secs, &input.tz),
        (None, None, Some(cron)) => {
            crate::schedule::next_occurrence(cron, &input.tz, crate::db::now()).and_then(|next| {
                next.ok_or_else(|| anyhow::anyhow!("cron expression '{cron}' never fires"))
            })
        }
        _ => Err(anyhow::anyhow!("give one of `at`, `in_secs` and `cron`")),
    }
    .map_err(bad)?;
    if next_fire_at < crate::db::now() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("{at} in {} has already passed", input.tz),
        ));
    }
    let versions = s.definitions(&pin);
    let schedule =
        s.db.blocking(move |db| {
            db.create_schedule(
                input.id,
                input.agent,
                input.prompt,
                at,
                input.tz,
                cron,
                input.notify,
                next_fire_at,
                input.deadline_secs,
                pin,
                &versions,
            )
        })
        .await
        .map_err(bad)?;
    Ok((StatusCode::CREATED, Json(s.with_changes(schedule))))
}

/// The schedule, or 404.
async fn find_schedule(s: &Service, id: &str) -> Result<Schedule, ApiError> {
    let id = id.to_string();
    s.db.blocking(move |db| db.schedule(&id))
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "schedule not found".into()))
}

async fn inspect_schedule(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Schedule>, ApiError> {
    Ok(Json(s.with_changes(find_schedule(&s, &id).await?)))
}

async fn schedule_history(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Vec<Event>>, ApiError> {
    find_schedule(&s, &id).await?;
    Ok(Json(
        s.db.blocking(move |db| db.schedule_history(&id))
            .await
            .map_err(internal)?,
    ))
}

async fn pause_schedule(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Schedule>, ApiError> {
    find_schedule(&s, &id).await?;
    let schedule =
        s.db.blocking(move |db| db.pause_schedule(&id, "paused by a client"))
            .await
            .map_err(conflict)?;
    Ok(Json(s.with_changes(schedule)))
}

/// Resumes a paused schedule whose agents haven't changed; one whose
/// agents have changed has to be approved again instead.
async fn resume_schedule(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Schedule>, ApiError> {
    let changed = s.with_changes(find_schedule(&s, &id).await?).changed_agents;
    if !changed.is_empty() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "{}; review and approve it instead",
                crate::db::version_change_reason(&changed)
            ),
        ));
    }
    let schedule =
        s.db.blocking(move |db| db.resume_schedule(&id))
            .await
            .map_err(conflict)?;
    Ok(Json(s.with_changes(schedule)))
}

/// Re-pins a schedule to its agents' current versions and makes it active,
/// reporting what changed since it was last approved.
async fn approve_schedule(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Approval>, ApiError> {
    let schedule = find_schedule(&s, &id).await?;
    let pin = s.pin(&schedule.agent).ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            format!(
                "agent '{}' or one of its delegates is no longer configured",
                schedule.agent
            ),
        )
    })?;
    let versions = s.definitions(&pin);
    let (schedule, previous) =
        s.db.blocking(move |db| db.approve_schedule(&id, pin, &versions))
            .await
            .map_err(conflict)?;
    let mut changes = Vec::new();
    for agent in host::version::changed(&previous, Some(&schedule.pinned_versions)) {
        let from = previous.get(&agent).cloned();
        let to = schedule.pinned_versions.get(&agent).cloned();
        let ids = [from.clone(), to.clone()];
        let definitions =
            s.db.blocking(move |db| {
                ids.iter()
                    .map(|id| match id {
                        Some(id) => Ok(db.agent_version(id)?.map(|v| v.definition)),
                        None => Ok(None),
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await
            .map_err(internal)?;
        let fields = match (&definitions[0], &definitions[1]) {
            (Some(Value::Object(old)), Some(Value::Object(new))) => {
                let mut fields: Vec<String> = old
                    .keys()
                    .chain(new.keys())
                    .filter(|key| old.get(*key) != new.get(*key))
                    .cloned()
                    .collect();
                fields.sort();
                fields.dedup();
                Some(fields)
            }
            _ => None,
        };
        changes.push(VersionChange {
            agent,
            from,
            to,
            fields,
        });
    }
    Ok(Json(Approval {
        schedule: s.with_changes(schedule),
        changes,
    }))
}

/// Changes when a schedule's runs notify the user.
async fn set_schedule_notify(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
    Json(input): Json<SetNotify>,
) -> Result<Json<Schedule>, ApiError> {
    find_schedule(&s, &id).await?;
    let schedule =
        s.db.blocking(move |db| db.set_schedule_notify(&id, &input.notify))
            .await
            .map_err(bad)?;
    Ok(Json(s.with_changes(schedule)))
}

/// Notifications, newest first: unread ones only with `?unread=true`, at
/// most `limit` (1–1000, default 100). `?posting=true` says the client
/// posts new ones itself, so the daemon leaves them to it for a while.
async fn list_notifications(
    State(s): State<Service>,
    Query(filter): Query<NotificationFilter>,
) -> Result<Json<Vec<crate::db::Notification>>, ApiError> {
    if !(1..=1000).contains(&filter.limit) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "limit must be between 1 and 1000".into(),
        ));
    }
    if filter.posting {
        s.posting.seen();
    }
    Ok(Json(
        s.db.blocking(move |db| db.notifications(filter.unread, filter.limit))
            .await
            .map_err(internal)?,
    ))
}

/// Marks a notification read: `{"marked": 1}`, or 0 if it already was.
async fn read_notification(
    State(s): State<Service>,
    ApiPath(id): ApiPath<i64>,
) -> Result<Json<Value>, ApiError> {
    let marked =
        s.db.blocking(move |db| {
            if !db.notification_exists(id)? {
                return Ok(None);
            }
            db.mark_read(Some(id)).map(Some)
        })
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "notification not found".into()))?;
    Ok(Json(json!({"marked": marked})))
}

/// Marks every unread notification read: `{"marked": n}`.
async fn read_all_notifications(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    let marked =
        s.db.blocking(|db| db.mark_read(None))
            .await
            .map_err(internal)?;
    Ok(Json(json!({"marked": marked})))
}

async fn delete_schedule(
    State(s): State<Service>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Schedule>, ApiError> {
    find_schedule(&s, &id).await?;
    let schedule =
        s.db.blocking(move |db| db.delete_schedule(&id))
            .await
            .map_err(conflict)?;
    Ok(Json(s.with_changes(schedule)))
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
        // Only the host makes events, and only its audit module makes these.
        // A record that can't be saved fails the run like any other event.
        // So does a version, which its audit records refer to.
        if value["kind"] == "agent_version" {
            if self
                .db
                .record_agent_version(
                    &self.id,
                    value["id"].as_str().unwrap_or_default(),
                    value["agent"].as_str().unwrap_or_default(),
                    &value["definition"],
                )
                .is_err()
            {
                self.failed = true;
            }
            return;
        }
        if value["kind"] == "audit" {
            if self.db.audit_event(&self.id, value).is_err() {
                self.failed = true;
            }
            return;
        }
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
    context: Arc<ExecutionContext>,
) -> Result<()> {
    // A scheduled run starts only as its schedule was approved: one queued
    // before a restart may otherwise start under a different config.
    if let Some(pinned) = &run.pinned_versions {
        let changed = host::version::changed(pinned, executor.pin(&run.agent).as_ref());
        if !changed.is_empty() {
            let reason = crate::db::version_change_reason(&changed);
            tracing::warn!(schedule = ?run.schedule_id, "scheduled run refused: its agent changed");
            db.blocking(move |db| db.refuse_scheduled_run(&run.id, &reason))
                .await?;
            return Ok(());
        }
    }
    let transcript = db
        .blocking({
            let session = run.session_id.clone();
            move |db| db.transcript(&session)
        })
        .await?;
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
    let sink: host::EventSink = Arc::new(move |value| {
        let _ = send.send(value);
    });
    tracing::info!("run started");
    let cancel = CancellationToken::new();
    let requested = db.cancellation(&run.id);
    let _execution_lease = context.clone();
    let execution_run = run.clone();
    let execution_cancel = cancel.clone();
    // Contain executor panics here so queued events drain before the terminal status.
    let mut future = Box::pin(
        std::panic::AssertUnwindSafe(async move {
            executor
                .execute_with_context(execution_run, transcript, sink, execution_cancel, context)
                .await
        })
        .catch_unwind(),
    );
    let remaining_ms = run
        .deadline_at
        .unwrap_or_else(|| crate::db::now() + run.deadline_secs as i64 * 1000)
        .saturating_sub(crate::db::now())
        .max(0) as u64;
    let deadline = tokio::time::sleep(Duration::from_millis(remaining_ms));
    tokio::pin!(deadline);
    let mut transcript = None;
    let (status, error): (&str, Option<String>) = if remaining_ms == 0 {
        ("failed", Some("run deadline exceeded".into()))
    } else if requested.is_cancelled() {
        ("cancelled", None)
    } else {
        tokio::select! {
            biased;
            // A turn that has finished wins over cancel, deadline and shutdown.
            result = &mut future => match result {
                Err(_) => ("failed", Some("internal error".into())),
                Ok(Ok(finished)) => {
                    transcript = Some(finished);
                    ("completed", None)
                }
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
    tracing::info!(
        outcome = if failed { "failed" } else { status },
        "run finished"
    );
    db.blocking(move |db| match transcript {
        // Every event has drained, so the turn's are all before `turn_saved`.
        Some(transcript) if !failed => db.complete(&run.id, &transcript),
        _ => db.finish(
            &run.id,
            if failed { "failed" } else { status },
            if failed {
                Some("could not persist run events")
            } else {
                error.as_deref()
            },
        ),
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
    let capacity = Arc::new(Semaphore::new(concurrency));
    let mut trees = HashMap::<String, String>::new();
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
                trees.retain(|id, _| runs.values().any(|run| run == id) || unfinished.contains(id));
                loop {
                    let Ok(permit) = capacity.clone().try_acquire_owned() else { break; };
                    let roots: std::collections::HashSet<_> = trees.values().collect();
                    let allow_roots = roots.len() < concurrency;
                    let run = match db.blocking(move |db| db.claim_for_worker(allow_roots)).await {
                        Ok(Some(run)) => run,
                        Ok(None) => break,
                        // Transient (e.g. SQLITE_BUSY); retry on the next tick.
                        Err(_error) => {
                            tracing::error!("could not claim run");
                            break;
                        }
                    };
                    let id = run.id.clone();
                    trees.insert(id.clone(), run.root_run_id.clone());
                    let context = Arc::new(ExecutionContext::new(db.clone(), run.clone(), capacity.clone(), permit));
                    let span = tracing::info_span!("run", run = ?run.id, session = ?run.session_id, agent = ?run.agent);
                    let job = jobs.spawn(execute(db.clone(), run, executor.clone(), shutdown.clone(), context).instrument(span));
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
    let (id, category) = match completed {
        Ok((id, Ok(()))) => {
            runs.remove(&id);
            return None;
        }
        Ok((id, Err(_))) => (id, "execution"),
        Err(error) => (error.id(), "task"),
    };
    let run = runs.remove(&id)?;
    tracing::error!(run = ?run, category, "run failed internally");
    if let Err(_error) = mark_failed(db, &run).await {
        tracing::error!(run = ?run, "could not mark run failed; will retry");
        return Some(run);
    }
    None
}

/// Where the notifier posts, if desktop notifications are on: nowhere but
/// macOS for now.
fn desktop(runtime: &host::Runtime) -> Option<Arc<dyn crate::notify::Desktop>> {
    if !runtime.desktop_notifications() {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(crate::notify::SystemDesktop {
            sound: runtime.notification_sound().map(String::from),
        }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        tracing::info!("desktop notifications are macOS only; see `runner notifications`");
        None
    }
}

pub async fn serve(config: &str, data_dir: &Path, concurrency: usize) -> Result<()> {
    if concurrency == 0 {
        bail!("concurrency must be positive");
    }
    let instance = LocalInstance::acquire(data_dir)?;
    let path = data_dir.join("state.sqlite3");
    let db = tokio::task::spawn_blocking(move || {
        let db = Database::open(&path)?;
        db.recover()?;
        Ok::<_, anyhow::Error>(db)
    })
    .await??;
    let runtime = host::Runtime::new(config).await?;
    runtime.check_dirs_exclude(data_dir)?;
    // Every current version, so each can be read, e.g. to review it before
    // approving a schedule for it, before any run has used it.
    let versions: Vec<_> = runtime.agent_versions().into_values().collect();
    db.blocking(move |db| db.store_agent_versions(&versions))
        .await?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let token = format!("{}{}", crate::db::new_id(), crate::db::new_id());
    let info = ConnectionInfo {
        port: listener.local_addr()?.port(),
        token: token.clone(),
    };
    instance.publish(&info)?;
    let posting = Arc::new(crate::notify::ClientPosting::default());
    let service = Service::new(db.clone(), token, runtime.agents())
        .with_components(runtime.capabilities())
        .with_versions(runtime.agent_versions())
        .with_posting(posting.clone());
    let shutdown = CancellationToken::new();
    let worker_stop = shutdown.clone();
    let http_stop = shutdown.clone();
    let desktop = desktop(&runtime);
    let executor: Arc<dyn Executor> = Arc::new(WasmExecutor(runtime));
    let scheduler = tokio::spawn(crate::schedule::scheduler(
        db.clone(),
        executor.clone(),
        shutdown.clone(),
    ));
    let notifier = tokio::spawn(crate::notify::notifier(
        db.clone(),
        desktop,
        posting,
        shutdown.clone(),
    ));
    let worker = worker(db, executor, concurrency, worker_stop);
    let http =
        axum::serve(listener, service.router()).with_graceful_shutdown(http_stop.cancelled_owned());
    tracing::info!(port = info.port, "runner listening on loopback");
    tokio::pin!(worker);
    let http = http.into_future();
    tokio::pin!(http);
    tokio::select! {
        result = &mut worker => { shutdown.cancel(); result?; }
        result = &mut http => { shutdown.cancel(); result?; worker.await?; }
        result = stop_signal() => { shutdown.cancel(); result?; worker.await?; }
    }
    let _ = scheduler.await;
    let _ = notifier.await;
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
