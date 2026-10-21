use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot};
use wasmtime::component::{
    Accessor, Component, Destination, FutureConsumer, FutureReader, HasSelf, Lift, Linker,
    ResourceTable, Source, StreamConsumer, StreamProducer, StreamReader, StreamResult,
};
use wasmtime::{
    Cache, Config, Engine, Error, Result, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder,
    bail,
};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

wasmtime::component::bindgen!({
    world: "chat-guest",
    path: "../wit",
    imports: {
        // Call into the tool's or session store's own store, which is async.
        "local:agent/chat-api.call-tool": async,
        "local:agent/session.load": async,
        "local:agent/session.save": async,
    },
});

use local::agent::messages::Message;
use local::agent::types::{ChatError, ChatRequest, ChatResponse, ChatStream};
use local::agent::{chat_api, run_events, session};

// Scoped in their own modules so each world's generated types (also under
// the `local:agent` package, since wit_bindgen requires one package per
// directory) don't collide with the chat-guest bindgen above.
mod tool_guest {
    wasmtime::component::bindgen!({
        world: "tool-guest",
        path: "../wit",
        exports: { default: async },
    });
}
use tool_guest::exports::local::agent::tool_api::ToolDescription;

mod provider_guest {
    wasmtime::component::bindgen!({
        world: "provider-guest",
        path: "../wit",
        // Reuse the chat-guest bindings' types, so a `chat-request` from the
        // agent can be handed straight to the provider, and the reply's
        // pieces straight back, with no conversion in between.
        with: {
            "local:agent/messages": crate::local::agent::messages,
            "local:agent/types": crate::local::agent::types,
        },
    });
}
use provider_guest::exports::local::agent::provider_api::ToolDefinition;

mod session_store_guest {
    wasmtime::component::bindgen!({
        world: "session-store-guest",
        path: "../wit",
        exports: { default: async },
        // So the agent's messages can be handed to the store as they are.
        with: {
            "local:agent/messages": crate::local::agent::messages,
        },
    });
}

#[derive(Clone, serde::Deserialize)]
struct RunnerConfig {
    agent: String,
    provider: ProviderConfig,
    #[serde(default)]
    tools: Vec<ComponentConfig>,
    /// Required in service mode; validated when constructing the runtime.
    session_store: Option<ComponentConfig>,
}

#[derive(Clone, serde::Deserialize)]
struct ProviderConfig {
    model: String,
    #[serde(flatten)]
    component: ComponentConfig,
}

/// A sandboxed component with network access: a provider or a tool.
#[derive(Clone, serde::Deserialize)]
struct ComponentConfig {
    #[serde(skip)]
    resolved: bool,
    path: String,
    #[serde(default)]
    allowed_hosts: Vec<String>,
    /// Headers the host adds to this component's outbound HTTPS requests,
    /// e.g. credentials. Values may reference env vars as `${NAME}`, which
    /// are resolved by the host -- the component itself never sees them.
    #[serde(default)]
    headers: BTreeMap<String, HeaderConfig>,
    /// A directory on the host the component can read, mounted as its `/`.
    /// Read-only, and must already exist, unless `dir_writable` is set. May
    /// reference env vars like `headers` values can.
    dir: Option<String>,
    /// Let the component change, add and delete things in `dir` too, which
    /// is created if it doesn't exist.
    #[serde(default)]
    dir_writable: bool,
    /// Tools only: how long one call may run, in seconds, before it's cut
    /// off and the model gets a "timed out" error. Without one, a call can
    /// run forever.
    timeout_secs: Option<u64>,
    /// Cap on the component's memory, in MiB. Past it, growing memory fails
    /// inside the component (e.g. Python's `MemoryError`).
    max_memory_mb: Option<u64>,
    /// Tools only: give every call a new instance, so nothing carries over
    /// between calls and a call that's cut off doesn't break the tool.
    #[serde(default)]
    fresh_per_call: bool,
}

/// A header's value and the hosts it's sent to. `hosts` is required, even
/// when it just repeats a single allowed host, so adding a host to
/// `allowed_hosts` later never silently starts sending it credentials.
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderConfig {
    value: String,
    hosts: Vec<String>,
}

fn load_config(path: &str) -> Result<RunnerConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::msg(format!("reading config {path}: {e}")))?;
    toml::from_str(&text).map_err(|e| Error::msg(format!("parsing config {path}: {e}")))
}

