use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::{Mutex, mpsc, oneshot};
use wasmtime::component::{
    Accessor, Component, Destination, FutureConsumer, FutureReader, HasSelf, Lift, Linker,
    ResourceTable, Source, StreamConsumer, StreamProducer, StreamReader, StreamResult,
};
use wasmtime::{Config, Engine, Error, Result, Store, StoreContextMut, bail};
use wasmtime_wasi::p3::bindings::Command;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

wasmtime::component::bindgen!({
    world: "chat-guest",
    path: "../wit",
    imports: {
        // Calls into the tool's own store, which is async.
        "local:agent/chat-api.call-tool": async,
    },
});

use local::agent::chat_api;
use local::agent::types::{ChatError, ChatRequest, ChatResponse, ChatStream};

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
            "local:agent/types": crate::local::agent::types,
        },
    });
}
use provider_guest::exports::local::agent::provider_api::ToolDefinition;

#[derive(serde::Deserialize)]
struct RunnerConfig {
    agent: String,
    provider: ProviderConfig,
    #[serde(default)]
    tools: Vec<ComponentConfig>,
}

#[derive(serde::Deserialize)]
struct ProviderConfig {
    model: String,
    #[serde(flatten)]
    component: ComponentConfig,
}

/// A sandboxed component with network access: a provider or a tool.
#[derive(serde::Deserialize)]
struct ComponentConfig {
    path: String,
    #[serde(default)]
    allowed_hosts: Vec<String>,
    /// Headers the host adds to every outbound request this component makes,
    /// e.g. credentials. Values may reference env vars as `${NAME}`, which
    /// are resolved by the host -- the component itself never sees them.
    #[serde(default)]
    headers: BTreeMap<String, String>,
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

/// Rejects any outbound request whose host isn't on this component's own
/// allow-list, before it ever reaches the real network, and adds its
/// configured headers (credentials) to the ones that are allowed.
struct SandboxHooks {
    allowed_hosts: HashSet<String>,
    headers: http::HeaderMap,
}

impl SandboxHooks {
    fn new(config: &ComponentConfig) -> Result<Self> {
        let mut headers = http::HeaderMap::new();
        for (name, template) in &config.headers {
            let context = |e: &dyn std::fmt::Display| {
                Error::msg(format!("header '{name}' for {}: {e}", config.path))
            };
            let name = http::HeaderName::try_from(name.as_str()).map_err(|e| context(&e))?;
            let value = expand_env(template).map_err(|e| context(&e))?;
            let mut value = http::HeaderValue::try_from(value).map_err(|e| context(&e))?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(Self {
            allowed_hosts: config.allowed_hosts.iter().cloned().collect(),
            headers,
        })
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
        let host = request.uri().host().map(str::to_lowercase);
        let allowed = host
            .as_deref()
            .is_some_and(|h| self.allowed_hosts.contains(h));
        if !allowed {
            drop(fut);
            let denied_host = host.unwrap_or_default();
            return Box::new(async move {
                eprintln!("network access denied: {denied_host}");
                Err(wasmtime_wasi_http::Error::HttpRequestDenied)
            });
        }
        // Overwrites any same-named header the guest set itself, so a
        // component can't substitute its own credentials for the host's.
        for (name, value) in &self.headers {
            request.headers_mut().insert(name, value.clone());
        }
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
    engine: &Engine,
    config: &ComponentConfig,
) -> Result<(Component, Linker<SandboxState>, Store<SandboxState>)> {
    let component = Component::from_file(engine, &config.path)
        .map_err(|e| Error::msg(format!("loading component {}: {e}", config.path)))?;

    let mut linker = Linker::<SandboxState>::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;

    let state = SandboxState {
        wasi: WasiCtx::builder().build(),
        http_ctx: WasiHttpCtx::new(),
        table: ResourceTable::new(),
        hooks: SandboxHooks::new(config)?,
    };
    Ok((component, linker, Store::new(engine, state)))
}

struct ToolHandle {
    description: ToolDescription,
    instance: tool_guest::ToolGuest,
    store: Store<SandboxState>,
}

async fn load_tools(
    engine: &Engine,
    configs: &[ComponentConfig],
) -> Result<HashMap<String, ToolHandle>> {
    let mut tools = HashMap::new();

    for config in configs {
        let (component, linker, mut store) = load_sandboxed(engine, config)?;
        let instance =
            tool_guest::ToolGuest::instantiate_async(&mut store, &component, &linker).await?;
        let description = instance
            .local_agent_tool_api()
            .call_describe(&mut store)
            .await?;

        tools.insert(
            description.name.clone(),
            ToolHandle {
                description,
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

async fn load_provider(engine: &Engine, config: &ProviderConfig) -> Result<ProviderHandle> {
    let (component, linker, mut store) = load_sandboxed(engine, &config.component)?;
    let instance =
        provider_guest::ProviderGuest::instantiate_async(&mut store, &component, &linker).await?;
    Ok(ProviderHandle {
        model: config.model.clone(),
        instance,
        store,
    })
}

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    // Shared with the task that runs each reply, which has to keep driving
    // the provider's store while the agent's store reads what it produces.
    provider: Arc<Mutex<ProviderHandle>>,
    tool_definitions: Vec<ToolDefinition>,
    tools: HashMap<String, ToolHandle>,
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
        let handle = self
            .tools
            .get_mut(&name)
            .ok_or_else(|| format!("no such tool: {name}"))?;
        handle
            .instance
            .local_agent_tool_api()
            .call_call(&mut handle.store, &input)
            .await
            .map_err(|e| e.to_string())?
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
        tokio::spawn(async move {
            let response = run_provider(&provider, tools, request, text_tx).await;
            let _ = response_tx.send(response);
        });

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
        Ok(Err(trap)) | Err(trap) => Err(ChatError::Internal(format!(
            "provider trapped: {trap:?}"
        ))),
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

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "runner.toml".to_string());
    let config = load_config(&config_path)?;

    let mut wasm_config = Config::new();
    wasm_config.wasm_component_model(true);
    wasm_config.wasm_component_model_async(true);
    let engine = Engine::new(&wasm_config)?;

    let provider = load_provider(&engine, &config.provider).await?;
    let tools = load_tools(&engine, &config.tools).await?;
    let tool_definitions = tools
        .values()
        .map(|t| ToolDefinition {
            name: t.description.name.clone(),
            description: t.description.description.clone(),
            input_schema_json: t.description.input_schema_json.clone(),
        })
        .collect();

    let component = Component::from_file(&engine, &config.agent)
        .map_err(|e| Error::msg(format!("loading guest component at {}: {e}", config.agent)))?;

    let mut linker = Linker::<HostState>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    chat_api::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;

    let wasi = WasiCtx::builder()
        .inherit_stdio()
        // Deliberately NOT .inherit_env() -- the guest gets zero ambient
        // env vars, so it has no path to the API key.
        .build();

    let mut store = Store::new(
        &engine,
        HostState {
            wasi,
            table: ResourceTable::new(),
            provider: Arc::new(Mutex::new(provider)),
            tool_definitions,
            tools,
        },
    );

    // The agent exports the async WASI 0.3 `wasi:cli/run`, which is run
    // inside the store's event loop so its streaming `chat` calls work.
    let command = Command::instantiate_async(&mut store, &component, &linker).await?;
    let result = store
        .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
        .await??;
    if result.is_err() {
        bail!("guest exited with a non-zero result");
    }
    Ok(())
}
