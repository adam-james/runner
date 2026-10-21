mod activity;
mod transcript;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;
use tracing::{Instrument, instrument::WithSubscriber};

use tokio::sync::{Mutex, mpsc, oneshot};
use wasmtime::component::{
    Component, FutureConsumer, Lift, Linker, ResourceTable, Source, StreamConsumer, StreamResult,
};
use wasmtime::{
    Cache, Config, Engine, Error, Result, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder,
    bail,
};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

// Scoped in their own modules so each world's generated types (all under
// the `local:agent` package, since wit_bindgen requires one package per
// directory) don't collide.
mod tool_guest {
    wasmtime::component::bindgen!({
        world: "tool-guest",
        path: "../wit",
        exports: { default: async },
    });
}
use tool_guest::exports::local::agent::tool_api::ToolDescription;

// Its bindings own the conversation types the turn loop builds requests from.
mod provider_guest {
    wasmtime::component::bindgen!({
        world: "provider-guest",
        path: "../wit",
    });
}
use provider_guest::exports::local::agent::provider_api::ToolDefinition;
use provider_guest::local::agent::messages::{ContentBlock, Message, Role, ToolResultBlock};
use provider_guest::local::agent::types::{ChatError, ChatRequest, ChatResponse, ChatStream};

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerConfig {
    /// Model providers by name; agent definitions pick one with `provider`.
    #[serde(default)]
    providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    tools: BTreeMap<String, ComponentConfig>,
    /// Named agent definitions, each picking from the providers and tools.
    /// Must include `default`.
    #[serde(default)]
    agents: BTreeMap<String, AgentConfig>,
}

/// One `[agents.<name>]` table.
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentConfig {
    /// A `[providers.<name>]` key.
    provider: String,
    /// Overrides the provider's `model`.
    model: Option<String>,
    /// Tools by their configured table keys. Omitted means none.
    #[serde(default)]
    tools: Vec<String>,
    /// The system prompt sent with each model request; without one,
    /// `DEFAULT_SYSTEM_PROMPT`.
    system_prompt: Option<String>,
    /// The most tokens one model reply may use, hidden reasoning included;
    /// `DEFAULT_MAX_TOKENS` if omitted.
    max_tokens: Option<u32>,
    /// The most model requests one turn may make before it's given up.
    /// Omitted means no limit beyond the run's deadline.
    max_model_calls: Option<u32>,
}

/// A definition resolved against the top-level config.
struct AgentDefinition {
    /// The `[providers.<name>]` key `provider` came from.
    provider_name: String,
    provider: ProviderConfig,
    tools: BTreeMap<String, ComponentConfig>,
    system_prompt: Option<String>,
    max_tokens: u32,
    max_model_calls: Option<u32>,
}

impl AgentDefinition {
    /// Always set: `resolve_agents` rejects definitions without a model.
    fn model(&self) -> &str {
        self.provider.model.as_deref().unwrap_or_default()
    }
}

/// The name of the definition used when a run doesn't pick one.
pub const DEFAULT_AGENT: &str = "default";

/// What an agent definition runs with, for clients to show.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub tools: Vec<String>,
    pub max_tokens: u32,
    pub max_model_calls: Option<u32>,
}

/// Tool and provider table keys: what agent definitions refer to them by.
/// Tool names are also what the model calls them.
fn validate_table_key(kind: &str, name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        bail!("invalid {kind} name '{name}': use 1–64 letters, digits, '-' or '_'");
    }
    Ok(())
}

/// Resolve agents' provider and tool selections using the names declared in the config.
fn resolve_agents(config: &RunnerConfig) -> Result<BTreeMap<String, AgentDefinition>> {
    for name in config.tools.keys() {
        validate_table_key("tool", name)?;
    }
    for name in config.providers.keys() {
        validate_table_key("provider", name)?;
    }
    if !config.agents.contains_key(DEFAULT_AGENT) {
        bail!("no [agents.{DEFAULT_AGENT}]: runs that don't name an agent use it");
    }
    let mut agents = BTreeMap::new();
    for (name, agent) in &config.agents {
        validate_name("agent name", name)?;
        if agent.max_tokens == Some(0) || agent.max_model_calls == Some(0) {
            bail!("agent '{name}': `max_tokens` and `max_model_calls` must be at least 1");
        }
        if agent
            .system_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.trim().is_empty())
        {
            bail!("agent '{name}': `system_prompt` is empty; leave it out to use the default");
        }
        let mut provider = config
            .providers
            .get(&agent.provider)
            .ok_or_else(|| {
                Error::msg(format!(
                    "agent '{name}': no provider named '{}'",
                    agent.provider
                ))
            })?
            .clone();
        if let Some(model) = &agent.model {
            provider.model = Some(model.clone());
        }
        if provider.model.is_none() {
            bail!(
                "agent '{name}': no model; set `model` on it or on [providers.{}]",
                agent.provider
            );
        }
        let tools = agent
            .tools
            .iter()
            .map(|tool| match config.tools.get(tool) {
                Some(component) => Ok((tool.clone(), component.clone())),
                None => Err(Error::msg(format!(
                    "agent '{name}': no tool named '{tool}'"
                ))),
            })
            .collect::<Result<_>>()?;
        agents.insert(
            name.clone(),
            AgentDefinition {
                provider_name: agent.provider.clone(),
                provider,
                tools,
                system_prompt: agent.system_prompt.clone(),
                max_tokens: agent.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
                max_model_calls: agent.max_model_calls,
            },
        );
    }
    Ok(agents)
}

