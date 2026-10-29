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

struct Ollama;

impl Guest for Ollama {
    async fn chat(model: String, tools: Vec<ToolDefinition>, request: ChatRequest) -> ChatStream {
        let (mut text_tx, text_rx) = wit_stream::new::<String>();
        let (response_tx, response_rx) = wit_future::new(|| {
            Err(ChatError::Internal(
                "provider dropped the response".to_string(),
            ))
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
    let body = build_request(request, model, tools)?;
    let (status, mut events) = post_json("localhost:11434", "/api/chat", body).await?;
    if !(200..300).contains(&status) {
        return Err(ChatError::Api(ApiErrorDetail {
            status,
            body: String::from_utf8_lossy(&events.body.collect().await).into_owned(),
        }));
    }
    let mut reply = Accumulator::default();
    while let Some(data) = events.next().await {
        let chunk: serde_json::Value = serde_json::from_str(&data)
            .map_err(|e| ChatError::Internal(format!("decoding stream chunk: {e}")))?;
        if let Some(text) = reply.apply(&chunk)? {
            text_tx.write_all(vec![text]).await;
        }
        if chunk["done"].as_bool() == Some(true) {
            return Ok(reply.finish());
        }
    }
    Err(ChatError::Network(
        "response stream ended early".to_string(),
    ))
}

fn build_request(
    request: &ChatRequest,
    model: &str,
    tools: &[ToolDefinition],
) -> Result<serde_json::Value, ChatError> {
    let invalid = |e| ChatError::InvalidRequest(format!("invalid tool JSON: {e}"));
    // Ollama matches tool results by function name rather than by call ID.
    let mut names = std::collections::HashMap::new();
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        let mut text = String::new();
        let mut calls = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text(piece) => text.push_str(piece),
                ContentBlock::ToolUse(call) => {
                    names.insert(call.id.as_str(), call.name.as_str());
                    let arguments: serde_json::Value =
                        serde_json::from_str(&call.input_json).map_err(invalid)?;
                    calls.push(serde_json::json!({"function": {
                        "name": call.name, "arguments": arguments
                    }}));
                }
                ContentBlock::ToolResult(result) => {
                    let name = names.get(result.tool_use_id.as_str()).ok_or_else(|| {
                        ChatError::InvalidRequest(format!(
                            "unknown tool call: {}",
                            result.tool_use_id
                        ))
                    })?;
                    messages.push(serde_json::json!({
                        "role": "tool", "tool_name": name, "content": result.content
                    }));
                }
            }
        }
        if !text.is_empty() || !calls.is_empty() || message.content.is_empty() {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let mut value = serde_json::json!({"role": role, "content": text});
            if !calls.is_empty() {
                value["tool_calls"] = calls.into();
            }
            messages.push(value);
        }
    }
    let mut body = serde_json::json!({
        "model": model, "messages": messages, "stream": true,
        "options": {"num_predict": request.max_tokens},
    });
    if !tools.is_empty() {
        let definitions = tools
            .iter()
            .map(|tool| {
                let parameters: serde_json::Value =
                    serde_json::from_str(&tool.input_schema_json).map_err(invalid)?;
                Ok(serde_json::json!({"type": "function", "function": {
                    "name": tool.name, "description": tool.description, "parameters": parameters
                }}))
            })
            .collect::<Result<Vec<_>, ChatError>>()?;
        body["tools"] = definitions.into();
    }
    Ok(body)
}

#[derive(Default)]
struct Accumulator {
    text: String,
    tool_calls: Vec<ToolUseBlock>,
    stop_reason: Option<String>,
    prompt_tokens: Option<u32>,
}

