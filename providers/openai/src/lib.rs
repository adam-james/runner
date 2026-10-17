wit_bindgen::generate!({
    world: "provider-guest",
    path: "../../wit",
});

use exports::local::agent::provider_api::{Guest, ToolDefinition};
use local::agent::types::{
    ApiErrorDetail, ChatError, ChatRequest, ChatResponse, ContentBlock, Role, ToolResultBlock,
    ToolUseBlock,
};
use wstd::http::{Body, Client, Request};

struct Openai;

impl Guest for Openai {
    fn chat(
        model: String,
        tools: Vec<ToolDefinition>,
        request: ChatRequest,
    ) -> Result<ChatResponse, ChatError> {
        let body = build_request(&request, &model, &tools);
        let json = wstd::runtime::block_on(post_json(
            "https://api.openai.com/v1/chat/completions",
            &body,
        ))?;
        parse_response(&json)
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

fn build_request(request: &ChatRequest, model: &str, tools: &[ToolDefinition]) -> serde_json::Value {
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

fn parse_response(json: &serde_json::Value) -> Result<ChatResponse, ChatError> {
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

/// No credentials here -- the host adds `authorization` to this request on
/// its way out, per this provider's `headers` in the runner config.
async fn post_json(url: &str, body: &serde_json::Value) -> Result<serde_json::Value, ChatError> {
    let request = Request::post(url)
        .header("content-type", "application/json")
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

export!(Openai);
