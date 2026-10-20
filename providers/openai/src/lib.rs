use std::collections::BTreeMap;

use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response, Scheme};
use wasip3::wit_bindgen::{self, FutureReader, FutureWriter, StreamReader, StreamResult};

wit_bindgen::generate!({
    world: "provider-guest",
    path: "../../wit",
    runtime_path: "wasip3::wit_bindgen::rt",
});

use exports::local::agent::provider_api::{Guest, ToolDefinition};
use local::agent::messages::{ContentBlock, Role, ToolResultBlock, ToolUseBlock};
use local::agent::types::{ApiErrorDetail, ChatError, ChatRequest, ChatResponse, ChatStream};

struct Openai;

impl Guest for Openai {
    async fn chat(model: String, tools: Vec<ToolDefinition>, request: ChatRequest) -> ChatStream {
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

    let (status, mut events) = post_sse("api.openai.com", "/v1/chat/completions", body).await?;
    if !(200..300).contains(&status) {
        return Err(ChatError::Api(ApiErrorDetail {
            status,
            body: String::from_utf8_lossy(&events.body.collect().await).into_owned(),
        }));
    }

    let mut reply = Accumulator::default();
    while let Some(data) = events.next().await {
        if data == "[DONE]" {
            return Ok(reply.finish());
        }
        let chunk: serde_json::Value = serde_json::from_str(&data)
            .map_err(|e| ChatError::Internal(format!("decoding stream chunk: {e}")))?;
        if let Some(text) = reply.apply(&chunk)? {
            // A dropped reader just means nobody's watching the text; the
            // full reply still goes out through `response`.
            text_tx.write_all(vec![text]).await;
        }
    }
    Err(ChatError::Network("response stream ended early".to_string()))
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
        // Adds a last chunk with token counts, for `reasoning_tokens`.
        "stream_options": { "include_usage": true },
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

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Rebuilds a complete reply from Chat Completions' streaming chunks.
#[derive(Default)]
struct Accumulator {
    text: String,
    /// Keyed by each call's `index`, since a call's id and name arrive only
    /// in its first chunk and its arguments are split across later ones.
    tool_calls: BTreeMap<u64, ToolCall>,
    finish_reason: Option<String>,
    reasoning_tokens: Option<u32>,
}

impl Accumulator {
    /// Applies one chunk, returning any new text to display.
    fn apply(&mut self, chunk: &serde_json::Value) -> Result<Option<String>, ChatError> {
        if !chunk["error"].is_null() {
            return Err(ChatError::Internal(format!("stream error: {}", chunk["error"])));
        }
        let usage = &chunk["usage"]["completion_tokens_details"];
        if let Some(tokens) = usage["reasoning_tokens"].as_u64() {
            self.reasoning_tokens = Some(tokens as u32);
        }
        let choice = &chunk["choices"][0];
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        let delta = &choice["delta"];
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let entry = self
                .tool_calls
                .entry(call["index"].as_u64().unwrap_or_default())
                .or_default();
            if let Some(id) = call["id"].as_str() {
                entry.id = id.to_string();
            }
            if let Some(name) = call["function"]["name"].as_str() {
                entry.name.push_str(name);
            }
            if let Some(arguments) = call["function"]["arguments"].as_str() {
                entry.arguments.push_str(arguments);
            }
        }
        match delta["content"].as_str() {
            Some(piece) if !piece.is_empty() => {
                self.text.push_str(piece);
                Ok(Some(piece.to_string()))
            }
            _ => Ok(None),
        }
    }

    fn finish(self) -> ChatResponse {
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentBlock::Text(self.text));
        }
        for call in self.tool_calls.into_values() {
            content.push(ContentBlock::ToolUse(ToolUseBlock {
                id: call.id,
                name: call.name,
                input_json: if call.arguments.is_empty() {
                    "{}".to_string()
                } else {
                    call.arguments
                },
            }));
        }
        ChatResponse {
            content,
            stop_reason: self.finish_reason,
            reasoning_tokens: self.reasoning_tokens,
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
/// `authorization` to this request on its way out, per this provider's
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

export!(Openai);

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a recorded Chat Completions stream through the same parsing as
    /// `stream_chat`, delivered in awkward chunk sizes.
    fn replay(stream: &str, chunk_size: usize) -> (Vec<String>, ChatResponse) {
        let mut buf = Vec::new();
        let mut reply = Accumulator::default();
        let mut pieces = Vec::new();
        for chunk in stream.as_bytes().chunks(chunk_size) {
            buf.extend(chunk.iter().filter(|&&b| b != b'\r'));
            while let Some(event) = take_event(&mut buf) {
                let Some(data) = event else { continue };
                if data == "[DONE]" {
                    return (pieces, reply.finish());
                }
                let chunk: serde_json::Value = serde_json::from_str(&data).unwrap();
                pieces.extend(reply.apply(&chunk).unwrap());
            }
        }
        panic!("stream had no [DONE]");
    }

    const STREAM: &str = "\
data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\r\n\r\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me \"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"check.\"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"locat\"}}]},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"get_scores\",\"arguments\":\"{\\\"league\\\":\\\"nfl\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ion\\\":\\\"Seattle\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";

    #[test]
    fn reads_reasoning_tokens_from_usage_chunk() {
        // With `include_usage`, a last chunk with no choices carries the counts.
        let stream = "\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"stop\"}]}\n\n\
data: {\"choices\":[],\"usage\":{\"completion_tokens\":1024,\"completion_tokens_details\":{\"reasoning_tokens\":980}}}\n\n\
data: [DONE]\n\n";
        let (_, response) = replay(stream, 16);
        assert_eq!(response.reasoning_tokens, Some(980));
        assert_eq!(replay(STREAM, 16).1.reasoning_tokens, None);
    }

    #[test]
    fn rebuilds_text_and_interleaved_tool_calls() {
        for chunk_size in [1, 7, 64, STREAM.len()] {
            let (pieces, response) = replay(STREAM, chunk_size);
            assert_eq!(pieces, ["Let me ", "check."]);
            assert_eq!(response.stop_reason.as_deref(), Some("tool_calls"));
            let [ContentBlock::Text(text), ContentBlock::ToolUse(first), ContentBlock::ToolUse(second)] =
                &response.content[..]
            else {
                panic!("unexpected content: {:?}", response.content);
            };
            assert_eq!(text, "Let me check.");
            assert_eq!((first.id.as_str(), first.name.as_str()), ("call_1", "get_weather"));
            assert_eq!(first.input_json, r#"{"location":"Seattle"}"#);
            assert_eq!((second.id.as_str(), second.name.as_str()), ("call_2", "get_scores"));
            assert_eq!(second.input_json, r#"{"league":"nfl"}"#);
        }
    }

    #[test]
    fn stream_error_chunk_fails() {
        let error = serde_json::json!({"error": {"message": "server overloaded"}});
        assert!(Accumulator::default().apply(&error).is_err());
    }
}
