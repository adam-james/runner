use std::env;

use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{bail, Config, Engine, Error, Result, Store};
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

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    api_key: String,
    http_agent: ureq::Agent,
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
        let body = build_anthropic_request(&request);

        let response = self
            .http_agent
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .send_json(&body);

        match response {
            Ok(mut r) => {
                let json: serde_json::Value = r
                    .body_mut()
                    .read_json()
                    .map_err(|e| ChatError::Internal(format!("decoding response: {e}")))?;
                parse_anthropic_response(&json)
            }
            Err(ureq::Error::StatusCode(status)) => Err(ChatError::Api(ApiErrorDetail {
                status: status as u16,
                body: String::new(),
            })),
            Err(e) => Err(ChatError::Network(e.to_string())),
        }
    }
}

fn build_anthropic_request(request: &ChatRequest) -> serde_json::Value {
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
        "model": request.model,
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

fn main() -> Result<()> {
    let api_key = env::var("ANTHROPIC_API_KEY")
        .map_err(|_| Error::msg("ANTHROPIC_API_KEY is not set in the host environment"))?;

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;

    let component_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "agent/target/wasm32-wasip2/release/agent.wasm".to_string());
    let component = Component::from_file(&engine, &component_path)
        .map_err(|e| Error::msg(format!("loading guest component at {component_path}: {e}")))?;

    let mut linker = Linker::<HostState>::new(&engine);
    add_to_linker_sync(&mut linker)?;
    chat_api::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;

    let wasi = WasiCtx::builder()
        .inherit_stdio()
        .inherit_args()
        // Deliberately NOT .inherit_env() -- the guest gets zero ambient
        // env vars, so it has no path to the API key.
        .build();

    // Use the OS trust store (rather than ureq's bundled Mozilla roots) so
    // the host picks up whatever CA chain the platform actually trusts.
    let tls_config = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let http_agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .tls_config(tls_config)
            .build(),
    );

    let mut store = Store::new(
        &engine,
        HostState {
            wasi,
            table: ResourceTable::new(),
            api_key,
            http_agent,
        },
    );

    let command = Command::instantiate(&mut store, &component, &linker)?;
    if command.wasi_cli_run().call_run(&mut store)?.is_err() {
        bail!("guest exited with a non-zero result");
    }
    Ok(())
}