/// Replaces every `${NAME}` in `template` with the value of env var `NAME`.
fn expand_env(template: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        let end = rest[start..]
            .find('}')
            .ok_or_else(|| Error::msg(format!("unterminated ${{ in '{template}'")))?;
        let name = &rest[start + 2..start + end];
        let value = env::var(name)
            .map_err(|_| Error::msg(format!("environment variable {name} is not set")))?;
        out.push_str(&rest[..start]);
        out.push_str(&value);
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// How often the engine's epoch advances; the precision of `timeout_secs`.
const EPOCH_TICK: Duration = Duration::from_millis(10);

/// The most of a tool's result (or error) passed on to the model, in bytes.
/// Enforced here rather than in the tool, so a tool running untrusted code
/// (like the python one) can't get around it to flood the model's context.
const MAX_TOOL_OUTPUT: usize = 64 * 1024;

/// `output`, cut to at most `MAX_TOOL_OUTPUT` bytes plus a note saying so.
fn cap_tool_output(mut output: String) -> String {
    if output.len() <= MAX_TOOL_OUTPUT {
        return output;
    }
    let total = output.len();
    let mut end = MAX_TOOL_OUTPUT;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output.truncate(end);
    output.push_str(&format!("\n... [cut by host: {} more bytes]", total - end));
    output
}

struct InjectedHeader {
    name: http::HeaderName,
    value: http::HeaderValue,
    hosts: HashSet<String>,
}

/// Rejects any outbound request whose host isn't on this component's own
/// allow-list, before it ever reaches the real network, and adds its
/// configured headers (credentials) to the ones that are allowed.
struct SandboxHooks {
    allowed_hosts: HashSet<String>,
    headers: Vec<InjectedHeader>,
}

impl SandboxHooks {
    fn new(config: &ComponentConfig) -> Result<Self> {
        let allowed_hosts: HashSet<String> = config
            .allowed_hosts
            .iter()
            .map(|h| h.to_lowercase())
            .collect();

        let mut headers = Vec::new();
        for (name, header) in &config.headers {
            let context = |e: &dyn std::fmt::Display| {
                Error::msg(format!("header '{name}' for {}: {e}", config.path))
            };
            let hosts: HashSet<String> = header.hosts.iter().map(|h| h.to_lowercase()).collect();
            // Either would silently mean the header is never sent.
            if hosts.is_empty() {
                return Err(context(&"`hosts` is empty"));
            }
            if let Some(host) = hosts.iter().find(|h| !allowed_hosts.contains(*h)) {
                return Err(context(&format!("host '{host}' is not in allowed_hosts")));
            }
            let name = http::HeaderName::try_from(name.as_str()).map_err(|e| context(&e))?;
            let value = if config.resolved {
                header.value.clone()
            } else {
                expand_env(&header.value).map_err(|e| context(&e))?
            };
            let mut value = http::HeaderValue::try_from(value).map_err(|e| context(&e))?;
            value.set_sensitive(true);
            headers.push(InjectedHeader { name, value, hosts });
        }

        Ok(Self {
            allowed_hosts,
            headers,
        })
    }

    /// Adds the configured headers that apply to `request`, which must
    /// already have passed the allow-list check. Overwrites any same-named
    /// header the guest set itself, so a component can't substitute its own
    /// credentials for the host's.
    fn add_headers<B>(&self, request: &mut http::Request<B>, host: &str) {
        // Never put credentials on a plain-text request, even to an allowed
        // host -- the component doesn't get to downgrade the connection.
        let https = request.uri().scheme() == Some(&http::uri::Scheme::HTTPS);
        for header in &self.headers {
            if !header.hosts.contains(host) {
                continue;
            }
            if !https {
                eprintln!(
                    "not sending header '{}' over plain http to {host}",
                    header.name
                );
                continue;
            }
            request
                .headers_mut()
                .insert(header.name.clone(), header.value.clone());
        }
    }
}

impl WasiHttpHooks for SandboxHooks {
    fn send_request(
        &mut self,
        mut request: http::Request<wasmtime_wasi_http::WasiBody>,
        options: Option<wasmtime_wasi_http::RequestOptions>,
        fut: Box<dyn std::future::Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
    ) -> Box<
        dyn std::future::Future<
                Output = Result<
                    (
                        http::Response<wasmtime_wasi_http::WasiBody>,
                        Box<
                            dyn std::future::Future<Output = Result<(), wasmtime_wasi_http::Error>>
                                + Send,
                        >,
                    ),
                    wasmtime_wasi_http::Error,
                >,
            > + Send,
    > {
        let host = request
            .uri()
            .host()
            .map(str::to_lowercase)
            .unwrap_or_default();
        if !self.allowed_hosts.contains(&host) {
            drop(fut);
            return Box::new(async move {
                eprintln!("network access denied: {host}");
                Err(wasmtime_wasi_http::Error::HttpRequestDenied)
            });
        }
        self.add_headers(&mut request, &host);
        Box::new(async move {
            use http_body_util::BodyExt;
            let (res, io) = wasmtime_wasi_http::default_send_request(request, options).await?;
            let io: Box<
                dyn std::future::Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send,
            > = Box::new(io);
            Ok((res.map(BodyExt::boxed_unsync), io))
        })
    }
}

struct SandboxState {
    wasi: WasiCtx,
    http_ctx: WasiHttpCtx,
    table: ResourceTable,
    hooks: SandboxHooks,
    limits: StoreLimits,
}

impl WasiView for SandboxState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for SandboxState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http_ctx,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

/// Loads a provider or tool component, with WASI plus `wasi:http` (both the
/// 0.2 and 0.3 versions) linked in and its outbound requests restricted to
/// its own allow-list.
fn load_sandboxed(
    runtime: &Runtime,
    config: &ComponentConfig,
) -> Result<(Component, Linker<SandboxState>, Store<SandboxState>)> {
    let component = runtime.component(&config.path)?;

    let mut linker = Linker::<SandboxState>::new(&runtime.engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;

    Ok((component, linker, sandbox_store(runtime, config)?))
}

/// The host directory `config` mounts as the component's `/`, if any, and
/// with what access. A writable one is created if it doesn't exist yet; a
/// read-only one has to exist already, as an empty one would be no use.
fn mounted_dir(config: &ComponentConfig) -> Result<Option<(String, FsPerms)>> {
    let context = |e: &dyn std::fmt::Display| Error::msg(format!("dir for {}: {e}", config.path));
    let Some(dir) = &config.dir else {
        if config.dir_writable {
            return Err(context(&"dir_writable is set, but there's no dir"));
        }
        return Ok(None);
    };
    let dir = if config.resolved {
        dir.clone()
    } else {
        expand_env(dir).map_err(|e| context(&e))?
    };
    if config.dir_writable {
        std::fs::create_dir_all(&dir).map_err(|e| context(&format!("creating {dir}: {e}")))?;
        return Ok(Some((dir, FsPerms::ReadWrite)));
    }
    if !std::path::Path::new(&dir).is_dir() {
        return Err(context(&format!(
            "{dir} doesn't exist (a read-only dir isn't created; set dir_writable = true \
             if the component should be able to write to it)"
        )));
    }
    Ok(Some((dir, FsPerms::ReadOnly)))
}

/// What a tool's description should say about its `dir`, which the tool
/// can't describe itself: whether it has one and can write to it is host
/// config, not something the tool knows.
fn dir_note(config: &ComponentConfig) -> Option<&'static str> {
    config.dir.as_ref()?;
    Some(if config.dir_writable {
        "Files under / can be read and written, and stay there between calls."
    } else {
        "Files under / can be read, but nothing there can be written, changed or \
         deleted."
    })
}

/// A new store for a provider or tool from `config`: its own WASI context,
/// `wasi:http` allow-list and memory cap.
fn sandbox_store(runtime: &Runtime, config: &ComponentConfig) -> Result<Store<SandboxState>> {
    let mut wasi = WasiCtx::builder();
    if let Some((dir, perms)) = mounted_dir(config)? {
        wasi.preopened_dir(&dir, "/", perms)
            .map_err(|e| Error::msg(format!("dir for {}: opening {dir}: {e}", config.path)))?;
    }

    let mut limits = StoreLimitsBuilder::new();
    if let Some(mb) = config.max_memory_mb {
        limits = limits.memory_size(mb as usize * 1024 * 1024);
    }

    let state = SandboxState {
        wasi: wasi.build(),
        http_ctx: WasiHttpCtx::new(),
        table: ResourceTable::new(),
        hooks: SandboxHooks::new(config)?,
        limits: limits.build(),
    };
    let mut store = Store::new(&runtime.engine, state);
    store.limiter(|s| &mut s.limits);
    // Only a tool call with a `timeout_secs` sets a real deadline.
    runtime.control_store(&mut store);
    Ok(store)
}

struct ToolHandle {
    runtime: Runtime,
    description: ToolDescription,
    config: ComponentConfig,
    // For making a new instance on every call, if `fresh_per_call` is set.
    pre: tool_guest::ToolGuestPre<SandboxState>,
    instance: tool_guest::ToolGuest,
    store: Store<SandboxState>,
}

impl ToolHandle {
    async fn call(&mut self, input: &str) -> Result<String, String> {
        self.call_uncapped(input)
            .await
            .map(cap_tool_output)
            .map_err(cap_tool_output)
    }

    async fn call_uncapped(&mut self, input: &str) -> Result<String, String> {
        if self.config.fresh_per_call {
            let mut store =
                sandbox_store(&self.runtime, &self.config).map_err(|e| e.to_string())?;
            self.instance = self
                .pre
                .instantiate_async(&mut store)
                .await
                .map_err(|e| e.to_string())?;
            self.store = store;
        }

        let Some(timeout) = self.config.timeout_secs.map(Duration::from_secs) else {
            return self
                .instance
                .local_agent_tool_api()
                .call_call(&mut self.store, input)
                .await
                .map_err(|e| e.to_string())?;
        };
        let timed_out = || format!("timed out after {}s", timeout.as_secs());
        // Every store yields at each epoch, even for CPU-bound guest code.
        // Keep that cadence independent of this tool's timeout so shorter
        // run deadlines and cancellation can always be polled by Tokio.
        let call = self
            .instance
            .local_agent_tool_api()
            .call_call(&mut self.store, input);
        match tokio::time::timeout(timeout, call).await {
            Err(_) => Err(timed_out()),
            Ok(result) => result.map_err(|e| e.to_string())?,
        }
    }
}

async fn load_tools(
    runtime: &Runtime,
    configs: &[ComponentConfig],
) -> Result<HashMap<String, ToolHandle>> {
    let mut tools = HashMap::new();

    for config in configs {
        let (component, linker, mut store) = load_sandboxed(runtime, config)?;
        let pre = tool_guest::ToolGuestPre::new(linker.instantiate_pre(&component)?)?;
        let instance = pre.instantiate_async(&mut store).await?;
        let mut description = instance
            .local_agent_tool_api()
            .call_describe(&mut store)
            .await?;
        if let Some(note) = dir_note(config) {
            description.description = format!("{} {note}", description.description);
        }

        tools.insert(
            description.name.clone(),
            ToolHandle {
                runtime: runtime.clone(),
                description,
                config: config.clone(),
                pre,
                instance,
                store,
            },
        );
    }

    Ok(tools)
}

struct ProviderHandle {
    model: String,
    instance: provider_guest::ProviderGuest,
    store: Store<SandboxState>,
}

async fn load_provider(runtime: &Runtime, config: &ProviderConfig) -> Result<ProviderHandle> {
    let (component, linker, mut store) = load_sandboxed(runtime, &config.component)?;
    let instance =
        provider_guest::ProviderGuest::instantiate_async(&mut store, &component, &linker).await?;
    Ok(ProviderHandle {
        model: config.model.clone(),
        instance,
        store,
    })
}

struct SessionHandle {
    /// Which session this run loads and saves, picked by the host.
    id: String,
    instance: session_store_guest::SessionStoreGuest,
    store: Store<SandboxState>,
}

async fn load_session_store(runtime: &Runtime, config: &ComponentConfig) -> Result<SessionHandle> {
    let (component, linker, mut store) = load_sandboxed(runtime, config)?;
    let instance =
        session_store_guest::SessionStoreGuest::instantiate_async(&mut store, &component, &linker)
            .await?;
    Ok(SessionHandle {
        id: String::new(),
        instance,
        store,
    })
}

/// Whether `id` is fine to hand a store as a session name: short, and
/// nothing a store using it as a file name or key could misread.
fn validate_session_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!(
            "invalid session id '{id}': use up to 64 letters, digits, '-', '_' or '.', \
             not starting with '.'"
        );
    }
    Ok(())
}

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    // Shared with the task that runs each reply, which has to keep driving
    // the provider's store while the agent's store reads what it produces.
    provider: Arc<Mutex<ProviderHandle>>,
    tool_definitions: Vec<ToolDefinition>,
    tools: HashMap<String, ToolHandle>,
    session: SessionHandle,
    events: EventSink,
    provider_tasks: Vec<tokio::task::AbortHandle>,
}

