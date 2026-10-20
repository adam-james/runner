use std::io::{self, BufRead, Write};
use std::time::Instant;

use wasip3::wit_bindgen;

mod ui;
use ui::Ui;

wit_bindgen::generate!({
    world: "chat-guest",
    path: "../wit",
    runtime_path: "wasip3::wit_bindgen::rt",
});

use local::agent::messages::{ContentBlock, Message, Role, ToolResultBlock};
use local::agent::types::{ChatError, ChatRequest, ChatStream};
use local::agent::{chat_api, session};

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
    let ui = Ui::new();
    let stdin = io::stdin();
    ui.banner();
    let mut history = match session::load() {
        Ok(Some(history)) => {
            ui.resumed(&history);
            history
        }
        Ok(None) => Vec::new(),
        Err(e) => {
            // Starting over beats refusing to start, but saving would then
            // overwrite what couldn't be read, so stop here instead.
            ui.error(&format!("couldn't load session: {e}"));
            return;
        }
    };

    ui.prompt();
    for line in stdin.lock().lines() {
        ui.end_input();
        let Ok(line) = line else { break };
        let line = line.trim();

        if line.is_empty() {
            ui.prompt();
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

        match run_turn(&ui, &mut history).await {
            Ok(()) => {
                if let Err(e) = session::save(&history) {
                    ui.error(&format!("couldn't save session: {e}"));
                }
            }
            Err(e) => {
                ui.chat_error(&e);
                // Discard the whole turn (the user line plus any partial
                // assistant/tool-result messages the loop below managed to
                // push) rather than desyncing role alternation on the next
                // call.
                history.truncate(turn_start);
            }
        }

        ui.prompt();
    }
    // Leaves the user's shell prompt on a line of its own after Ctrl-D.
    ui.end_input();
    println!();
}

/// Runs one human turn to completion: calls `chat`, printing its text as it
/// streams in, and for as long as the response asks for tools, calls them
/// via the generic `call-tool` import and feeds the results back.
async fn run_turn(ui: &Ui, history: &mut Vec<Message>) -> Result<(), ChatError> {
    loop {
        let request = ChatRequest {
            system: Some(SYSTEM_PROMPT.to_string()),
            max_tokens: MAX_TOKENS,
            messages: history.clone(),
        };

        let mut thinking = Some(ui.thinking());
        let ChatStream { mut text, response } = chat_api::chat(request).await;
        while let Some(piece) = text.next().await {
            if let Some(thinking) = thinking.take() {
                ui.stop_thinking(thinking);
            }
            print!("{piece}");
            io::stdout().flush().ok();
        }
        match thinking {
            Some(thinking) => ui.stop_thinking(thinking),
            None => println!(),
        }
        let response = response.await?;

        let tool_results: Vec<ContentBlock> = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse(tu) => {
                    ui.tool_start(&tu.name, &tu.input_json);
                    let start = Instant::now();
                    let result = chat_api::call_tool(&tu.name, &tu.input_json);
                    ui.tool_done(&tu.name, &tu.input_json, &result, start.elapsed());
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
