use std::collections::HashMap;
use std::env;

use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Error, Result, Store, bail};
use wasmtime_wasi::p2::add_to_linker_sync;
use wasmtime_wasi::p2::bindings::sync::Command;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

wasmtime::component::bindgen!({
    world: "chat-guest",
    path: "../wit",
});

use local::agent::chat_api::{
    self, ApiErrorDetail, ChatError, ChatRequest, ChatResponse, ContentBlock, Role,
    ToolResultBlock, ToolUseBlock,
};

// Scoped in its own module so this world's generated types (also under the
// `local:agent` package, since wit_bindgen requires one package per
// directory) don't collide with the chat-guest bindgen above.
mod tool_guest {
    wasmtime::component::bindgen!({
        world: "tool-guest",
        path: "../wit",
    });
}
use tool_guest::exports::local::agent::tool_api::ToolDescription;

// Host-only: the guest never sees which provider or model is in use, so any
// guest component works unmodified against whichever of these the host is
// configured for.
#[derive(Clone, Copy)]
enum Provider {
    Anthropic,
    Openai,
}

#[derive(serde::Deserialize)]
struct RunnerConfig {
    agent: String,
    provider: String,
    model: String,
    #[serde(default)]
    tools: Vec<ToolConfig>,
}

#[derive(serde::Deserialize)]
struct ToolConfig {
    path: String,
    #[serde(default)]
    allowed_hosts: Vec<String>,
}

fn load_config(path: &str) -> Result<RunnerConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::msg(format!("reading config {path}: {e}")))?;
    toml::from_str(&text).map_err(|e| Error::msg(format!("parsing config {path}: {e}")))
}

/// Rejects any outbound request whose host isn't on this tool's own
/// allow-list, before it ever reaches the real network.
struct AllowListHooks {
    allowed_hosts: std::collections::HashSet<String>,
}

impl WasiHttpHooks for AllowListHooks {
    fn send_request(
        &mut self,
        request: http::Request<wasmtime_wasi_http::WasiBody>,
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
                eprintln!("tool network access denied: {denied_host}");
                Err(wasmtime_wasi_http::Error::HttpRequestDenied)
            });
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

struct ToolState {
    wasi: WasiCtx,
    http_ctx: WasiHttpCtx,
    table: ResourceTable,
    hooks: AllowListHooks,
}

impl WasiView for ToolState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for ToolState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http_ctx,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

struct ToolHandle {
    description: ToolDescription,
    instance: tool_guest::ToolGuest,
    store: Store<ToolState>,
}

fn load_tools(engine: &Engine, configs: &[ToolConfig]) -> Result<HashMap<String, ToolHandle>> {
    let mut tools = HashMap::new();

    for config in configs {
        let component = Component::from_file(engine, &config.path)
            .map_err(|e| Error::msg(format!("loading tool component {}: {e}", config.path)))?;

        let mut linker = Linker::<ToolState>::new(engine);
        add_to_linker_sync(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_sync::<ToolState>(&mut linker)?;

        let state = ToolState {
            wasi: WasiCtx::builder().build(),
            http_ctx: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            hooks: AllowListHooks {
                allowed_hosts: config.allowed_hosts.iter().cloned().collect(),
            },
        };
        let mut store = Store::new(engine, state);

        let instance = tool_guest::ToolGuest::instantiate(&mut store, &component, &linker)?;
        let description = instance.local_agent_tool_api().call_describe(&mut store)?;

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

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    anthropic_api_key: Option<String>,
    openai_api_key: Option<String>,
    http_agent: ureq::Agent,
    provider: Provider,
    model: String,
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
    fn chat(&mut self, request: ChatRequest) -> Result<ChatResponse, ChatError> {
        let descriptions: Vec<&ToolDescription> =
            self.tools.values().map(|t| &t.description).collect();

        match self.provider {
            Provider::Anthropic => {
                let api_key = self.anthropic_api_key.as_deref().ok_or_else(|| {
                    ChatError::Internal("ANTHROPIC_API_KEY is not set".to_string())
                })?;
                let body = build_anthropic_request(&request, &self.model, &descriptions);
                let json = post_json(
                    &self.http_agent,
                    "https://api.anthropic.com/v1/messages",
                    &[("x-api-key", api_key), ("anthropic-version", "2023-06-01")],
                    &body,
                )?;
                parse_anthropic_response(&json)
            }
            Provider::Openai => {
                let api_key = self
                    .openai_api_key
                    .as_deref()
                    .ok_or_else(|| ChatError::Internal("OPENAI_API_KEY is not set".to_string()))?;
                let body = build_openai_request(&request, &self.model, &descriptions);
                let auth = format!("Bearer {api_key}");
                let json = post_json(
                    &self.http_agent,
                    "https://api.openai.com/v1/chat/completions",
                    &[("authorization", auth.as_str())],
                    &body,
                )?;
                parse_openai_response(&json)
            }
        }
    }

    fn call_tool(&mut self, name: String, input: String) -> Result<String, String> {
        let handle = self
            .tools
            .get_mut(&name)
            .ok_or_else(|| format!("no such tool: {name}"))?;
        handle
            .instance
            .local_agent_tool_api()
            .call_call(&mut handle.store, &input)
            .map_err(|e| e.to_string())?
    }
}

fn post_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
) -> Result<serde_json::Value, ChatError> {
    let mut builder = agent.post(url).header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }

    match builder.send_json(body) {
        Ok(mut r) => r
            .body_mut()
            .read_json()
            .map_err(|e| ChatError::Internal(format!("decoding response: {e}"))),
        Err(ureq::Error::StatusCode(status)) => Err(ChatError::Api(ApiErrorDetail {
            status: status as u16,
            body: String::new(),
        })),
        Err(e) => Err(ChatError::Network(e.to_string())),
    }
}