fn sort_tool_definitions(definitions: &mut [ToolDefinition]) {
    definitions.sort_unstable_by(|a, b| a.name.cmp(&b.name));
}

impl Drop for HostState {
    fn drop(&mut self) {
        for task in &self.provider_tasks {
            task.abort();
        }
    }
}

impl run_events::Host for HostState {
    fn emit(&mut self, value: run_events::Event) {
        let value = match value {
            run_events::Event::Text(text) => serde_json::json!({"kind":"text", "text":text}),
            run_events::Event::Usage(u) => {
                serde_json::json!({"kind":"usage", "reasoning_tokens":u.reasoning_tokens,
                "max_tokens":u.max_tokens, "prompt_tokens":u.prompt_tokens,
                "cache_read_tokens":u.cache_read_tokens, "cache_creation_tokens":u.cache_creation_tokens})
            }
        };
        (self.events)(value);
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl chat_api::Host for HostState {
    async fn call_tool(&mut self, name: String, input: String) -> Result<String, String> {
        (self.events)(serde_json::json!({"kind":"tool_started", "name":name, "input":input}));
        let result = match self.tools.get_mut(&name) {
            Some(tool) => tool.call(&input).await,
            None => Err(format!("no such tool: {name}")),
        };
        (self.events)(
            serde_json::json!({"kind":"tool_finished", "name":name, "ok":result.is_ok()}),
        );
        result
    }
}

impl session::Host for HostState {
    async fn load(&mut self) -> Result<Option<Vec<Message>>, String> {
        let handle = &mut self.session;
        handle
            .instance
            .local_agent_session_store()
            .call_load(&mut handle.store, &handle.id)
            .await
            .map_err(|trap| format!("session store trapped: {trap}"))?
    }

    async fn save(&mut self, messages: Vec<Message>) -> Result<(), String> {
        let handle = &mut self.session;
        handle
            .instance
            .local_agent_session_store()
            .call_save(&mut handle.store, &handle.id, &messages)
            .await
            .map_err(|trap| format!("session store trapped: {trap}"))??;
        Ok(())
    }
}

impl<U> chat_api::HostWithStore<U> for HasSelf<HostState> {
    /// Streams belong to a single store, and the provider and agent each have
    /// their own, so the host can't just hand the provider's stream to the
    /// agent: it copies the reply across through channels, read on the
    /// provider side by a background task and written on the agent side by
    /// the stream/future returned here.
    async fn chat(accessor: &Accessor<U, Self>, request: ChatRequest) -> ChatStream {
        let (provider, tools) = accessor.with(|mut store| {
            let state = store.get();
            (state.provider.clone(), state.tool_definitions.clone())
        });

        let (text_tx, text_rx) = mpsc::unbounded_channel();
        let (response_tx, response_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let response = run_provider(&provider, tools, request, text_tx).await;
            let _ = response_tx.send(response);
        });

        accessor.with(|mut store| store.get().provider_tasks.push(task.abort_handle()));
        accessor.with(|mut store| ChatStream {
            text: StreamReader::new(&mut store, ChannelProducer(text_rx))
                .expect("concurrency support is enabled"),
            response: FutureReader::new(&mut store, async move {
                Ok::<_, Error>(response_rx.await.unwrap_or_else(|_| {
                    Err(ChatError::Internal("provider task ended early".to_string()))
                }))
            })
            .expect("concurrency support is enabled"),
        })
    }
}

/// Calls the provider's `chat` and forwards its reply: text pieces into
/// `text_tx` as they arrive, then the whole response as the return value.
async fn run_provider(
    provider: &Mutex<ProviderHandle>,
    tools: Vec<ToolDefinition>,
    request: ChatRequest,
    text_tx: mpsc::UnboundedSender<String>,
) -> Result<ChatResponse, ChatError> {
    let mut provider = provider.lock().await;
    let ProviderHandle {
        model,
        instance,
        store,
    } = &mut *provider;
    let (response_tx, response_rx) = oneshot::channel();

    let outcome = store
        .run_concurrent(async |accessor| -> Result<_> {
            let ChatStream { text, response } = instance
                .local_agent_provider_api()
                .call_chat(accessor, model.clone(), tools, request)
                .await?;
            accessor.with(|mut store| -> Result<()> {
                text.pipe(&mut store, ChannelConsumer(text_tx))?;
                response.pipe(&mut store, OneshotConsumer(Some(response_tx)))
            })?;
            // Keeps the provider's store running until its reply is done.
            Ok(response_rx.await)
        })
        .await;

    match outcome {
        Ok(Ok(Ok(response))) => response,
        Ok(Ok(Err(_))) => Err(ChatError::Internal(
            "provider dropped its response".to_string(),
        )),
        Ok(Err(trap)) | Err(trap) => {
            Err(ChatError::Internal(format!("provider trapped: {trap:?}")))
        }
    }
}

/// Feeds a stream returned to a guest from a channel.
struct ChannelProducer<T>(mpsc::UnboundedReceiver<T>);

impl<T: Send + Sync + 'static, D> StreamProducer<D> for ChannelProducer<T> {
    type Item = T;
    type Buffer = Option<T>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'a, D>,
        mut destination: Destination<'a, T, Option<T>>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        // A zero-length read is the guest checking for readiness; say yes
        // rather than taking an item it might never collect.
        if destination.remaining(store) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        match self.get_mut().0.poll_recv(cx) {
            Poll::Ready(Some(item)) => {
                destination.set_buffer(Some(item));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Drains a stream a guest returned into a channel.
struct ChannelConsumer<T>(mpsc::UnboundedSender<T>);

impl<T: Lift + Send + Sync + 'static, D> StreamConsumer<D> for ChannelConsumer<T> {
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        // `read` only fills up to the buffer's capacity.
        let mut items = Vec::with_capacity(source.remaining(&mut store));
        source.read(&mut store, &mut items)?;
        for item in items {
            if self.0.send(item).is_err() {
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// Hands the value of a future a guest returned to a oneshot channel.
struct OneshotConsumer<T>(Option<oneshot::Sender<T>>);

impl<T: Lift + Send + Sync + 'static, D> FutureConsumer<D> for OneshotConsumer<T> {
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        _: bool,
    ) -> Poll<Result<()>> {
        let mut value = None;
        source.read(&mut store, &mut value)?;
        if let (Some(value), Some(tx)) = (value, self.get_mut().0.take()) {
            let _ = tx.send(value);
        }
        Poll::Ready(Ok(()))
    }
}

/// Native service runtime. Compiled code is shared; execution stores are not.
#[derive(Clone)]
pub struct Runtime {
    engine: Engine,
    config: Arc<RunnerConfig>,
    components: Arc<HashMap<String, Component>>,
    cancelled: Arc<AtomicBool>,
    _ticker: Arc<EpochTicker>,
}

struct EpochTicker(Arc<AtomicBool>);
impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub type EventSink = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

impl Runtime {
    pub fn new(path: &str) -> Result<Self> {
        let config = load_config(path)?;
        if config.session_store.is_none() {
            bail!("daemon mode requires a [session_store]");
        }
        Self::from_config(config)
    }

    fn from_config(mut config: RunnerConfig) -> Result<Self> {
        // Resolve credentials once; runs use the daemon's startup configuration.
        for component in std::iter::once(&mut config.provider.component)
            .chain(config.tools.iter_mut())
            .chain(config.session_store.iter_mut())
        {
            for header in component.headers.values_mut() {
                header.value = expand_env(&header.value)?;
            }
            if let Some(dir) = &mut component.dir {
                *dir = expand_env(dir)?;
            }
            component.resolved = true;
            SandboxHooks::new(component)?;
        }
        let mut options = Config::new();
        options
            .wasm_component_model(true)
            .wasm_component_model_async(true)
            .epoch_interruption(true);
        options.cache(Some(Cache::from_file(None)?));
        let engine = Engine::new(&options)?;
        let mut components = HashMap::new();
        let paths = std::iter::once(&config.agent)
            .chain(std::iter::once(&config.provider.component.path))
            .chain(config.tools.iter().map(|tool| &tool.path))
            .chain(config.session_store.iter().map(|store| &store.path));
        for path in paths {
            components.insert(path.clone(), Component::from_file(&engine, path)?);
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let ticker = engine.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(EPOCH_TICK);
                ticker.increment_epoch();
            }
        });
        Ok(Self {
            engine,
            config: Arc::new(config),
            components: Arc::new(components),
            cancelled: Arc::new(AtomicBool::new(false)),
            _ticker: Arc::new(EpochTicker(stopped)),
        })
    }

    pub fn execution(&self) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            ..self.clone()
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn component(&self, path: &str) -> Result<Component> {
        self.components
            .get(path)
            .cloned()
            .ok_or_else(|| Error::msg("component not preloaded"))
    }

    fn control_store<T: Send + 'static>(&self, store: &mut Store<T>) {
        let cancelled = self.cancelled.clone();
        store.epoch_deadline_callback(move |_| {
            if cancelled.load(Ordering::Relaxed) {
                bail!("run cancelled");
            }
            Ok(wasmtime::UpdateDeadline::Yield(1))
        });
        store.set_epoch_deadline(1);
    }