#[derive(Clone, serde::Deserialize)]
struct ProviderConfig {
    /// The model agents using this provider get, unless they set their own.
    model: Option<String>,
    #[serde(flatten)]
    component: ComponentConfig,
}

/// A sandboxed component with network access: a provider or a tool.
#[derive(Clone, serde::Deserialize)]
struct ComponentConfig {
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
    let mut config: RunnerConfig =
        toml::from_str(&text).map_err(|e| Error::msg(format!("parsing config {path}: {e}")))?;
    // Resolve credentials once; runs use the daemon's startup configuration.
    for component in config
        .providers
        .values_mut()
        .map(|provider| &mut provider.component)
        .chain(config.tools.values_mut())
    {
        for (name, header) in &mut component.headers {
            header.value = expand_env(&header.value)
                .map_err(|e| Error::msg(format!("header '{name}' for {}: {e}", component.path)))?;
        }
        if let Some(dir) = &mut component.dir {
            *dir = expand_env(dir)
                .map_err(|e| Error::msg(format!("dir for {}: {e}", component.path)))?;
        }
    }
    Ok(config)
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
    component: String,
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
            let mut value =
                http::HeaderValue::try_from(header.value.as_str()).map_err(|e| context(&e))?;
            value.set_sensitive(true);
            headers.push(InjectedHeader { name, value, hosts });
        }

        Ok(Self {
            component: config.path.clone(),
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
                tracing::warn!(component = ?self.component, host = ?host, "credential injection skipped for plain HTTP");
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
        let allowed = self.allowed_hosts.contains(&host);
        let span = tracing::info_span!("outbound_http", request_id = activity::next_id(),
            component = ?self.component, method = ?request.method(),
            scheme = ?request.uri().scheme_str().unwrap_or(""), host = ?host,
            port = request.uri().port_u16());
        let guard = activity::Activity::new(span.clone(), "http");
        tracing::info!(parent: &span, allowed, "network policy");
        if !allowed {
            drop(fut);
            return Box::new(
                async move {
                    guard.finish("denied", 0, None);
                    Err(wasmtime_wasi_http::Error::HttpRequestDenied)
                }
                .instrument(span)
                .with_current_subscriber(),
            );
        }
        self.add_headers(&mut request, &host);
        let io_span = span.clone();
        Box::new(
            async move {
                use http_body_util::BodyExt;
                let (res, io) =
                    match wasmtime_wasi_http::default_send_request(request, options).await {
                        Ok(response) => response,
                        Err(error) => {
                            guard.finish("transport_error", 0, None);
                            return Err(error);
                        }
                    };
                guard.finish("headers_received", 0, Some(res.status().as_u16()));
                let io: Box<
                    dyn std::future::Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send,
                > = Box::new(io.instrument(io_span).with_current_subscriber());
                Ok((res.map(BodyExt::boxed_unsync), io))
            }
            .instrument(span)
            .with_current_subscriber(),
        )
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

    Ok((component, linker, sandbox_store(&runtime.engine, config)?))
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
    let dir = dir.clone();
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
fn sandbox_store(engine: &Engine, config: &ComponentConfig) -> Result<Store<SandboxState>> {
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
    let mut store = Store::new(engine, state);
    store.limiter(|s| &mut s.limits);
    // Only a tool call with a `timeout_secs` sets a real deadline.
    Runtime::control_store(&mut store);
    Ok(store)
}

struct ToolHandle {
    engine: Engine,
    healthy: bool,
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

    #[tracing::instrument(name = "component", skip_all, fields(component = ?self.config.path, kind = "tool"))]
    async fn call_uncapped(&mut self, input: &str) -> Result<String, String> {
        if self.config.fresh_per_call || !self.healthy {
            let mut store = sandbox_store(&self.engine, &self.config).map_err(|e| e.to_string())?;
            self.instance = self
                .pre
                .instantiate_async(&mut store)
                .await
                .map_err(|e| e.to_string())?;
            self.store = store;
        }

        self.healthy = false;
        let Some(timeout) = self.config.timeout_secs.map(Duration::from_secs) else {
            let result = self
                .instance
                .local_agent_tool_api()
                .call_call(&mut self.store, input)
                .await;
            self.healthy = result.is_ok();
            return result.map_err(|e| e.to_string())?;
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
            Ok(result) => {
                self.healthy = result.is_ok();
                result.map_err(|e| e.to_string())?
            }
        }
    }
}

async fn load_tool(runtime: &Runtime, config: &ComponentConfig) -> Result<ToolHandle> {
    let (component, linker, mut store) = load_sandboxed(runtime, config)?;
    let pre = tool_guest::ToolGuestPre::new(linker.instantiate_pre(&component)?)?;
    let instance = pre.instantiate_async(&mut store).await?;
    let mut description = instance
        .local_agent_tool_api()
        .call_describe(&mut store)
        .await
        .map_err(|e| e.context(format!("describing tool {}", config.path)))?;
    if let Some(note) = dir_note(config) {
        description.description = format!("{} {note}", description.description);
    }
    Ok(ToolHandle {
        engine: runtime.engine.clone(),
        healthy: true,
        description,
        config: config.clone(),
        pre,
        instance,
        store,
    })
}

async fn load_tools(
    runtime: &Runtime,
    configs: &BTreeMap<String, ComponentConfig>,
) -> Result<HashMap<String, ToolHandle>> {
    let mut tools = HashMap::new();
    for (name, config) in configs {
        let tool = load_tool(runtime, config).await?;
        tools.insert(name.clone(), tool);
    }
    Ok(tools)
}

struct ProviderHandle {
    engine: Engine,
    config: ComponentConfig,
    pre: provider_guest::ProviderGuestPre<SandboxState>,
    healthy: bool,
    model: String,
    instance: provider_guest::ProviderGuest,
    store: Store<SandboxState>,
}

async fn load_provider(runtime: &Runtime, definition: &AgentDefinition) -> Result<ProviderHandle> {
    let config = &definition.provider;
    let (component, linker, mut store) = load_sandboxed(runtime, &config.component)?;
    let pre = provider_guest::ProviderGuestPre::new(linker.instantiate_pre(&component)?)?;
    let instance = pre.instantiate_async(&mut store).await?;
    Ok(ProviderHandle {
        engine: runtime.engine.clone(),
        config: config.component.clone(),
        pre,
        healthy: true,
        model: definition.model().to_string(),
        instance,
        store,
    })
}

/// Whether `id` is fine as a session name: short, and nothing a store using
/// it as a file name or key could misread.
fn validate_session_id(id: &str) -> Result<()> {
    validate_name("session id", id)
}

/// Session ids' rules, for any name a client sends.
fn validate_name(kind: &str, id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!(
            "invalid {kind} '{id}': use up to 64 letters, digits, '-', '_' or '.', \
             not starting with '.'"
        );
    }
    Ok(())
}

/// A session's retained provider and tools, and the turn loop that drives them.
struct SessionComponents {
    provider: ProviderHandle,
    tool_definitions: Vec<ToolDefinition>,
    tools: HashMap<String, ToolHandle>,
}

fn sort_tool_definitions(definitions: &mut [ToolDefinition]) {
    definitions.sort_unstable_by(|a, b| a.name.cmp(&b.name));
}

/// The system prompt for definitions that don't set `system_prompt`.
const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
/// `max_tokens` for definitions that don't set one. It's per request, and it
/// covers a reasoning model's hidden reasoning as well as the reply, so it
/// needs headroom. Only tokens actually used are billed.
pub const DEFAULT_MAX_TOKENS: u32 = 16384;

impl SessionComponents {
    /// Runs one turn to completion on `history`: calls the model and the
    /// tools it asks for until it finishes, and returns the conversation with
    /// the turn added. An error is why the turn was given up, written for the
    /// user.
    async fn turn(
        &mut self,
        definition: &AgentDefinition,
        mut history: Vec<Message>,
        prompt: String,
        events: &EventSink,
    ) -> Result<Vec<Message>, String> {
        let system = definition
            .system_prompt
            .as_deref()
            .unwrap_or(DEFAULT_SYSTEM_PROMPT);
        let max_tokens = definition.max_tokens;
        history.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text(prompt)],
        });
        for calls in 0.. {
            if definition.max_model_calls.is_some_and(|max| calls >= max) {
                return Err(format!(
                    "stopped after {calls} model {} without a final reply, so this turn \
                     wasn't saved",
                    if calls == 1 { "call" } else { "calls" }
                ));
            }
            let request = ChatRequest {
                system: Some(system.to_string()),
                max_tokens,
                messages: history.clone(),
            };
            let response = self.chat(request, events).await.map_err(|e| describe(&e))?;
            events(serde_json::json!({"kind":"usage",
                "reasoning_tokens":response.reasoning_tokens, "max_tokens":max_tokens,
                "prompt_tokens":response.prompt_tokens,
                "cache_read_tokens":response.cache_read_tokens,
                "cache_creation_tokens":response.cache_creation_tokens}));
            // A cut-off reply may end mid-sentence or mid-tool-call, and keeping
            // it would only confuse the next turn. OpenAI reports "length",
            // Anthropic "max_tokens".
            if matches!(
                response.stop_reason.as_deref(),
                Some("length" | "max_tokens")
            ) {
                return Err(format!(
                    "reply cut off: hit the {max_tokens} token limit, so this turn wasn't saved"
                ));
            }
            let mut tool_results = Vec::new();
            for block in &response.content {
                if let ContentBlock::ToolUse(tool_use) = block {
                    let result = self
                        .call_tool(&tool_use.name, &tool_use.input_json, events)
                        .await;
                    tool_results.push(ContentBlock::ToolResult(ToolResultBlock {
                        tool_use_id: tool_use.id.clone(),
                        content: result.unwrap_or_else(|e| format!("error: {e}")),
                    }));
                }
            }
            history.push(Message {
                role: Role::Assistant,
                content: response.content,
            });
            if tool_results.is_empty() {
                break;
            }
            history.push(Message {
                role: Role::User,
                content: tool_results,
            });
        }
        Ok(history)
    }

    /// One model request, its text emitted as `text` events as it streams in.
    async fn chat(
        &mut self,
        request: ChatRequest,
        events: &EventSink,
    ) -> Result<ChatResponse, ChatError> {
        let (text_tx, mut text_rx) = mpsc::unbounded_channel();
        let reply = run_provider(
            &mut self.provider,
            self.tool_definitions.clone(),
            request,
            text_tx,
        );
        tokio::pin!(reply);
        let response = loop {
            tokio::select! {
                // Text that has arrived goes out before the reply it belongs to.
                biased;
                Some(text) = text_rx.recv() => events(serde_json::json!({"kind":"text", "text":text})),
                response = &mut reply => break response,
            }
        };
        while let Ok(text) = text_rx.try_recv() {
            events(serde_json::json!({"kind":"text", "text":text}));
        }
        response
    }

    async fn call_tool(
        &mut self,
        name: &str,
        input: &str,
        events: &EventSink,
    ) -> Result<String, String> {
        events(serde_json::json!({"kind":"tool_started", "name":name, "input":input}));
        let start = std::time::Instant::now();
        let span = tracing::info_span!("tool", call_id = activity::next_id(),
            tool = ?activity::bounded_name(name),
            component = ?self.tools.get(name).map(|tool| &tool.config.path));
        let guard = activity::Activity::new(span.clone(), "tool");
        let result = async {
            match self.tools.get_mut(name) {
                Some(tool) => tool.call(input).await,
                None => Err(format!("no such tool: {name}")),
            }
        }
        .instrument(span)
        .await;
        guard.finish(
            if result.is_ok() { "ok" } else { "error" },
            result.as_ref().map_or(0, String::len),
            None,
        );
        // The error is what the model sees as the tool result, so it is no more private than the session.
        events(
            serde_json::json!({"kind":"tool_finished", "name":name, "ok":result.is_ok(),
            "elapsed_ms":start.elapsed().as_millis() as u64,
            "bytes":result.as_ref().map_or(0, String::len), "error":result.as_ref().err()}),
        );
        result
    }
}

