use std::collections::BTreeMap;

use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response, Scheme};
use wasip3::wit_bindgen::{self, FutureReader, FutureWriter, StreamReader, StreamResult};

wit_bindgen::generate!({
    world: "provider-guest",
    path: "../../wit",
    runtime_path: "wasip3::wit_bindgen::rt",
});

use exports::local::agent::provider_api::{Guest, ToolDefinition};
use local::agent::messages::{ContentBlock, Role, ToolUseBlock};
use local::agent::types::{ApiErrorDetail, ChatError, ChatRequest, ChatResponse, ChatStream};

struct Anthropic;

impl Guest for Anthropic {
    async fn chat(
        model: String,
        tools: Vec<ToolDefinition>,
        // Anthropic has no equivalent to OpenAI's `prompt_cache_key`; its
        // prompt cache keys off request content plus `cache_control`.
        _session_id: Option<String>,
        request: ChatRequest,
    ) -> ChatStream {
        let (mut text_tx, text_rx) = wit_stream::new::<String>();
        let (response_tx, response_rx) = wit_future::new(|| {
            Err(ChatError::Internal("provider dropped the response".to_string()))
        });

        // Keeps running after `chat` returns the stream, feeding it as
        // events arrive, then resolves `response` with the whole reply.
        wit_bindgen::spawn_local(async move {
            let result = stream_chat(&model, &tools, &request, &mut text_tx).await;
            drop(text_tx);
            let _ = response_tx.write(result).await;
        });

        ChatStream {
            text: text_rx,
            response: response_rx,
        }
    }
}