fn content_block_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn build_anthropic_request(
    request: &ChatRequest,
    model: &str,
    tools: &[&ToolDescription],
) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let content: Vec<serde_json::Value> = m
                .content
                .iter()
                .map(|c| match c {
                    ContentBlock::Text(s) => serde_json::json!({ "type": "text", "text": s }),
                    ContentBlock::ToolUse(tu) => serde_json::json!({
                        "type": "tool_use",
                        "id": tu.id,
                        "name": tu.name,
                        "input": serde_json::from_str::<serde_json::Value>(&tu.input_json)
                            .unwrap_or(serde_json::Value::Null),
                    }),
                    ContentBlock::ToolResult(tr) => serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tr.tool_use_id,
                        "content": tr.content,
                    }),
                })
                .collect();
            serde_json::json!({ "role": role, "content": content })
        })
        .collect();

    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": request.max_tokens,
        "messages": messages,
    });
    if let Some(system) = &request.system {
        body["system"] = serde_json::Value::String(system.clone());
    }
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(
            tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": serde_json::from_str::<serde_json::Value>(&t.input_schema_json)
                            .unwrap_or(serde_json::Value::Null),
                    })
                })
                .collect(),
        );
    }
    body
}

fn parse_anthropic_response(json: &serde_json::Value) -> Result<ChatResponse, ChatError> {
    let content = json["content"]
        .as_array()
        .ok_or_else(|| ChatError::Internal("missing content array in response".to_string()))?
        .iter()
        .filter_map(|block| match block["type"].as_str() {
            Some("text") => block["text"]
                .as_str()
                .map(|s| ContentBlock::Text(s.to_string())),
            Some("tool_use") => Some(ContentBlock::ToolUse(ToolUseBlock {
                id: block["id"].as_str().unwrap_or_default().to_string(),
                name: block["name"].as_str().unwrap_or_default().to_string(),
                input_json: block["input"].to_string(),
            })),
            _ => None,
        })
        .collect();

    let stop_reason = json["stop_reason"].as_str().map(|s| s.to_string());

    Ok(ChatResponse {
        content,
        stop_reason,
    })
}