/// A model error as the user sees it.
fn describe(error: &ChatError) -> String {
    match error {
        ChatError::InvalidRequest(m) => format!("invalid request: {m}"),
        ChatError::Network(m) => format!("network error: {m}"),
        ChatError::Api(detail) => format!(
            "API error {}: {}",
            detail.status,
            api_error_message(&detail.body).unwrap_or_else(|| detail.body.trim().to_string())
        ),
        ChatError::Internal(m) => format!("internal error: {m}"),
    }
}

/// The message out of an API error body shaped like
/// `{"error": {"message": ...}}`, which both Anthropic and OpenAI use.
fn api_error_message(body: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(body["error"]["message"].as_str()?.to_string())
}

/// Calls the provider's `chat` and forwards its reply: text pieces into
/// `text_tx` as they arrive, then the whole response as the return value.
#[tracing::instrument(name = "provider", skip_all, fields(component = tracing::field::Empty))]
async fn run_provider(
    provider: &mut ProviderHandle,
    tools: Vec<ToolDefinition>,
    request: ChatRequest,
    text_tx: mpsc::UnboundedSender<String>,
) -> Result<ChatResponse, ChatError> {
    tracing::Span::current().record("component", tracing::field::debug(&provider.config.path));
    if !provider.healthy {
        let mut store = sandbox_store(&provider.engine, &provider.config)
            .map_err(|e| ChatError::Internal(e.to_string()))?;
        let instance = provider
            .pre
            .instantiate_async(&mut store)
            .await
            .map_err(|e| ChatError::Internal(e.to_string()))?;
        provider.store = store;
        provider.instance = instance;
    }
    let guard = activity::Activity::new(tracing::Span::current(), "provider");
    provider.healthy = false;
    let ProviderHandle {
        model,
        instance,
        store,
        healthy,
        ..
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

    let result = match outcome {
        Ok(Ok(Ok(response))) => {
            *healthy = true;
            response
        }
        Ok(Ok(Err(_))) => Err(ChatError::Internal(
            "provider dropped its response".to_string(),
        )),
        Ok(Err(trap)) | Err(trap) => {
            Err(ChatError::Internal(format!("provider trapped: {trap:?}")))
        }
    };
    guard.finish(if result.is_ok() { "ok" } else { "error" }, 0, None);
    result
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

/// Native service runtime. Compiled code is shared globally; component stores
/// are retained per session and serialized across turns, including runtime clones.
#[derive(Clone)]
pub struct Runtime {
    engine: Engine,
    config: Arc<RunnerConfig>,
    agents: Arc<BTreeMap<String, AgentDefinition>>,
    components: Arc<HashMap<String, Component>>,
    _ticker: Arc<EpochTicker>,
    sessions: Arc<std::sync::Mutex<HashMap<String, SessionSlot>>>,
}

/// A session's retained components, and the agent definition they were built for.
type SessionSlot = Arc<Mutex<Option<(String, SessionComponents)>>>;

struct EpochTicker(Arc<AtomicBool>);

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub type EventSink = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

impl Runtime {
    /// Loads every component and resolves named tools directly from the
    /// config.
    pub async fn new(path: &str) -> Result<Self> {
        Self::from_config(load_config(path)?).await
    }

    async fn from_config(config: RunnerConfig) -> Result<Self> {
        let agents = resolve_agents(&config)?;
        for component in sandboxed_components(&config) {
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
        for path in sandboxed_components(&config).map(|component| &component.path) {
            if components.contains_key(path) {
                continue;
            }
            let component = Component::from_file(&engine, path)
                .map_err(|e| e.context(format!("loading component {path}")))?;
            components.insert(path.clone(), component);
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
        let runtime = Self {
            engine,
            config: Arc::new(config),
            agents: Arc::new(agents),
            components: Arc::new(components),
            _ticker: Arc::new(EpochTicker(stopped)),
            sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };
        Ok(runtime)
    }

    /// The agent definitions runs can use, sorted by name.
    pub fn agents(&self) -> Vec<AgentInfo> {
        self.agents
            .iter()
            .map(|(name, definition)| AgentInfo {
                name: name.clone(),
                provider: definition.provider_name.clone(),
                model: definition.model().to_string(),
                tools: definition.tools.keys().cloned().collect(),
                max_tokens: definition.max_tokens,
                max_model_calls: definition.max_model_calls,
            })
            .collect()
    }

    fn component(&self, path: &str) -> Result<Component> {
        self.components
            .get(path)
            .cloned()
            .ok_or_else(|| Error::msg(format!("component {path} not preloaded")))
    }

    /// Yielding at each epoch lets Tokio poll timeouts and drop a cancelled
    /// run's future even while guest code is CPU-bound.
    fn control_store<T: Send + 'static>(store: &mut Store<T>) {
        store.epoch_deadline_callback(|_| Ok(wasmtime::UpdateDeadline::Yield(1)));
        store.set_epoch_deadline(1);
    }

    /// Runs one turn of `session_id` with `agent`, continuing the
    /// conversation in `transcript` (from an earlier turn's return value;
    /// `None` for a new session). Returns the transcript with this turn
    /// added, for the caller to keep: nothing is saved here, so a turn that
    /// fails, is cancelled or never returns leaves the conversation as it was.
    #[tracing::instrument(name = "turn", skip_all, fields(agent = ?agent, session = ?session_id))]
    pub async fn run_turn(
        &self,
        agent: &str,
        session_id: &str,
        transcript: Option<&str>,
        prompt: &str,
        events: EventSink,
    ) -> Result<String> {
        validate_session_id(session_id)?;
        let definition = self
            .agents
            .get(agent)
            .ok_or_else(|| Error::msg(format!("unknown agent '{agent}'")))?;
        let history = match transcript {
            None => Vec::new(),
            Some(text) => transcript::decode(text).map_err(|error| {
                tracing::warn!(%error, "could not decode transcript");
                Error::new(TurnFailed(
                    "could not load this session's history".to_string(),
                ))
            })?,
        };
        let session = self
            .sessions
            .lock()
            .unwrap()
            .entry(session_id.to_owned())
            .or_default()
            .clone();
        let mut cached = session.lock().await;
        // Taking the components out makes cancellation/panics discard interrupted
        // stores automatically. A normally returned turn puts them back, even if
        // the agent reports an application error (e.g. a rejected API request).
        // A turn with a different definition starts over with that definition's
        // own instances; it never inherits another agent's provider or tool state.
        let mut components = match cached.take() {
            Some((cached_agent, components)) if cached_agent == agent => components,
            _ => self.load_session(definition).await?,
        };
        let result = components
            .turn(definition, history, prompt.to_string(), &events)
            .await;
        *cached = Some((agent.to_string(), components));
        result
            .map(|history| transcript::encode(&history))
            .map_err(|message| Error::new(TurnFailed(redact(&self.config, &message))))
    }

    async fn load_session(&self, definition: &AgentDefinition) -> Result<SessionComponents> {
        let provider = load_provider(self, definition).await?;
        let tools = load_tools(self, &definition.tools).await?;
        let mut definitions: Vec<_> = tools
            .iter()
            .map(|(name, tool)| ToolDefinition {
                name: name.clone(),
                description: tool.description.description.clone(),
                input_schema_json: tool.description.input_schema_json.clone(),
            })
            .collect();
        sort_tool_definitions(&mut definitions);
        Ok(SessionComponents {
            provider,
            tools,
            tool_definitions: definitions,
        })
    }
}

/// Every sandboxed component `config` declares: providers and tools.
fn sandboxed_components(config: &RunnerConfig) -> impl Iterator<Item = &ComponentConfig> {
    config
        .providers
        .values()
        .map(|provider| &provider.component)
        .chain(config.tools.values())
}

/// Strips configured credentials out of a message headed for run storage
/// and the user, in case a server reflected one back in an error.
fn redact(config: &RunnerConfig, message: &str) -> String {
    let mut secrets: Vec<&str> = sandboxed_components(config)
        .flat_map(|component| component.headers.values())
        // The whole value, and its parts, e.g. the token in "Bearer <token>".
        .flat_map(|header| {
            std::iter::once(header.value.as_str()).chain(header.value.split_whitespace())
        })
        .filter(|secret| secret.len() >= 8)
        .collect();
    // Longest first, so a value is redacted whole before any part of it.
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    let mut message = message.to_string();
    for secret in secrets {
        message = message.replace(secret, "[redacted]");
    }
    if message.chars().count() > MAX_ERROR_CHARS {
        message = message
            .chars()
            .take(MAX_ERROR_CHARS - 1)
            .collect::<String>()
            + "…";
    }
    message
}

/// Longest turn error kept; API error bodies can be whole HTML pages.
const MAX_ERROR_CHARS: usize = 500;

/// A turn the agent gave up on, with a message written for the user (e.g.
/// "API error 401: invalid x-api-key"). Credentials are already redacted.
/// Any other `run_turn` error is internal and may contain secrets.
#[derive(Debug)]
pub struct TurnFailed(pub String);

impl std::fmt::Display for TurnFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TurnFailed {}

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

    #[tokio::test]
    async fn outbound_http_logs_policy_headers_errors_and_interruption_without_payloads() {
        use http_body_util::BodyExt;
        use std::io::{Read, Write};
        let capture = activity::tests::Capture::default();
        let _subscriber = tracing::subscriber::set_default(capture.subscriber());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut bytes = [0; 4096];
            let count = socket.read(&mut bytes).unwrap();
            assert!(count > 0);
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let mut hooks = hooks(
            r#"path = "test-component.wasm"
            allowed_hosts = ["127.0.0.1"]"#,
        )
        .unwrap();
        let request = |url: String| {
            http::Request::get(url)
                .header("authorization", "HEADER_SECRET")
                .body(
                    http_body_util::Empty::new()
                        .map_err(|never| match never {})
                        .boxed_unsync(),
                )
                .unwrap()
        };
        let call = |hooks: &mut SandboxHooks, url| {
            hooks.send_request(request(url), None, Box::new(async { Ok(()) }))
        };
        let (response, io) = Box::into_pin(call(
            &mut hooks,
            format!("http://127.0.0.1:{port}/PATH_SECRET?QUERY_SECRET"),
        ))
        .await
        .unwrap();
        assert_eq!(response.status(), 204);
        drop(io);
        server.join().unwrap();
        assert!(
            Box::into_pin(call(
                &mut hooks,
                "http://denied.invalid/PATH_SECRET?QUERY_SECRET".into()
            ))
            .await
            .is_err()
        );
        // A reserved then closed local port gives a deterministic transport failure.
        assert!(
            Box::into_pin(call(
                &mut hooks,
                format!("http://127.0.0.1:{port}/ERROR_SECRET")
            ))
            .await
            .is_err()
        );
        // Even a request dropped before its first poll records interruption.
        drop(call(
            &mut hooks,
            format!("http://127.0.0.1:{port}/DROP_SECRET"),
        ));
        let logs = capture.text();
        for expected in [
            "allowed=true",
            "allowed=false",
            "headers_received",
            "denied",
            "transport_error",
            "interrupted",
            "status=204",
            "test-component.wasm",
        ] {
            assert!(logs.contains(expected), "{expected}: {logs}");
        }
        for secret in [
            "PATH_SECRET",
            "QUERY_SECRET",
            "HEADER_SECRET",
            "ERROR_SECRET",
            "DROP_SECRET",
        ] {
            assert!(!logs.contains(secret), "{logs}");
        }
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

    #[test]
    fn turn_errors_redact_configured_credentials() {
        let config: RunnerConfig = toml::from_str(
            r#"
            [providers.main]
            path = "provider.wasm"
            model = "m"
            allowed_hosts = ["api.example.com"]
            headers = { authorization = { value = "Bearer sk-live-12345", hosts = ["api.example.com"] } }
            [tools.tool]
            path = "tool.wasm"
            allowed_hosts = ["tool.example.com"]
            headers = { x-key = { value = "tool-key-abcdef", hosts = ["tool.example.com"] }, x-client = { value = "runner", hosts = ["tool.example.com"] } }
            "#,
        )
        .unwrap();
        assert_eq!(
            redact(&config, "API error 401: bad header 'Bearer sk-live-12345'"),
            "API error 401: bad header '[redacted]'"
        );
        assert_eq!(
            redact(&config, "token sk-live-12345 and tool-key-abcdef rejected"),
            "token [redacted] and [redacted] rejected"
        );
        // Short values like "runner" would mangle ordinary words.
        assert_eq!(redact(&config, "runner failed"), "runner failed");
        let long = redact(&config, &"x".repeat(2000));
        assert_eq!(long.chars().count(), MAX_ERROR_CHARS);
        assert!(long.ends_with('…'));
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

    #[tokio::test]
    async fn component_load_errors_name_the_file() {
        let load = async |tool: &str| {
            let config: RunnerConfig = toml::from_str(&format!(
                "[providers.p]\npath = {tool:?}\nmodel = \"m\"\n\
                 [agents.default]\nprovider = \"p\""
            ))
            .unwrap();
            format!("{:#}", Runtime::from_config(config).await.err().unwrap())
        };

        let missing = env::temp_dir().join(format!("runner-missing-{}.wasm", std::process::id()));
        let missing = missing.to_string_lossy().into_owned();
        let err = load(&missing).await;
        assert!(
            err.contains(&format!("loading component {missing}")),
            "{err}"
        );

        // Wasmtime's own error for a corrupt binary doesn't mention the file.
        let corrupt = env::temp_dir().join(format!("runner-corrupt-{}.wasm", std::process::id()));
        std::fs::write(&corrupt, b"\0asm\x0d\0\x01\0garbage").unwrap();
        let corrupt_path = corrupt.to_string_lossy().into_owned();
        let err = load(&corrupt_path).await;
        std::fs::remove_file(&corrupt).unwrap();
        assert!(
            err.contains(&format!("loading component {corrupt_path}")),
            "{err}"
        );
    }

    #[test]
    fn load_config_expands_env_vars_once() {
        let home = env::var("HOME").unwrap();
        let path = env::temp_dir().join(format!("runner-config-{}.toml", std::process::id()));
        let write = |key: &str| {
            std::fs::write(
                &path,
                format!(
                    r#"
                    [providers.main]
                    path = "provider.wasm"
                    model = "m"
                    allowed_hosts = ["api.example.com"]
                    headers = {{ x-api-key = {{ value = "k-${{{key}}}", hosts = ["api.example.com"] }} }}
                    [tools.tool]
                    path = "tool.wasm"
                    dir = "${{HOME}}/data"
                    "#
                ),
            )
            .unwrap();
        };

        write("HOME");
        let config = load_config(path.to_str().unwrap()).unwrap();
        assert_eq!(
            config.providers["main"].component.headers["x-api-key"].value,
            format!("k-{home}")
        );
        assert_eq!(
            config.tools["tool"].dir.as_deref(),
            Some(&*format!("{home}/data"))
        );

        write("RUNNER_TEST_UNSET_VAR");
        let err = load_config(path.to_str().unwrap())
            .err()
            .unwrap()
            .to_string();
        std::fs::remove_file(&path).unwrap();
        assert!(
            err.contains("header 'x-api-key' for provider.wasm"),
            "{err}"
        );
        assert!(err.contains("RUNNER_TEST_UNSET_VAR is not set"), "{err}");
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

    /// `extra` added to a config with two providers (`main`, with a default
    /// model, and `other`, without) and two tools, and no agents of its own.
    fn agents(extra: &str, tools: &[&str]) -> Result<BTreeMap<String, AgentDefinition>> {
        let config: RunnerConfig = toml::from_str(&format!(
            r#"
            [providers.main]
            path = "provider.wasm"
            model = "big"
            [providers.other]
            path = "other-provider.wasm"
            [tools.{}]
            path = "files.wasm"
            [tools.{}]
            path = "docs.wasm"
            {extra}
            "#,
            tools[0], tools[1]
        ))
        .map_err(|e| Error::msg(e.to_string()))?;
        resolve_agents(&config)
    }

    fn tool_paths(agent: &AgentDefinition) -> Vec<&str> {
        agent
            .tools
            .values()
            .map(|tool| tool.path.as_str())
            .collect()
    }

    #[test]
    fn a_default_agent_must_be_declared() {
        let err = agents(
            "[agents.researcher]\nprovider = \"main\"",
            &["files", "docs"],
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("no [agents.default]"), "{err}");

        let declared = agents(
            "[agents.default]\nprovider = \"main\"\ntools = [\"docs\"]",
            &["files", "docs"],
        )
        .unwrap();
        let default = &declared["default"];
        assert_eq!(default.provider_name, "main");
        assert_eq!(default.model(), "big");
        assert_eq!(tool_paths(default), ["docs.wasm"]);
    }

    #[test]
    fn named_agents_pick_a_provider_tools_and_a_model() {
        let agents = agents(
            r#"
            [agents.default]
            provider = "main"
            [agents.researcher]
            provider = "main"
            model = "small"
            tools = ["docs"]
            system_prompt = "Cite your sources."
            max_tokens = 4096
            max_model_calls = 8
            [agents.bare]
            provider = "main"
            [agents.elsewhere]
            provider = "other"
            model = "theirs"
            "#,
            &["files", "docs"],
        )
        .unwrap();
        assert_eq!(
            agents.keys().collect::<Vec<_>>(),
            ["bare", "default", "elsewhere", "researcher"]
        );
        let researcher = &agents["researcher"];
        assert_eq!(researcher.model(), "small");
        assert_eq!(researcher.provider.component.path, "provider.wasm");
        assert_eq!(tool_paths(researcher), ["docs.wasm"]);
        assert_eq!(
            researcher.system_prompt.as_deref(),
            Some("Cite your sources.")
        );
        assert_eq!(agents["bare"].system_prompt, None);
        assert_eq!(
            (researcher.max_tokens, researcher.max_model_calls),
            (4096, Some(8))
        );
        // Without limits: the default token cap, and no cap on model calls.
        assert_eq!(
            (agents["bare"].max_tokens, agents["bare"].max_model_calls),
            (DEFAULT_MAX_TOKENS, None)
        );
        // Tools are opt-in for a named definition.
        assert!(agents["bare"].tools.is_empty());
        assert_eq!(
            agents["elsewhere"].provider.component.path,
            "other-provider.wasm"
        );
        assert_eq!(agents["elsewhere"].model(), "theirs");
        assert_eq!(agents["elsewhere"].provider_name, "other");
        // Overriding one agent's model leaves the provider's default alone.
        assert_eq!(agents["default"].model(), "big");
    }

    #[test]
    fn bad_agent_definitions_are_rejected() {
        let err = |extra: &str, tools: &[&str]| agents(extra, tools).err().unwrap().to_string();
        let tools = &["files", "docs"];
        let err = |extra: &str, tools: &[&str]| {
            err(
                &format!("[agents.default]\nprovider = \"main\"\n{extra}"),
                tools,
            )
        };
        let main = "[agents.r]\nprovider = \"main\"";
        assert!(
            err(&format!("{main}\ntools = [\"wether\"]"), tools)
                .contains("agent 'r': no tool named 'wether'")
        );
        assert!(err("", &["files", "files"]).contains("duplicate"));
        assert!(
            err("[agents.\"../x\"]\nprovider = \"main\"", tools)
                .contains("invalid agent name '../x'")
        );
        assert!(err(&format!("{main}\nmodle = \"x\""), tools).contains("unknown field"));
        assert!(
            err(&format!("{main}\nsystem_prompt = \" \""), tools)
                .contains("`system_prompt` is empty")
        );
        for zero in ["max_tokens = 0", "max_model_calls = 0"] {
            assert!(
                err(&format!("{main}\n{zero}"), tools).contains("must be at least 1"),
                "{zero}"
            );
        }
        assert!(err(&format!("{main}\nmax_tokens = -1"), tools).contains("invalid value"));
        // Every agent names its provider, and it has to exist.
        assert!(err("[agents.r]", tools).contains("missing field `provider`"));
        assert!(
            err("[agents.r]\nprovider = \"opneai\"", tools)
                .contains("agent 'r': no provider named 'opneai'")
        );
        // A provider without a default model needs the agent to pick one.
        assert!(
            err("[agents.r]\nprovider = \"other\"", tools)
                .contains("agent 'r': no model; set `model` on it or on [providers.other]")
        );
        // Providers are named by table key now, not configured inline.
        assert!(
            err(
                "[agents.r.provider]\npath = \"p.wasm\"\nmodel = \"y\"",
                tools
            )
            .contains("invalid type")
        );
    }

    #[test]
    fn tool_names_are_validated_from_config() {
        let mut config: RunnerConfig = toml::from_str(
            r#"
            [providers.main]
            path = "provider.wasm"
            model = "m"
            [tools.valid_name-1]
            path = "tool.wasm"
            [agents.default]
            provider = "main"
            tools = ["valid_name-1"]
            "#,
        )
        .unwrap();
        assert!(resolve_agents(&config).is_ok());
        let tool = config.tools.remove("valid_name-1").unwrap();
        for invalid in ["", "two words", "dot.name", "../file", "é", &"a".repeat(65)] {
            config.tools.clear();
            config.tools.insert(invalid.into(), tool.clone());
            let error = resolve_agents(&config).err().unwrap().to_string();
            assert!(error.contains("invalid tool name"), "{error}");
        }
        config.tools.clear();
        let provider = config.providers.remove("main").unwrap();
        config.providers.insert("dot.name".into(), provider);
        let error = resolve_agents(&config).err().unwrap().to_string();
        assert!(
            error.contains("invalid provider name 'dot.name'"),
            "{error}"
        );
    }

    #[test]
    fn example_config_has_valid_named_tools_and_agents() {
        let config: RunnerConfig = toml::from_str(include_str!("../../runner.toml")).unwrap();
        let agents = resolve_agents(&config).unwrap();
        assert_eq!(agents["researcher"].provider_name, "openai");
        assert_eq!(agents["researcher"].model(), "gpt-5-mini");
        assert_eq!(agents["researcher"].max_model_calls, Some(20));
        assert_eq!(agents["default"].provider_name, "anthropic");
        assert_eq!(agents["default"].model(), "claude-sonnet-5");
        assert_eq!(
            agents["reader"].tools.keys().collect::<Vec<_>>(),
            ["read_file"]
        );
        assert_eq!(
            agents["researcher"].tools.keys().collect::<Vec<_>>(),
            ["web_search"]
        );
        assert!(
            agents["researcher"]
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.starts_with("You are a research assistant."))
        );
    }

    #[test]
    fn every_providers_credentials_are_redacted() {
        let config: RunnerConfig = toml::from_str(
            r#"
            [providers.main]
            path = "provider.wasm"
            model = "m"
            [providers.other]
            path = "other.wasm"
            model = "m"
            allowed_hosts = ["api.example.com"]
            headers = { x-api-key = { value = "other-secret-key", hosts = ["api.example.com"] } }
            "#,
        )
        .unwrap();
        assert_eq!(
            redact(&config, "bad key other-secret-key"),
            "bad key [redacted]"
        );
    }

    #[test]
    fn session_store_config_is_rejected() {
        // Conversations are kept by the service now, not a component.
        let stale = toml::from_str::<RunnerConfig>(
            "[session_store]\npath = \"fs_store.wasm\"\ndir = \"sessions\"",
        );
        let error = stale.err().unwrap().to_string();
        assert!(error.contains("unknown field `session_store`"), "{error}");
    }

    #[test]
    fn agent_component_config_is_rejected() {
        // The turn loop is native now; a config naming an agent component is stale.
        let stale = toml::from_str::<RunnerConfig>(
            "agent = \"agent.wasm\"\n[providers.p]\npath = \"p.wasm\"\nmodel = \"m\"",
        );
        assert!(
            stale
                .err()
                .unwrap()
                .to_string()
                .contains("unknown field `agent`")
        );
        let err = agents(
            "[agents.default]\nprovider = \"main\"\nagent = \"other-agent.wasm\"",
            &["files", "docs"],
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("unknown field `agent`"), "{err}");
    }

    #[test]
    fn model_errors_are_described_for_the_user() {
        use provider_guest::local::agent::types::ApiErrorDetail;
        let api = |body: &str| {
            describe(&ChatError::Api(ApiErrorDetail {
                status: 401,
                body: body.into(),
            }))
        };
        // Anthropic and OpenAI both put the reason in error.message.
        assert_eq!(
            api(r#"{"error":{"message":"invalid x-api-key"}}"#),
            "API error 401: invalid x-api-key"
        );
        assert_eq!(api("  Unauthorized\n"), "API error 401: Unauthorized");
        assert_eq!(
            describe(&ChatError::Network("timed out".into())),
            "network error: timed out"
        );
        assert_eq!(
            describe(&ChatError::InvalidRequest("no messages".into())),
            "invalid request: no messages"
        );
        assert_eq!(
            describe(&ChatError::Internal("provider trapped".into())),
            "internal error: provider trapped"
        );
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