async fn stream_chat(
    model: &str,
    tools: &[ToolDefinition],
    request: &ChatRequest,
    text_tx: &mut wit_bindgen::StreamWriter<String>,
) -> Result<ChatResponse, ChatError> {
    let mut body = build_request(request, model, tools);
    body["stream"] = serde_json::Value::Bool(true);

    let (status, mut events) = post_sse("api.anthropic.com", "/v1/messages", body).await?;
    if !(200..300).contains(&status) {
        return Err(ChatError::Api(ApiErrorDetail {
            status,
            body: String::from_utf8_lossy(&events.body.collect().await).into_owned(),
        }));
    }

    let mut reply = Accumulator::default();
    while let Some(data) = events.next().await {
        let event: serde_json::Value = serde_json::from_str(&data)
            .map_err(|e| ChatError::Internal(format!("decoding stream event: {e}")))?;
        if let Some(text) = reply.apply(&event)? {
            // A dropped reader just means nobody's watching the text; the
            // full reply still goes out through `response`.
            text_tx.write_all(vec![text]).await;
        }
        if reply.done {
            return Ok(reply.finish());
        }
    }
    Err(ChatError::Network("response stream ended early".to_string()))
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
        "cache_control": { "type": "ephemeral" },
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

enum Block {
    Text(String),
    ToolUse { id: String, name: String, input_json: String },
    /// Block types this runner doesn't use (e.g. thinking), kept so the
    /// stream's indices still line up.
    Other,
}

/// Rebuilds a complete reply from the Messages API's streaming events.
#[derive(Default)]
struct Accumulator {
    blocks: BTreeMap<u64, Block>,
    stop_reason: Option<String>,
    done: bool,
    input_tokens: Option<u32>,
    cache_read_tokens: Option<u32>,
    cache_creation_tokens: Option<u32>,
}

impl Accumulator {
    /// Applies one event, returning any new text to display.
    fn apply(&mut self, event: &serde_json::Value) -> Result<Option<String>, ChatError> {
        let index = event["index"].as_u64().unwrap_or_default();
        match event["type"].as_str() {
            Some("message_start") => {
                let usage = &event["message"]["usage"];
                self.input_tokens = usage["input_tokens"].as_u64().map(|n| n as u32);
                self.cache_read_tokens = usage["cache_read_input_tokens"]
                    .as_u64()
                    .map(|n| n as u32);
                self.cache_creation_tokens = usage["cache_creation_input_tokens"]
                    .as_u64()
                    .map(|n| n as u32);
            }
            Some("content_block_start") => {
                let block = &event["content_block"];
                let block = match block["type"].as_str() {
                    Some("text") => Block::Text(String::new()),
                    Some("tool_use") => Block::ToolUse {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        input_json: String::new(),
                    },
                    _ => Block::Other,
                };
                self.blocks.insert(index, block);
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                match (self.blocks.get_mut(&index), delta["type"].as_str()) {
                    (Some(Block::Text(text)), Some("text_delta")) => {
                        let piece = delta["text"].as_str().unwrap_or_default();
                        text.push_str(piece);
                        return Ok(Some(piece.to_string()));
                    }
                    (Some(Block::ToolUse { input_json, .. }), Some("input_json_delta")) => {
                        input_json.push_str(delta["partial_json"].as_str().unwrap_or_default());
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_string());
                }
            }
            Some("message_stop") => self.done = true,
            Some("error") => {
                return Err(ChatError::Internal(format!("stream error: {}", event["error"])));
            }
            _ => {}
        }
        Ok(None)
    }

    fn finish(self) -> ChatResponse {
        let content = self
            .blocks
            .into_values()
            .filter_map(|block| match block {
                Block::Text(text) => Some(ContentBlock::Text(text)),
                Block::ToolUse {
                    id,
                    name,
                    input_json,
                } => Some(ContentBlock::ToolUse(ToolUseBlock {
                    id,
                    name,
                    // A tool called with no arguments streams no JSON at all.
                    input_json: if input_json.is_empty() {
                        "{}".to_string()
                    } else {
                        input_json
                    },
                })),
                Block::Other => None,
            })
            .collect();
        ChatResponse {
            content,
            stop_reason: self.stop_reason,
            // Only reported as part of `output_tokens`, and thinking is off.
            reasoning_tokens: None,
            prompt_tokens: self.input_tokens.map(|tokens| {
                tokens
                    .saturating_add(self.cache_read_tokens.unwrap_or_default())
                    .saturating_add(self.cache_creation_tokens.unwrap_or_default())
            }),
            cache_read_tokens: self.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
        }
    }
}

/// A response body being read as server-sent events.
struct SseStream {
    body: StreamReader<u8>,
    buf: Vec<u8>,
    ended: bool,
    /// Tells the host we're done with the body (`Ok`) when dropped, so it
    /// must live as long as the body is still being read.
    _done: FutureWriter<Result<(), ErrorCode>>,
    /// The request's own result. The host ties the connection's lifetime to
    /// it, so dropping it early cuts off a response still streaming in.
    _sent: FutureReader<Result<(), ErrorCode>>,
}

impl SseStream {
    /// Returns the `data` of the next event, or `None` once the body ends.
    async fn next(&mut self) -> Option<String> {
        loop {
            if let Some(event) = take_event(&mut self.buf) {
                match event {
                    Some(data) => return Some(data),
                    None => continue, // comment or keep-alive, no data
                }
            }
            if self.ended {
                return None;
            }
            let (result, chunk) = self.body.read(Vec::with_capacity(16 * 1024)).await;
            self.buf.extend(chunk.into_iter().filter(|&b| b != b'\r'));
            if !matches!(result, StreamResult::Complete(_)) {
                self.ended = true;
            }
        }
    }
}

/// Removes the first complete event (terminated by a blank line) from
/// `buf`: `None` if there isn't one yet, `Some(None)` if it had no data.
fn take_event(buf: &mut Vec<u8>) -> Option<Option<String>> {
    let end = buf.windows(2).position(|w| w == b"\n\n")?;
    let event: Vec<u8> = buf.drain(..end + 2).collect();
    let event = String::from_utf8_lossy(&event);
    let data: Vec<&str> = event
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| data.strip_prefix(' ').unwrap_or(data))
        .collect();
    Some((!data.is_empty()).then(|| data.join("\n")))
}

