use std::io::{self, BufRead, Write};

wit_bindgen::generate!({
    world: "chat-guest",
    path: "../wit",
});

use local::agent::chat_api::{self, ChatRequest, ContentBlock, Message, Role};

const SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const MODEL: &str = "claude-sonnet-5";
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

        history.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text(line.to_string())],
        });

        let request = ChatRequest {
            model: MODEL.to_string(),
            system: Some(SYSTEM_PROMPT.to_string()),
            max_tokens: MAX_TOKENS,
            messages: history.clone(),
        };

        match chat_api::chat(&request) {
            Ok(response) => {
                println!("{}", flatten_text(&response.content));
                history.push(Message {
                    role: Role::Assistant,
                    content: response.content,
                });
            }
            Err(e) => {
                eprintln!("chat error: {e:?}");
                // Don't keep the dangling user-only turn; it would desync
                // role alternation on the next call.
                history.pop();
            }
        }

        prompt();
    }
}

fn prompt() {
    print!("> ");
    io::stdout().flush().ok();
}

fn flatten_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text(s) => s.as_str(),
        })
        .collect::<Vec<_>>()
        .join("")
}