impl Accumulator {
    fn apply(&mut self, chunk: &serde_json::Value) -> Result<Option<String>, ChatError> {
        if !chunk["error"].is_null() {
            return Err(ChatError::Internal(format!(
                "stream error: {}",
                chunk["error"]
            )));
        }
        for call in chunk["message"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let function = &call["function"];
            let name = function["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| ChatError::Internal("tool call missing name".into()))?;
            let arguments = &function["arguments"];
            if !arguments.is_object() {
                return Err(ChatError::Internal(
                    "tool arguments must be an object".into(),
                ));
            }
            // Ollama sends complete argument objects and may omit call IDs.
            self.tool_calls.push(ToolUseBlock {
                id: format!("ollama_call_{}", self.tool_calls.len()),
                name: name.into(),
                input_json: arguments.to_string(),
            });
        }
        if chunk["done"].as_bool() == Some(true) {
            self.stop_reason = chunk["done_reason"].as_str().map(str::to_owned);
            self.prompt_tokens = chunk["prompt_eval_count"]
                .as_u64()
                .and_then(|count| u32::try_from(count).ok());
        }
        match chunk["message"]["content"].as_str() {
            Some(piece) if !piece.is_empty() => {
                self.text.push_str(piece);
                Ok(Some(piece.into()))
            }
            _ => Ok(None),
        }
    }

    fn finish(self) -> ChatResponse {
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentBlock::Text(self.text));
        }
        content.extend(self.tool_calls.into_iter().map(ContentBlock::ToolUse));
        ChatResponse {
            content,
            stop_reason: self.stop_reason,
            prompt_tokens: self.prompt_tokens,
            reasoning_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        }
    }
}

/// A response body being read as newline-delimited JSON.
struct JsonStream {
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

impl JsonStream {
    /// Returns the next JSON record, or `None` once the body ends.
    async fn next(&mut self) -> Option<String> {
        loop {
            if let Some(event) = take_event(&mut self.buf) {
                match event {
                    Some(data) => return Some(data),
                    None => continue, // blank line
                }
            }
            if self.ended {
                return (!self.buf.is_empty())
                    .then(|| String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned());
            }
            let (result, chunk) = self.body.read(Vec::with_capacity(16 * 1024)).await;
            self.buf.extend(chunk.into_iter().filter(|&b| b != b'\r'));
            if !matches!(result, StreamResult::Complete(_)) {
                self.ended = true;
            }
        }
    }
}

/// Removes a complete NDJSON record, preserving partial UTF-8 until complete.
fn take_event(buf: &mut Vec<u8>) -> Option<Option<String>> {
    let end = buf.iter().position(|&b| b == b'\n')?;
    let line: Vec<u8> = buf.drain(..=end).collect();
    let line = String::from_utf8_lossy(&line).trim().to_string();
    Some((!line.is_empty()).then_some(line))
}

/// POSTs `body` as JSON over `wasi:http` and returns the status and the
/// response body as a JSON stream. Local Ollama requires no credentials.
async fn post_json(
    authority: &str,
    path: &str,
    body: serde_json::Value,
) -> Result<(u16, JsonStream), ChatError> {
    let internal = |e: &dyn std::fmt::Debug| ChatError::Internal(format!("{e:?}"));
    let headers = Fields::from_list(&[
        ("content-type".to_string(), b"application/json".to_vec()),
        ("accept".to_string(), b"application/x-ndjson".to_vec()),
    ])
    .map_err(|e| internal(&e))?;

    let (mut body_tx, body_rx) = wasip3::wit_stream::new();
    // Dropping the writer right away resolves this to its default: no trailers.
    let (_, trailers_rx) = wasip3::wit_future::new(|| Ok(None));
    let (request, sent) = Request::new(headers, Some(body_rx), trailers_rx, None);
    request
        .set_method(&Method::Post)
        .map_err(|e| internal(&e))?;
    request
        .set_scheme(Some(&Scheme::Http))
        .map_err(|e| internal(&e))?;
    request
        .set_authority(Some(authority))
        .map_err(|e| internal(&e))?;
    request
        .set_path_with_query(Some(path))
        .map_err(|e| internal(&e))?;

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
        JsonStream {
            body,
            buf: Vec::new(),
            ended: false,
            _done: done_tx,
            _sent: sent,
        },
    ))
}

export!(Ollama);

#[cfg(test)]
mod tests {
    use super::*;
    use local::agent::messages::{Message, ToolResultBlock};
    use serde_json::json;