/// POSTs `body` as JSON over `wasi:http` and returns the status and the
/// response body as an event stream. No credentials here -- the host adds
/// `x-api-key` to this request on its way out, per this provider's
/// `headers` in the runner config.
async fn post_sse(
    authority: &str,
    path: &str,
    body: serde_json::Value,
) -> Result<(u16, SseStream), ChatError> {
    let internal = |e: &dyn std::fmt::Debug| ChatError::Internal(format!("{e:?}"));
    let headers = Fields::from_list(&[
        ("content-type".to_string(), b"application/json".to_vec()),
        ("accept".to_string(), b"text/event-stream".to_vec()),
        ("anthropic-version".to_string(), b"2023-06-01".to_vec()),
    ])
    .map_err(|e| internal(&e))?;

    let (mut body_tx, body_rx) = wasip3::wit_stream::new();
    // Dropping the writer right away resolves this to its default: no trailers.
    let (_, trailers_rx) = wasip3::wit_future::new(|| Ok(None));
    let (request, sent) = Request::new(headers, Some(body_rx), trailers_rx, None);
    request.set_method(&Method::Post).map_err(|e| internal(&e))?;
    request.set_scheme(Some(&Scheme::Https)).map_err(|e| internal(&e))?;
    request.set_authority(Some(authority)).map_err(|e| internal(&e))?;
    request.set_path_with_query(Some(path)).map_err(|e| internal(&e))?;

    let bytes = body.to_string().into_bytes();
    wit_bindgen::spawn_local(async move {
        body_tx.write_all(bytes).await;
    });

    let response = wasip3::http::client::send(request)
        .await
        .map_err(|e| ChatError::Network(format!("{e:?}")))?;
    let status = response.get_status_code();
    let (done_tx, done_rx) = wasip3::wit_future::new(|| Ok(()));
    let (body, _trailers) = Response::consume_body(response, done_rx);
    Ok((
        status,
        SseStream {
            body,
            buf: Vec::new(),
            ended: false,
            _done: done_tx,
            _sent: sent,
        },
    ))
}

export!(Anthropic);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_ephemeral_prompt_caching() {
        let request = ChatRequest {
            system: None,
            max_tokens: 1024,
            messages: Vec::new(),
        };
        let body = build_request(&request, "claude-test", &[]);
        assert_eq!(
            body["cache_control"],
            serde_json::json!({ "type": "ephemeral" })
        );
    }

    /// Feeds a recorded Messages API stream through the same parsing as
    /// `stream_chat`, delivered in awkward chunk sizes.
    fn replay(stream: &str, chunk_size: usize) -> (Vec<String>, ChatResponse) {
        let mut buf = Vec::new();
        let mut reply = Accumulator::default();
        let mut pieces = Vec::new();
        for chunk in stream.as_bytes().chunks(chunk_size) {
            buf.extend(chunk.iter().filter(|&&b| b != b'\r'));
            while let Some(event) = take_event(&mut buf) {
                let Some(data) = event else { continue };
                let event: serde_json::Value = serde_json::from_str(&data).unwrap();
                pieces.extend(reply.apply(&event).unwrap());
            }
        }
        assert!(reply.done);
        (pieces, reply.finish())
    }

    const STREAM: &str = "event: message_start\r\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"content\":[],\"usage\":{\"input_tokens\":200,\"cache_read_input_tokens\":700,\"cache_creation_input_tokens\":100}}}\r\n\r\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
: keep-alive\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me \"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"check.\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\",\"input\":{}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"locat\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"ion\\\": \\\"Seattle\\\"}\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

    #[test]
    fn rebuilds_text_and_tool_call() {
        for chunk_size in [1, 7, 64, STREAM.len()] {
            let (pieces, response) = replay(STREAM, chunk_size);
            assert_eq!(pieces, ["Let me ", "check."]);
            assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
            assert_eq!(response.prompt_tokens, Some(1000));
            assert_eq!(response.cache_read_tokens, Some(700));
            assert_eq!(response.cache_creation_tokens, Some(100));
            let [ContentBlock::Text(text), ContentBlock::ToolUse(tool)] = &response.content[..] else {
                panic!("unexpected content: {:?}", response.content);
            };
            assert_eq!(text, "Let me check.");
            assert_eq!((tool.id.as_str(), tool.name.as_str()), ("toolu_1", "get_weather"));
            assert_eq!(tool.input_json, r#"{"location": "Seattle"}"#);
        }
    }

    #[test]
    fn tool_without_arguments_gets_empty_object() {
        let mut reply = Accumulator::default();
        reply
            .apply(&serde_json::json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "t", "name": "n", "input": {}}}))
            .unwrap();
        let ContentBlock::ToolUse(tool) = &reply.finish().content[0] else { panic!() };
        assert_eq!(tool.input_json, "{}");
    }

    #[test]
    fn stream_error_event_fails() {
        let error = serde_json::json!({"type": "error", "error": {"type": "overloaded_error"}});
        assert!(Accumulator::default().apply(&error).is_err());
    }
}