    pub async fn session_ids(&self) -> Result<Vec<String>> {
        let config = self
            .config
            .session_store
            .as_ref()
            .ok_or_else(|| Error::msg("session store required"))?;
        let mut session = load_session_store(self, config).await?;
        session
            .instance
            .local_agent_session_store()
            .call_ids(&mut session.store)
            .await?
            .map_err(Error::msg)
    }

    pub async fn run_turn(&self, session_id: &str, prompt: &str, events: EventSink) -> Result<()> {
        validate_session_id(session_id)?;
        let config = &self.config;
        let provider = load_provider(self, &config.provider).await?;
        let tools = load_tools(self, &config.tools).await?;
        let mut definitions: Vec<_> = tools
            .values()
            .map(|tool| ToolDefinition {
                name: tool.description.name.clone(),
                description: tool.description.description.clone(),
                input_schema_json: tool.description.input_schema_json.clone(),
            })
            .collect();
        sort_tool_definitions(&mut definitions);
        let mut session = load_session_store(
            self,
            config
                .session_store
                .as_ref()
                .ok_or_else(|| Error::msg("session store required"))?,
        )
        .await?;
        session.id = session_id.to_string();
        let mut linker = Linker::<HostState>::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        chat_api::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;
        session::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;
        run_events::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;
        let diagnostics = Diagnostics::new(events.clone());
        // No inherited terminal, environment, filesystem or network for the agent.
        let mut store = Store::new(
            &self.engine,
            HostState {
                wasi: WasiCtx::builder()
                    .stdout(diagnostics.stdout.clone())
                    .stderr(diagnostics.stderr.clone())
                    .build(),
                table: ResourceTable::new(),
                provider: Arc::new(Mutex::new(provider)),
                tools,
                tool_definitions: definitions,
                session,
                events,
                provider_tasks: Vec::new(),
            },
        );
        self.control_store(&mut store);
        let guest =
            ChatGuest::instantiate_async(&mut store, &self.component(&config.agent)?, &linker)
                .await?;
        let prompt = prompt.to_string();
        store
            .run_concurrent(async |accessor| {
                guest
                    .local_agent_agent_api()
                    .call_run_turn(accessor, prompt)
                    .await
            })
            .await??
            .map_err(Error::msg)
    }
}