    #[test]
    fn translates_tool_round_trip_and_preserves_text() {
        let mut reply = Accumulator::default();
        assert_eq!(
            reply
                .apply(&json!({"message":{"content":"Checking", "tool_calls":[
                    {"function":{"name":"weather", "arguments":{"city":"Paris"}}},
                    {"function":{"name":"weather", "arguments":{"city":"Rome"}}}
                ]}}))
                .unwrap()
                .as_deref(),
            Some("Checking")
        );
        reply
            .apply(&json!({"done":true,"done_reason":"stop","prompt_eval_count":123}))
            .unwrap();
        let response = reply.finish();
        assert_eq!(response.prompt_tokens, Some(123));
        assert_eq!(response.stop_reason.as_deref(), Some("stop"));
        assert_eq!(response.reasoning_tokens, None);
        let calls: Vec<_> = response
            .content
            .iter()
            .filter_map(|block| {
                if let ContentBlock::ToolUse(call) = block {
                    Some(call)
                } else {
                    None
                }
            })
            .collect();
        assert_ne!(calls[0].id, calls[1].id);
        let request = ChatRequest {
            system: Some("Be helpful".into()),
            max_tokens: 256,
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: response.content.clone(),
                },
                Message {
                    role: Role::User,
                    content: calls
                        .iter()
                        .map(|call| {
                            ContentBlock::ToolResult(ToolResultBlock {
                                tool_use_id: call.id.clone(),
                                content: "Sunny".into(),
                            })
                        })
                        .collect(),
                },
            ],
        };
        let body = build_request(
            &request,
            "qwen3:8b",
            &[ToolDefinition {
                name: "weather".into(),
                description: "Weather".into(),
                input_schema_json: r#"{"type":"object"}"#.into(),
            }],
        )
        .unwrap();
        assert_eq!(
            body["messages"][0],
            json!({"role":"system","content":"Be helpful"})
        );
        assert_eq!(body["messages"][1]["content"], "Checking");
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["arguments"],
            json!({"city":"Paris"})
        );
        assert_eq!(
            body["messages"][2],
            json!({"role":"tool","tool_name":"weather","content":"Sunny"})
        );
        assert_eq!(body["messages"].as_array().unwrap().len(), 4);
        assert_eq!(body["options"]["num_predict"], 256);
        assert_eq!(body["model"], "qwen3:8b");
        assert_eq!(
            body["tools"][0]["function"]["parameters"],
            json!({"type":"object"})
        );
    }

    #[test]
    fn parses_ndjson_across_arbitrary_chunks() {
        let stream = "{\"message\":{\"content\":\"Hé\"},\"done\":false}\r\n\n{\"message\":{\"content\":\"llo\"},\"done\":false}\n{\"done\":true,\"done_reason\":\"length\"}\n";
        for size in [1, 7, stream.len()] {
            let mut buf = Vec::new();
            let mut reply = Accumulator::default();
            let mut pieces = Vec::new();
            for chunk in stream.as_bytes().chunks(size) {
                buf.extend(chunk);
                while let Some(record) = take_event(&mut buf) {
                    if let Some(record) = record {
                        pieces.extend(
                            reply
                                .apply(&serde_json::from_str(&record).unwrap())
                                .unwrap(),
                        );
                    }
                }
            }
            assert!(buf.is_empty());
            assert_eq!(pieces, ["Hé", "llo"]);
            let response = reply.finish();
            assert!(matches!(&response.content[0], ContentBlock::Text(text) if text == "Héllo"));
            assert_eq!(response.stop_reason.as_deref(), Some("length"));
        }
    }

    #[test]
    fn rejects_stream_errors_and_malformed_tools() {
        for chunk in [
            json!({"error":"model missing"}),
            json!({"message":{"tool_calls":[{"function":{"arguments":{}}}]}}),
            json!({"message":{"tool_calls":[{"function":{"name":"bad","arguments":"{}"}}]}}),
        ] {
            assert!(Accumulator::default().apply(&chunk).is_err());
        }
    }

    #[test]
    fn rejects_invalid_request_json_and_unmatched_results() {
        let mut request = ChatRequest {
            system: None,
            max_tokens: 10,
            messages: vec![],
        };
        let body = build_request(&request, "model", &[]).unwrap();
        assert!(body.get("tools").is_none());
        let tool = ToolDefinition {
            name: "bad".into(),
            description: String::new(),
            input_schema_json: "{".into(),
        };
        assert!(build_request(&request, "model", &[tool]).is_err());
        request.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult(ToolResultBlock {
                tool_use_id: "unknown".into(),
                content: String::new(),
            })],
        });
        assert!(build_request(&request, "model", &[]).is_err());
        request.messages[0].content = vec![ContentBlock::ToolUse(ToolUseBlock {
            id: "call".into(),
            name: "bad".into(),
            input_json: "{".into(),
        })];
        assert!(build_request(&request, "model", &[]).is_err());
    }
}
