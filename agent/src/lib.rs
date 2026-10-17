use std::io::{self, BufRead, Write};

use wasip3::wit_bindgen;

wit_bindgen::generate!({
    world: "chat-guest",
    path: "../wit",
    runtime_path: "wasip3::wit_bindgen::rt",
});

use local::agent::chat_api;
use local::agent::types::{
    ChatError, ChatRequest, ChatStream, ContentBlock, Message, Role, ToolResultBlock,
};

// The entry point is the async WASI 0.3 `wasi:cli/run` rather than a plain
// `fn main`: awaiting the host's streaming `chat` from a synchronous
// (WASI 0.2-style) `run` would trap.
wasip3::cli::command::export!(Agent);

struct Agent;

impl wasip3::exports::cli::run::Guest for Agent {
    async fn run() -> Result<(), ()> {
        repl().await;
        Ok(())
    }
}

const SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const MAX_TOKENS: u32 = 1024;

async fn repl() {
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

        if let Err(e) = run_turn(&mut history).await {
            eprintln!("chat error: {e:?}");
            // Discard the whole turn (the user line plus any partial
            // assistant/tool-result messages the loop below managed to
            // push) rather than desyncing role alternation on the next call.
            history.truncate(turn_start);
        }

        prompt();
    }
}

/// Runs one human turn to completion: calls `chat`, printing its text as it
/// streams in, and for as long as the response asks for tools, calls them
/// via the generic `call-tool` import and feeds the results back.
async fn run_turn(history: &mut Vec<Message>) -> Result<(), ChatError> {
    loop {
        let request = ChatRequest {
            system: Some(SYSTEM_PROMPT.to_string()),
            max_tokens: MAX_TOKENS,
            messages: history.clone(),
        };

        let ChatStream { mut text, response } = chat_api::chat(request).await;
        let mut printed = false;
        while let Some(piece) = text.next().await {
            print!("{piece}");
            io::stdout().flush().ok();
            printed = true;
        }
        if printed {
            println!();
        }
        let response = response.await?;

        let tool_results: Vec<ContentBlock> = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse(tu) => {
                    println!("[tool] {} {}", tu.name, tu.input_json);
                    let result =
                        chat_api::call_tool(&tu.name, &tu.input_json).unwrap_or_else(|e| {
                            println!("[tool] {} failed: {e}", tu.name);
                            format!("error: {e}")
                        });
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