// Bounded diagnostic capture; dropped after execution stores on every exit path.
struct Diagnostics {
    stdout: wasmtime_wasi::p2::pipe::MemoryOutputPipe,
    stderr: wasmtime_wasi::p2::pipe::MemoryOutputPipe,
    events: EventSink,
}
impl Diagnostics {
    fn new(events: EventSink) -> Self {
        Self {
            stdout: wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(64 * 1024),
            stderr: wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(64 * 1024),
            events,
        }
    }
}
impl Drop for Diagnostics {
    fn drop(&mut self) {
        for (source, pipe) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            let bytes = pipe.contents();
            if !bytes.is_empty() {
                (self.events)(
                    serde_json::json!({"kind":"diagnostic", "source":source, "text":String::from_utf8_lossy(&bytes)}),
                );
            }
        }
    }
}

pub fn check_session_id(id: &str) -> Result<()> {
    validate_session_id(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks(toml: &str) -> Result<SandboxHooks> {
        let config: ComponentConfig = toml::from_str(toml).unwrap();
        SandboxHooks::new(&config)
    }

    /// The headers `hooks` would add to a GET of `url`.
    fn sent(hooks: &SandboxHooks, url: &str) -> Vec<(String, String)> {
        let mut request = http::Request::get(url).body(()).unwrap();
        let host = request.uri().host().unwrap().to_lowercase();
        hooks.add_headers(&mut request, &host);
        request
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string()))
            .collect()
    }

    const CONFIG: &str = r#"
        path = "tool.wasm"
        allowed_hosts = ["api.example.com", "CDN.example.com"]
        headers = { authorization = { value = "Bearer secret", hosts = ["api.example.com"] }, x-client = { value = "runner", hosts = ["api.example.com", "cdn.example.com"] } }
    "#;

    #[test]
    fn header_only_goes_to_its_hosts() {
        let hooks = hooks(CONFIG).unwrap();
        assert_eq!(
            sent(&hooks, "https://api.example.com/search"),
            [
                ("authorization".to_string(), "Bearer secret".to_string()),
                ("x-client".to_string(), "runner".to_string()),
            ]
        );
        assert_eq!(
            sent(&hooks, "https://cdn.example.com/x"),
            [("x-client".to_string(), "runner".to_string())]
        );
    }

    #[test]
    fn no_headers_over_plain_http() {
        let hooks = hooks(CONFIG).unwrap();
        assert!(sent(&hooks, "http://api.example.com/search").is_empty());
    }

    #[test]
    fn allowed_hosts_are_case_insensitive() {
        assert!(
            hooks(CONFIG)
                .unwrap()
                .allowed_hosts
                .contains("cdn.example.com")
        );
    }

    #[test]
    fn scoped_host_must_be_allowed() {
        let err = hooks(
            r#"
            path = "tool.wasm"
            allowed_hosts = ["api.example.com"]
            headers = { authorization = { value = "x", hosts = ["api.exmaple.com"] } }
        "#,
        )
        .err()
        .unwrap();
        assert!(
            err.to_string()
                .contains("'api.exmaple.com' is not in allowed_hosts")
        );
    }

    #[test]
    fn hosts_must_not_be_empty() {
        let err = hooks(
            r#"
            path = "tool.wasm"
            allowed_hosts = ["api.example.com"]
            headers = { authorization = { value = "x", hosts = [] } }
        "#,
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("`hosts` is empty"));
    }

    #[test]
    fn session_ids_are_validated() {
        assert!(validate_session_id("20261017-081500").is_ok());
        assert!(validate_session_id("trip_plans.v2").is_ok());
        for bad in [
            "",
            "../etc",
            ".hidden",
            "a/b",
            "with space",
            &"x".repeat(65),
        ] {
            assert!(validate_session_id(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn tool_definitions_are_sorted_by_name() {
        let definition = |name: &str| ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            input_schema_json: "{}".to_string(),
        };
        let mut definitions = vec![
            definition("weather"),
            definition("files"),
            definition("sports"),
        ];

        sort_tool_definitions(&mut definitions);

        assert_eq!(
            definitions
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            ["files", "sports", "weather"]
        );
    }

    #[test]
    fn dirs_are_read_only_unless_writable() {
        let config = |toml: &str| toml::from_str::<ComponentConfig>(toml).unwrap();

        let none = config(r#"path = "tool.wasm""#);
        assert!(!none.dir_writable);
        assert!(mounted_dir(&none).unwrap().is_none());

        let existing = env::temp_dir().to_string_lossy().into_owned();
        let ro = config(&format!("path = \"python.wasm\"\ndir = {existing:?}"));
        let (dir, perms) = mounted_dir(&ro).unwrap().unwrap();
        assert_eq!(dir, existing);
        assert!(perms.write_not_permitted());

        // A read-only dir isn't created, so a missing one is an error...
        let missing = env::temp_dir().join(format!("runner-test-{}", std::process::id()));
        let missing = missing.to_string_lossy().into_owned();
        let ro_missing = config(&format!("path = \"python.wasm\"\ndir = {missing:?}"));
        let err = mounted_dir(&ro_missing).unwrap_err().to_string();
        assert!(err.contains("doesn't exist"), "{err}");

        // ...while a writable one is created.
        let rw = config(&format!(
            "path = \"store.wasm\"\ndir = {missing:?}\ndir_writable = true"
        ));
        let (_, perms) = mounted_dir(&rw).unwrap().unwrap();
        assert!(!perms.write_not_permitted());
        assert!(std::path::Path::new(&missing).is_dir());
        std::fs::remove_dir(&missing).unwrap();

        let no_dir = config("path = \"tool.wasm\"\ndir_writable = true");
        assert!(mounted_dir(&no_dir).is_err());
    }

    #[test]
    fn dir_note_matches_access() {
        let config = |toml: &str| toml::from_str::<ComponentConfig>(toml).unwrap();
        assert_eq!(dir_note(&config(r#"path = "tool.wasm""#)), None);

        let ro = dir_note(&config("path = \"python.wasm\"\ndir = \".\"")).unwrap();
        assert!(ro.contains("nothing there can be written"), "{ro}");

        let rw = dir_note(&config(
            "path = \"python.wasm\"\ndir = \".\"\ndir_writable = true",
        ))
        .unwrap();
        assert!(rw.contains("read and written"), "{rw}");
    }

    #[test]
    fn tool_output_is_capped() {
        let short = "ok".to_string();
        assert_eq!(cap_tool_output(short.clone()), short);

        let exact = "x".repeat(MAX_TOOL_OUTPUT);
        assert_eq!(cap_tool_output(exact.clone()), exact);

        let capped = cap_tool_output("x".repeat(MAX_TOOL_OUTPUT + 10));
        assert!(capped.starts_with(&"x".repeat(MAX_TOOL_OUTPUT)));
        assert!(capped.ends_with("[cut by host: 10 more bytes]"));

        // Never cuts a multi-byte character in half.
        let capped = cap_tool_output(format!("{}é", "x".repeat(MAX_TOOL_OUTPUT - 1)));
        assert!(capped.starts_with(&"x".repeat(MAX_TOOL_OUTPUT - 1)));
        assert!(capped.ends_with("[cut by host: 2 more bytes]"));
    }

    #[test]
    fn sandbox_limits_parse() {
        let config = toml::from_str::<ComponentConfig>(
            r#"
            path = "python.wasm"
            timeout_secs = 10
            max_memory_mb = 256
            fresh_per_call = true
        "#,
        )
        .unwrap();
        assert_eq!(config.timeout_secs, Some(10));
        assert_eq!(config.max_memory_mb, Some(256));
        assert!(config.fresh_per_call);

        let config = toml::from_str::<ComponentConfig>(r#"path = "tool.wasm""#).unwrap();
        assert_eq!(config.timeout_secs, None);
        assert_eq!(config.max_memory_mb, None);
        assert!(!config.fresh_per_call);
    }

    #[test]
    fn plain_string_header_is_rejected() {
        let config = toml::from_str::<ComponentConfig>(
            r#"
            path = "tool.wasm"
            allowed_hosts = ["api.example.com"]
            headers = { authorization = "Bearer x" }
        "#,
        );
        assert!(config.is_err());
    }
}
