//! Where a session's conversation is kept between turns, and its format.

use std::future::Future;
use std::pin::Pin;

use crate::provider_guest::local::agent::messages::{
    ContentBlock, Message, Role, ToolResultBlock, ToolUseBlock,
};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Stores each session's conversation as an opaque transcript, written by
/// `encode`. The service implements it over its database. Errors are for
/// the host's logs, not for clients.
pub trait Transcripts: Send + Sync + 'static {
    /// `None` if nothing is saved for `session` yet.
    fn load(&self, session: &str) -> BoxFuture<Result<Option<String>, String>>;
    /// Replaces whatever was saved for `session`.
    fn save(&self, session: &str, transcript: String) -> BoxFuture<Result<(), String>>;
}

// The stored format: serde's representation of the messages, the same JSON
// the old session-store component wrote. Kept apart from the WIT bindings,
// whose other generated types (streams, futures) can't be serialized.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredMessage {
    role: StoredRole,
    content: Vec<StoredBlock>,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum StoredRole {
    User,
    Assistant,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum StoredBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

pub(crate) fn encode(messages: &[Message]) -> String {
    let stored: Vec<StoredMessage> = messages
        .iter()
        .map(|message| StoredMessage {
            role: match message.role {
                Role::User => StoredRole::User,
                Role::Assistant => StoredRole::Assistant,
            },
            content: message
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text(text) => StoredBlock::Text(text.clone()),
                    ContentBlock::ToolUse(tool_use) => StoredBlock::ToolUse {
                        id: tool_use.id.clone(),
                        name: tool_use.name.clone(),
                        input_json: tool_use.input_json.clone(),
                    },
                    ContentBlock::ToolResult(result) => StoredBlock::ToolResult {
                        tool_use_id: result.tool_use_id.clone(),
                        content: result.content.clone(),
                    },
                })
                .collect(),
        })
        .collect();
    serde_json::to_string(&stored).expect("messages are serializable")
}

pub(crate) fn decode(transcript: &str) -> Result<Vec<Message>, serde_json::Error> {
    let stored: Vec<StoredMessage> = serde_json::from_str(transcript)?;
    Ok(stored
        .into_iter()
        .map(|message| Message {
            role: match message.role {
                StoredRole::User => Role::User,
                StoredRole::Assistant => Role::Assistant,
            },
            content: message
                .content
                .into_iter()
                .map(|block| match block {
                    StoredBlock::Text(text) => ContentBlock::Text(text),
                    StoredBlock::ToolUse {
                        id,
                        name,
                        input_json,
                    } => ContentBlock::ToolUse(ToolUseBlock {
                        id,
                        name,
                        input_json,
                    }),
                    StoredBlock::ToolResult {
                        tool_use_id,
                        content,
                    } => ContentBlock::ToolResult(ToolResultBlock {
                        tool_use_id,
                        content,
                    }),
                })
                .collect(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcripts_round_trip_in_the_session_store_format() {
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text("hi".into())],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse(ToolUseBlock {
                    id: "t1".into(),
                    name: "get_weather".into(),
                    input_json: "{}".into(),
                })],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: "t1".into(),
                    content: "sunny".into(),
                })],
            },
        ];
        let encoded = encode(&messages);
        // What the fs session store wrote for the same conversation.
        assert_eq!(
            encoded,
            r#"[{"role":"User","content":[{"Text":"hi"}]},{"role":"Assistant","content":[{"ToolUse":{"id":"t1","name":"get_weather","input_json":"{}"}}]},{"role":"User","content":[{"ToolResult":{"tool_use_id":"t1","content":"sunny"}}]}]"#
        );
        assert_eq!(encode(&decode(&encoded).unwrap()), encoded);
    }
}
