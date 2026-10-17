use std::io::{self, BufRead, Write};

wit_bindgen::generate!({
    world: "chat-guest",
    path: "../wit",
});

use local::agent::chat_api;
use local::agent::types::{ChatRequest, ContentBlock, Message, Role, ToolResultBlock};

const SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const MAX_TOKENS: u32 = 1024;

fn main() {
    let stdin = io::stdin();
    let mut history: Vec<Message> = Vec::new();

    prompt();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();

        if line.is_empty() {
            prompt();
            continue;
        }
        if line == "exit" || line == "quit" {
            break;
        }

        let turn_start = history.len();
        history.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text(line.to_string())],
        });

        if let Err(e) = run_turn(&mut history) {
            eprintln!("chat error: {e:?}");
            // Discard the whole turn (the user line plus any partial
            // assistant/tool-result messages the loop below managed to
            // push) rather than desyncing role alternation on the next call.
            history.truncate(turn_start);
        }

        prompt();
    }
}

/// Runs one human turn to completion: calls `chat`, and for as long as the
/// response asks for tools, calls them via the generic `call-tool` import
/// and feeds the results back, before printing the final text reply.
fn run_turn(history: &mut Vec<Message>) -> Result<(), chat_api::ChatError> {
    loop {
        let request = ChatRequest {
            system: Some(SYSTEM_PROMPT.to_string()),
            max_tokens: MAX_TOKENS,
            messages: history.clone(),
        };

        let response = chat_api::chat(&request)?;

        let tool_results: Vec<ContentBlock> = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse(tu) => {
                    let result = chat_api::call_tool(&tu.name, &tu.input_json)
                        .unwrap_or_else(|e| format!("error: {e}"));
                    Some(ContentBlock::ToolResult(ToolResultBlock {
                        tool_use_id: tu.id.clone(),
                        content: result,
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
            println!("{}", flatten_text(&response.content));
            return Ok(());
        }

        history.push(Message {
            role: Role::User,
            content: tool_results,
        });
    }
}

fn prompt() {
    print!("> ");
    io::stdout().flush().ok();
}

fn flatten_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
