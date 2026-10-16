use std::env;

use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Error, Result, Store, bail};
use wasmtime_wasi::p2::add_to_linker_sync;
use wasmtime_wasi::p2::bindings::sync::Command;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    world: "chat-guest",
    path: "../wit",
});

use local::agent::chat_api::{
    self, ApiErrorDetail, ChatError, ChatRequest, ChatResponse, ContentBlock, Role,
};

// Host-only: the guest never sees which provider or model is in use, so any
// guest component works unmodified against whichever of these the host is
// configured for (see `parse_host_args`).
#[derive(Clone, Copy)]
enum Provider {
    Anthropic,
    Openai,
}

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    anthropic_api_key: Option<String>,
    openai_api_key: Option<String>,
    http_agent: ureq::Agent,
    provider: Provider,
    model: String,
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
        match self.provider {
            Provider::Anthropic => {
                let api_key = self.anthropic_api_key.as_deref().ok_or_else(|| {
                    ChatError::Internal("ANTHROPIC_API_KEY is not set".to_string())
                })?;
                let body = build_anthropic_request(&request, &self.model);
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
                let body = build_openai_request(&request, &self.model);
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

fn build_anthropic_request(request: &ChatRequest, model: &str) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let text = m
                .content
                .iter()
                .map(|c| match c {
                    ContentBlock::Text(s) => s.as_str(),
                })
                .collect::<Vec<_>>()
                .join("");
            serde_json::json!({ "role": role, "content": text })
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
    body
}

fn parse_anthropic_response(json: &serde_json::Value) -> Result<ChatResponse, ChatError> {
    let content = json["content"]
        .as_array()
        .ok_or_else(|| ChatError::Internal("missing content array in response".to_string()))?
        .iter()
        .filter_map(|block| block["text"].as_str())
        .map(|s| ContentBlock::Text(s.to_string()))
        .collect();

    let stop_reason = json["stop_reason"].as_str().map(|s| s.to_string());

    Ok(ChatResponse {
        content,
        stop_reason,
    })
}

fn build_openai_request(request: &ChatRequest, model: &str) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Some(system) = &request.system {
        messages.push(serde_json::json!({ "role": "system", "content": system }));
    }
    messages.extend(request.messages.iter().map(|m| {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let text = m
            .content
            .iter()
            .map(|c| match c {
                ContentBlock::Text(s) => s.as_str(),
            })
            .collect::<Vec<_>>()
            .join("");
        serde_json::json!({ "role": role, "content": text })
    }));

    serde_json::json!({
        "model": model,
        // OpenAI's Chat Completions API deprecated `max_tokens` in favor of
        // `max_completion_tokens`.
        "max_completion_tokens": request.max_tokens,
        "messages": messages,
    })
}

fn parse_openai_response(json: &serde_json::Value) -> Result<ChatResponse, ChatError> {
    let choice = &json["choices"][0];
    let text = choice["message"]["content"]
        .as_str()
        .ok_or_else(|| ChatError::Internal("missing choices[0].message.content".to_string()))?;
    let stop_reason = choice["finish_reason"].as_str().map(|s| s.to_string());

    Ok(ChatResponse {
        content: vec![ContentBlock::Text(text.to_string())],
        stop_reason,
    })
}

/// Parses the host's own CLI args: `<component-path> [--provider anthropic|openai] [--model <id>]`.
fn parse_host_args() -> (String, Provider, String) {
    let mut args = env::args();
    args.next(); // program name

    let component_path = args
        .next()
        .unwrap_or_else(|| "agent/target/wasm32-wasip2/release/agent.wasm".to_string());

    let mut provider = Provider::Anthropic;
    let mut model = "claude-sonnet-5".to_string();

    let rest: Vec<String> = args.collect();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--provider" => {
                if let Some(value) = rest.get(i + 1) {
                    provider = match value.as_str() {
                        "anthropic" => Provider::Anthropic,
                        "openai" => Provider::Openai,
                        other => {
                            eprintln!("unknown --provider '{other}', using anthropic");
                            Provider::Anthropic
                        }
                    };
                    i += 1;
                }
            }
            "--model" => {
                if let Some(value) = rest.get(i + 1) {
                    model = value.clone();
                    i += 1;
                }
            }
            other => eprintln!("ignoring unrecognized arg '{other}'"),
        }
        i += 1;
    }

    (component_path, provider, model)
}

fn main() -> Result<()> {
    // Which provider is in use isn't known until the host's own args are
    // parsed below, so neither key is required up front -- a missing key
    // only surfaces as a per-call `ChatError::Internal` from `chat()`.
    let anthropic_api_key = env::var("ANTHROPIC_API_KEY").ok();
    let openai_api_key = env::var("OPENAI_API_KEY").ok();

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;

    let (component_path, provider, model) = parse_host_args();
    let component = Component::from_file(&engine, &component_path)
        .map_err(|e| Error::msg(format!("loading guest component at {component_path}: {e}")))?;

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
            model,
        },
    );

    let command = Command::instantiate(&mut store, &component, &linker)?;
    if command.wasi_cli_run().call_run(&mut store)?.is_err() {
        bail!("guest exited with a non-zero result");
    }
    Ok(())
}
