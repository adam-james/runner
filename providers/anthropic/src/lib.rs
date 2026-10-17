wit_bindgen::generate!({
    world: "provider-guest",
    path: "../../wit",
});

use exports::local::agent::provider_api::{Guest, ToolDefinition};
use local::agent::types::{
    ApiErrorDetail, ChatError, ChatRequest, ChatResponse, ContentBlock, Role, ToolUseBlock,
};
use wstd::http::{Body, Client, Request};

struct Anthropic;

impl Guest for Anthropic {
    fn chat(
        model: String,
        tools: Vec<ToolDefinition>,
        request: ChatRequest,
    ) -> Result<ChatResponse, ChatError> {
        let body = build_request(&request, &model, &tools);
        let json = wstd::runtime::block_on(post_json("https://api.anthropic.com/v1/messages", &body))?;
        parse_response(&json)
    }
}

fn build_request(request: &ChatRequest, model: &str, tools: &[ToolDefinition]) -> serde_json::Value {
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

fn parse_response(json: &serde_json::Value) -> Result<ChatResponse, ChatError> {
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

/// No credentials here -- the host adds `x-api-key` to this request on its
/// way out, per this provider's `headers` in the runner config.
async fn post_json(url: &str, body: &serde_json::Value) -> Result<serde_json::Value, ChatError> {
    let request = Request::post(url)
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(body.to_string()))
        .map_err(|e| ChatError::Internal(e.to_string()))?;
    let response = Client::new()
        .send(request)
        .await
        .map_err(|e| ChatError::Network(e.to_string()))?;
    let status = response.status();
    let mut body = response.into_body();
    let bytes = body
        .contents()
        .await
        .map_err(|e| ChatError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(ChatError::Api(ApiErrorDetail {
            status: status.as_u16(),
            body: String::from_utf8_lossy(bytes).into_owned(),
        }));
    }
    serde_json::from_slice(bytes).map_err(|e| ChatError::Internal(format!("decoding response: {e}")))
}

export!(Anthropic);
