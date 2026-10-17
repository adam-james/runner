//! Everything the REPL prints, apart from the model's own text.
//!
//! Styling (ANSI colors, and the indicators that redraw a line in place)
//! only happens when the output is a terminal, so piped output stays plain.

use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use crate::local::agent::types::ChatError;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";
/// Back to the start of the line, then clear it.
const CLEAR_LINE: &str = "\r\x1b[K";

/// Longest a tool call's arguments get on screen, and any one string
/// argument within them.
const ARGS_LIMIT: usize = 60;
const STRING_ARG_LIMIT: usize = 30;

pub struct Ui {
    stdout_tty: bool,
    stderr_tty: bool,
}

impl Ui {
    pub fn new() -> Self {
        Self {
            stdout_tty: io::stdout().is_terminal(),
            stderr_tty: io::stderr().is_terminal(),
        }
    }

    pub fn banner(&self) {
        println!(
            "{}{}",
            self.style(BOLD, "runner"),
            self.style(DIM, " · type exit or quit, or Ctrl-D, to leave")
        );
    }

    /// Shows the prompt. In a terminal, what the user types after it is
    /// bold, until `end_input`.
    pub fn prompt(&self) {
        println!();
        if self.stdout_tty {
            print!("{BOLD}{CYAN}>{RESET} {BOLD}");
        } else {
            print!("> ");
        }
        flush();
    }

    pub fn end_input(&self) {
        if self.stdout_tty {
            print!("{RESET}");
            flush();
        }
    }

    /// Shows that a reply is on its way. Cleared by `stop_thinking`.
    pub fn thinking(&self) {
        if self.stdout_tty {
            print!("{DIM}…{RESET}");
            flush();
        }
    }

    pub fn stop_thinking(&self) {
        if self.stdout_tty {
            print!("{CLEAR_LINE}");
            flush();
        }
    }

    /// Shows a tool call as `⚙ name(args)` while it runs. Finished by
    /// `tool_done`.
    pub fn tool_start(&self, name: &str, input_json: &str) {
        print!("{}", self.style(DIM, &tool_call(name, input_json)));
        if self.stdout_tty {
            print!("{}", self.style(DIM, " …"));
        }
        flush();
    }

    pub fn tool_done(
        &self,
        name: &str,
        input_json: &str,
        result: &Result<String, String>,
        elapsed: Duration,
    ) {
        if self.stdout_tty {
            // Redraws the line to drop the "…".
            print!(
                "{CLEAR_LINE}{}",
                self.style(DIM, &tool_call(name, input_json))
            );
        }
        let secs = elapsed.as_secs_f32();
        match result {
            Ok(output) => println!(
                "{}",
                self.style(DIM, &format!(" ✓ {secs:.1}s · {}", size(output.len())))
            ),
            Err(e) => println!(
                "{}",
                self.style(RED, &format!(" ✗ {}", truncate(e, ARGS_LIMIT)))
            ),
        }
    }

    pub fn chat_error(&self, error: &ChatError) {
        self.error(&describe(error));
    }

    fn error(&self, message: &str) {
        if self.stderr_tty {
            eprintln!("{RED}{message}{RESET}");
        } else {
            eprintln!("{message}");
        }
    }

    fn style(&self, code: &str, text: &str) -> String {
        if self.stdout_tty {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }
}

fn flush() {
    io::stdout().flush().ok();
}

/// `⚙ name(key: value, …)`, from the tool's JSON input. Falls back to the
/// raw input if it isn't a JSON object.
fn tool_call(name: &str, input_json: &str) -> String {
    let args = match serde_json::from_str::<serde_json::Value>(input_json) {
        Ok(serde_json::Value::Object(fields)) => fields
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    serde_json::Value::String(s) => {
                        format!("\"{}\"", truncate(s, STRING_ARG_LIMIT))
                    }
                    other => other.to_string(),
                };
                format!("{key}: {value}")
            })
            .collect::<Vec<_>>()
            .join(", "),
        _ => input_json.to_string(),
    };
    format!("⚙ {name}({})", truncate(&args, ARGS_LIMIT))
}

/// Cuts `s` to at most `max` chars, ending in "…" if anything was cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max - 1).collect();
    format!("{kept}…")
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

fn size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f32 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_shows_object_fields() {
        assert_eq!(
            tool_call("sports", r#"{"league":"nfl","day":"today","limit":3}"#),
            r#"⚙ sports(league: "nfl", day: "today", limit: 3)"#
        );
    }

    #[test]
    fn tool_call_cuts_long_strings() {
        let input = format!(r#"{{"query":"{}"}}"#, "a".repeat(50));
        assert_eq!(
            tool_call("search", &input),
            format!(r#"⚙ search(query: "{}…")"#, "a".repeat(29))
        );
    }

    #[test]
    fn tool_call_falls_back_to_raw_input() {
        assert_eq!(tool_call("weather", "not json"), "⚙ weather(not json)");
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("héllo", 5), "héllo");
        assert_eq!(truncate("héllo", 4), "hél…");
    }

    #[test]
    fn api_error_message_is_extracted() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(
            api_error_message(body).as_deref(),
            Some("invalid x-api-key")
        );
        assert_eq!(api_error_message("<html>Bad Gateway</html>"), None);
    }

    #[test]
    fn size_switches_to_kb() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(2048), "2.0 KB");
    }
}