fn build_openai_request(
    request: &ChatRequest,
    model: &str,
    tools: &[&ToolDescription],
) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Some(system) = &request.system {
        messages.push(serde_json::json!({ "role": "system", "content": system }));
    }
    for m in &request.messages {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };

        // OpenAI puts tool results in their own `role: "tool"` messages
        // rather than folding them into a user message like Anthropic does.
        let tool_results: Vec<&ToolResultBlock> = m
            .content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::ToolResult(tr) => Some(tr),
                _ => None,
            })
            .collect();
        if !tool_results.is_empty() {
            for tr in tool_results {
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tr.tool_use_id,
                    "content": tr.content,
                }));
            }
            continue;
        }

        let tool_calls: Vec<serde_json::Value> = m
            .content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::ToolUse(tu) => Some(serde_json::json!({
                    "id": tu.id,
                    "type": "function",
                    "function": { "name": tu.name, "arguments": tu.input_json },
                })),
                _ => None,
            })
            .collect();

        let text = content_block_text(&m.content);
        let mut message = serde_json::json!({ "role": role, "content": text });
        if !tool_calls.is_empty() {
            message["tool_calls"] = serde_json::Value::Array(tool_calls);
        }
        messages.push(message);
    }

    let mut body = serde_json::json!({
        "model": model,
        // OpenAI's Chat Completions API deprecated `max_tokens` in favor of
        // `max_completion_tokens`.
        "max_completion_tokens": request.max_tokens,
        "messages": messages,
    });
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(
            tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": serde_json::from_str::<serde_json::Value>(&t.input_schema_json)
                                .unwrap_or(serde_json::Value::Null),
                        },
                    })
                })
                .collect(),
        );
    }
    body
}

fn parse_openai_response(json: &serde_json::Value) -> Result<ChatResponse, ChatError> {
    let message = &json["choices"][0]["message"];
    let mut content = Vec::new();

    if let Some(text) = message["content"].as_str() {
        if !text.is_empty() {
            content.push(ContentBlock::Text(text.to_string()));
        }
    }
    if let Some(tool_calls) = message["tool_calls"].as_array() {
        for call in tool_calls {
            content.push(ContentBlock::ToolUse(ToolUseBlock {
                id: call["id"].as_str().unwrap_or_default().to_string(),
                name: call["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                input_json: call["function"]["arguments"]
                    .as_str()
                    .unwrap_or("{}")
                    .to_string(),
            }));
        }
    }

    let stop_reason = json["choices"][0]["finish_reason"]
        .as_str()
        .map(|s| s.to_string());

    Ok(ChatResponse {
        content,
        stop_reason,
    })
}

fn main() -> Result<()> {
    let config_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "runner.toml".to_string());
    let config = load_config(&config_path)?;

    // Which provider is in use isn't known until the config above is
    // parsed, so neither key is required up front -- a missing key only
    // surfaces as a per-call `ChatError::Internal` from `chat()`.
    let anthropic_api_key = env::var("ANTHROPIC_API_KEY").ok();
    let openai_api_key = env::var("OPENAI_API_KEY").ok();

    let provider = match config.provider.as_str() {
        "anthropic" => Provider::Anthropic,
        "openai" => Provider::Openai,
        other => bail!("unknown provider '{other}' in {config_path}"),
    };

    let mut wasm_config = Config::new();
    wasm_config.wasm_component_model(true);
    let engine = Engine::new(&wasm_config)?;

    let tools = load_tools(&engine, &config.tools)?;

    let component = Component::from_file(&engine, &config.agent)
        .map_err(|e| Error::msg(format!("loading guest component at {}: {e}", config.agent)))?;

    let mut linker = Linker::<HostState>::new(&engine);
    add_to_linker_sync(&mut linker)?;
    chat_api::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;

    let wasi = WasiCtx::builder()
        .inherit_stdio()
        // Deliberately NOT .inherit_env() -- the guest gets zero ambient
        // env vars, so it has no path to the API key.
        .build();

    // Use the OS trust store (rather than ureq's bundled Mozilla roots) so
    // the host picks up whatever CA chain the platform actually trusts.
    let tls_config = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let http_agent =
        ureq::Agent::new_with_config(ureq::Agent::config_builder().tls_config(tls_config).build());

    let mut store = Store::new(
        &engine,
        HostState {
            wasi,
            table: ResourceTable::new(),
            anthropic_api_key,
            openai_api_key,
            http_agent,
            provider,
            model: config.model,
            tools,
        },
    );

    let command = Command::instantiate(&mut store, &component, &linker)?;
    if command.wasi_cli_run().call_run(&mut store)?.is_err() {
        bail!("guest exited with a non-zero result");
    }
    Ok(())
}
