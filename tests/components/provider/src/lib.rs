//! Deterministic streaming provider for runtime integration tests. No network.
use wasip3::wit_bindgen;
wit_bindgen::generate!({
    world: "provider-guest",
    path: "../../../wit",
    runtime_path: "wasip3::wit_bindgen::rt",
});
use exports::local::agent::provider_api::{Guest, ToolDefinition};
use local::agent::messages::{ContentBlock, Role, ToolUseBlock};
use local::agent::types::{ChatError, ChatRequest, ChatResponse, ChatStream};
struct Provider;
impl Guest for Provider {
    async fn chat(_: String, _: Vec<ToolDefinition>, request: ChatRequest) -> ChatStream {
        let (mut writer, text) = wit_stream::new::<String>();
        let (result, response) =
            wit_future::new(|| Err(ChatError::Internal("fixture dropped".into())));
        wit_bindgen::spawn_local(async move {
            let blocks = &request.messages.last().unwrap().content;
            let prompt = blocks
                .iter()
                .find_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or_default();
            if prompt == "cpu" {
                loop {
                    std::hint::black_box(42);
                }
            }
            if prompt == "wait" {
                wasip3::clocks::monotonic_clock::wait_for(60_000_000_000).await;
            }
            if prompt == "fail" {
                drop(writer);
                result
                    .write(Err(ChatError::Internal("fixture failure".into())))
                    .await
                    .ok();
                return;
            }
            let tool = blocks.iter().find_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result.content.clone()),
                _ => None,
            });
            let content = if let Some(input) = prompt.strip_prefix("tool:") {
                vec![ContentBlock::ToolUse(ToolUseBlock {
                    id: "fixture-tool".into(),
                    name: "fixture".into(),
                    input_json: input.into(),
                })]
            } else {
                let turns = request
                    .messages
                    .iter()
                    .filter(|m| {
                        m.role == Role::User
                            && m.content.iter().any(|b| matches!(b, ContentBlock::Text(_)))
                    })
                    .count();
                let text = tool.unwrap_or_else(|| format!("turn {turns}: {prompt}"));
                // Multiple pieces exercise streaming across the component boundary.
                for word in text.split_inclusive(' ') {
                    writer.write_all(vec![word.to_string()]).await;
                }
                vec![ContentBlock::Text(text)]
            };
            drop(writer);
            result
                .write(Ok(ChatResponse {
                    content,
                    stop_reason: Some("end_turn".into()),
                    reasoning_tokens: Some(1),
                    prompt_tokens: Some(7),
                    cache_read_tokens: Some(0),
                    cache_creation_tokens: None,
                }))
                .await
                .ok();
        });
        ChatStream { text, response }
    }
}
export!(Provider);
