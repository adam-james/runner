use wasip3::wit_bindgen;

wit_bindgen::generate!({
    world: "chat-guest",
    path: "../wit",
    runtime_path: "wasip3::wit_bindgen::rt",
});

use local::agent::messages::{ContentBlock, Message, Role, ToolResultBlock};
use local::agent::run_events::{self, Event, Usage};
use local::agent::types::{ChatError, ChatRequest, ChatStream};
use local::agent::{chat_api, session};

struct Agent;

export!(Agent);

impl exports::local::agent::agent_api::Guest for Agent {
    async fn run_turn(prompt: String) -> Result<(), String> {
        let mut history = session::load()?.unwrap_or_default();
        history.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text(prompt)],
        });
        run_turn(&mut history).await.map_err(|error| match error {
            TurnError::Chat(error) => describe(&error),
            TurnError::TokenLimit => format!(
                "reply cut off: hit the {MAX_TOKENS} token limit, so this turn wasn't saved"
            ),
        })?;
        session::save(&history)
    }
}

const SYSTEM_PROMPT: &str = "You are a helpful assistant.";
/// Per request, and it covers a reasoning model's hidden reasoning as well
/// as the reply, so it needs headroom. Only tokens actually used are billed.
const MAX_TOKENS: u32 = 16384;

/// Why a turn was abandoned.
enum TurnError {
    Chat(ChatError),
    /// A reply hit `MAX_TOKENS` and was cut off.
    TokenLimit,
}

impl From<ChatError> for TurnError {
    fn from(e: ChatError) -> Self {
        TurnError::Chat(e)
    }
}

/// Runs one turn to completion, emitting text and usage events. Calls tools
/// through the host and feeds results back until the model finishes.
async fn run_turn(history: &mut Vec<Message>) -> Result<(), TurnError> {
    loop {
        let request = ChatRequest {
            system: Some(SYSTEM_PROMPT.to_string()),
            max_tokens: MAX_TOKENS,
            messages: history.clone(),
        };

        let ChatStream { mut text, response } = chat_api::chat(request).await;
        while let Some(piece) = text.next().await {
            run_events::emit(&Event::Text(piece));
        }
        let response = response.await?;
        run_events::emit(&Event::Usage(Usage {
            reasoning_tokens: response.reasoning_tokens,
            max_tokens: MAX_TOKENS,
            prompt_tokens: response.prompt_tokens,
            cache_read_tokens: response.cache_read_tokens,
            cache_creation_tokens: response.cache_creation_tokens,
        }));
        // A cut-off reply may end mid-sentence or mid-tool-call, and keeping
        // it would only confuse the next turn. OpenAI reports "length",
        // Anthropic "max_tokens".
        if matches!(
            response.stop_reason.as_deref(),
            Some("length" | "max_tokens")
        ) {
            return Err(TurnError::TokenLimit);
        }

        let tool_results: Vec<ContentBlock> = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse(tu) => {
                    let result = chat_api::call_tool(&tu.name, &tu.input_json);
                    Some(ContentBlock::ToolResult(ToolResultBlock {
                        tool_use_id: tu.id.clone(),
                        content: result.unwrap_or_else(|e| format!("error: {e}")),
                    }))
                }
                _ => None,
            })
            .collect();

        history.push(Message {
            role: Role::Assistant,
            content: response.content.clone(),
        });

        if tool_results.is_empty() {
            return Ok(());
        }

        history.push(Message {
            role: Role::User,
            content: tool_results,
        });
    }
}

fn describe(error: &ChatError) -> String {
    match error {
        ChatError::InvalidRequest(m) => format!("invalid request: {m}"),
        ChatError::Network(m) => format!("network error: {m}"),
        ChatError::Api(detail) => format!(
            "API error {}: {}",
            detail.status,
            api_error_message(&detail.body).unwrap_or_else(|| detail.body.trim().to_string())
        ),
        ChatError::Internal(m) => format!("internal error: {m}"),
    }
}

/// The message out of an API error body shaped like
/// `{"error": {"message": ...}}`, which both Anthropic and OpenAI use.
fn api_error_message(body: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(body["error"]["message"].as_str()?.to_string())
}
