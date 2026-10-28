//! Everything `runner chat` and `runner run` print, apart from the model's own text.
//!
//! Styling (ANSI colors, and the indicators that redraw a line in place)
//! only happens when the output is a terminal, so piped output stays plain.

use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::task::JoinHandle;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";
/// Back to the start of the line, then clear it.
const CLEAR_LINE: &str = "\r\x1b[K";

/// Longest a tool call's arguments get on screen, and any one string
/// argument within them.
pub(crate) const ARGS_LIMIT: usize = 60;
const STRING_ARG_LIMIT: usize = 30;

/// How often the "Thinking…" timer redraws.
const THINKING_TICK: Duration = Duration::from_millis(100);

/// A "Thinking…" indicator in progress.
struct Thinking {
    start: Instant,
    /// Started after a tool finished, so the model may not be called next
    /// (another tool may run first). Such an indicator is cleared silently.
    after_tool: bool,
    /// Held while drawing, so the redraw task never writes over other output.
    done: Arc<Mutex<bool>>,
    redraw: Option<JoinHandle<()>>,
}

/// Renders one run's events the way the old in-guest REPL did.
pub struct Ui {
    stdout_tty: bool,
    /// Rendering a run as it happens, rather than replaying a finished one:
    /// only then are there waits to show or time.
    live: bool,
    thinking: Option<Thinking>,
    queued: bool,
    running: bool,
    /// Model text is on the current line and needs a newline before anything else.
    in_text: bool,
    /// The `⚙ name(args)` line of the tool running now, to redraw when it finishes.
    tool: Option<String>,
}

impl Ui {
    /// Wait for persisted status events before showing an execution indicator.
    pub fn start() -> Self {
        Self::new(true)
    }

    /// For showing a finished run's events, with no "Thinking…" indicators
    /// or timings, which replayed events can't reproduce.
    pub fn replay() -> Self {
        Self::new(false)
    }

    fn new(live: bool) -> Self {
        Self {
            stdout_tty: io::stdout().is_terminal(),
            live,
            thinking: None,
            queued: false,
            running: false,
            in_text: false,
            tool: None,
        }
    }

    pub fn event(&mut self, data: &Value) {
        match data["kind"].as_str() {
            Some("delegated" | "delegation_waiting" | "delegation_finished") => {
                self.stop_thinking(false);
                self.end_text();
                if self.tool.take().is_some() {
                    println!();
                }
                let label = delegation_label(data);
                println!("{}", style(self.stdout_tty, DIM, &label));
            }
            Some("status") if self.live => match data["status"].as_str() {
                Some("queued") if !self.running => {
                    self.stop_thinking(false);
                    if !self.queued {
                        println!("{}", style(self.stdout_tty, DIM, "Queued — waiting to run"));
                    }
                    self.queued = true;
                }
                Some("running") => {
                    self.running = true;
                    self.queued = false;
                    if self.thinking.is_none() {
                        self.start_thinking(false);
                    }
                }
                Some("completed" | "failed" | "cancelled" | "interrupted") => self.finish(),
                _ => {}
            },
            Some("text") => {
                self.stop_thinking(true);
                print!("{}", data["text"].as_str().unwrap_or_default());
                flush();
                self.in_text = true;
            }
            Some("usage") => {
                self.stop_thinking(true);
                self.end_text();
                if let Some(line) = token_usage_line(data) {
                    println!("{}", style(self.stdout_tty, DIM, &line));
                }
            }
            Some("tool_started") => {
                // Only a model call with no text or usage ends in a tool; between tools nothing was thought.
                let model_responded = self.thinking.as_ref().is_some_and(|t| !t.after_tool);
                self.stop_thinking(model_responded);
                self.end_text();
                let call = tool_call(name(data), data["input"].as_str().unwrap_or_default());
                print!("{}", style(self.stdout_tty, DIM, &call));
                if self.stdout_tty && self.live {
                    print!("{}", style(true, DIM, " …"));
                }
                flush();
                self.tool = Some(call);
            }
            Some("tool_finished") => {
                let started = self.tool.take();
                if self.stdout_tty {
                    // Redraws the line to drop the "…".
                    print!("{CLEAR_LINE}");
                }
                if self.stdout_tty || started.is_none() {
                    // A replay that starts mid-call never saw tool_started.
                    let call = started.unwrap_or_else(|| tool_call(name(data), ""));
                    print!("{}", style(self.stdout_tty, DIM, &call));
                }
                let secs = data["elapsed_ms"].as_u64().unwrap_or(0) as f32 / 1000.0;
                if data["ok"] == true {
                    let bytes = data["bytes"].as_u64().unwrap_or(0) as usize;
                    println!(
                        "{}",
                        style(
                            self.stdout_tty,
                            DIM,
                            &format!(" ✓ {secs:.1}s · {}", size(bytes))
                        )
                    );
                } else {
                    let error = data["error"].as_str().unwrap_or("failed");
                    println!(
                        "{}",
                        style(
                            self.stdout_tty,
                            RED,
                            &format!(" ✗ {}", truncate(error, ARGS_LIMIT))
                        )
                    );
                }
                self.start_thinking(true);
            }
            _ => {}
        }
    }

    /// Clears any indicator and ends the reply's line, once the run is over.
    pub fn finish(&mut self) {
        self.stop_thinking(false);
        self.end_text();
        if self.tool.take().is_some() {
            println!();
        }
    }

    fn end_text(&mut self) {
        if self.in_text {
            println!();
            self.in_text = false;
        }
    }

    /// Shows `Thinking… 1.2s`, counting up, while a reply is on its way.
    fn start_thinking(&mut self, after_tool: bool) {
        if !self.live {
            return;
        }
        let done = Arc::new(Mutex::new(false));
        let start = Instant::now();
        let redraw = self.stdout_tty.then(|| {
            print!("{DIM}{}{RESET}", thinking_line(Duration::ZERO));
            flush();
            let done = done.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(THINKING_TICK).await;
                    let done = done.lock().unwrap();
                    if *done {
                        break;
                    }
                    print!("{CLEAR_LINE}{DIM}{}{RESET}", thinking_line(start.elapsed()));
                    flush();
                }
            })
        });
        self.thinking = Some(Thinking {
            start,
            after_tool,
            done,
            redraw,
        });
    }

    /// Replaces the indicator with how long the wait took, if the model was
    /// what we waited for.
    fn stop_thinking(&mut self, model_responded: bool) {
        let Some(thinking) = self.thinking.take() else {
            return;
        };
        *thinking.done.lock().unwrap() = true;
        if let Some(redraw) = thinking.redraw {
            redraw.abort();
            print!("{CLEAR_LINE}");
        }
        if model_responded {
            let secs = thinking.start.elapsed().as_secs_f32();
            println!(
                "{}",
                style(self.stdout_tty, DIM, &format!("Thought for {secs:.1}s"))
            );
        }
        flush();
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Chat's opening lines: the agent and session, then what the agent runs with.
pub fn banner(agent: &host::AgentInfo, session: &str) {
    let tty = io::stdout().is_terminal();
    println!(
        "{}{}",
        style(tty, BOLD, "runner"),
        style(
            tty,
            DIM,
            &format!(
                " · agent {} · session {session} · type exit or quit, or Ctrl-D, to leave",
                agent.name
            )
        )
    );
    println!("{}", style(tty, DIM, &agent_details(agent)));
    for line in tool_access(agent) {
        println!("{}", style(tty, DIM, &line));
    }
}

/// `  web_search: can reach api.tavily.com; ...`, one line per tool with
/// a component, saying what it can reach.
pub(crate) fn tool_access(agent: &host::AgentInfo) -> Vec<String> {
    agent
        .tool_access
        .iter()
        .map(|(tool, summary)| format!("  {tool}: {summary}"))
        .collect()
}

/// `provider openai · model gpt-5 · tools get_scores, get_weather`, then
/// any limits that aren't the defaults.
pub(crate) fn agent_details(agent: &host::AgentInfo) -> String {
    let tools = if agent.tools.is_empty() {
        "none".to_string()
    } else {
        agent.tools.join(", ")
    };
    let mut details = format!(
        "provider {} · model {} · tools {tools}",
        agent.provider, agent.model
    );
    if !agent.delegates.is_empty() {
        details.push_str(&format!(" · delegates {}", agent.delegates.join(", ")));
    }
    if agent.max_tokens != host::DEFAULT_MAX_TOKENS {
        details.push_str(&format!(" · max tokens {}", agent.max_tokens));
    }
    if let Some(calls) = agent.max_model_calls {
        details.push_str(&format!(" · max model calls {calls}"));
    }
    details
}

/// Says a saved conversation was picked up; its turns are replayed after.
pub fn resumed(turns: usize) {
    let tty = io::stdout().is_terminal();
    println!(
        "{}",
        style(
            tty,
            DIM,
            &format!(
                "resumed session · {turns} earlier {}",
                plural(turns, "turn")
            )
        )
    );
}

/// Shows the prompt. In a terminal, what the user types after it is bold,
/// until `end_input`.
pub fn prompt() {
    println!();
    if io::stdout().is_terminal() {
        print!("{BOLD}{CYAN}>{RESET} {BOLD}");
    } else {
        print!("> ");
    }
    flush();
}

/// An earlier turn's prompt, shown as it looked when it was typed.
pub fn past_prompt(text: &str) {
    prompt();
    println!("{text}");
    end_input();
}

/// Says how to pick the conversation back up, as `chat` ends.
pub fn goodbye(session: &str) {
    end_input();
    let tty = io::stdout().is_terminal();
    println!();
    println!(
        "{}",
        style(
            tty,
            DIM,
            &format!("resume with: runner chat --session {session}")
        )
    );
}

/// Says how to get back to the session `tui` was showing, once it has quit.
pub fn goodbye_tui(session: Option<&str>) {
    let tty = io::stdout().is_terminal();
    let line = match session {
        Some(session) => format!("runs keep going · reopen with: runner tui --session {session}"),
        None => "runs keep going · reopen with: runner tui".to_string(),
    };
    println!("{}", style(tty, DIM, &line));
}

/// The agent a `run` or `runs watch` is for and its session, then what the
/// agent runs with, if the daemon still has that agent. On stderr, so stdout
/// stays the run's output (or its ID, with `--detach`).
pub fn run_header(agent: &str, details: Option<&host::AgentInfo>, session: &str) {
    let tty = io::stderr().is_terminal();
    eprintln!(
        "{}",
        style(tty, DIM, &format!("agent {agent} · session {session}"))
    );
    if let Some(details) = details {
        eprintln!("{}", style(tty, DIM, &agent_details(details)));
        for line in tool_access(details) {
            eprintln!("{}", style(tty, DIM, &line));
        }
    }
}

/// Pending work shown before following its output.
pub fn pending(run: &crate::db::Run) {
    let tty = io::stdout().is_terminal();
    println!(
        "{}",
        style(
            tty,
            DIM,
            &format!("run {} · agent {} · {}", run.id, run.agent, run.status)
        )
    );
}

/// Detaching is a normal exit; the server continues the run.
pub fn detached(run: &crate::db::Run) {
    let tty = io::stderr().is_terminal();
    eprintln!(
        "{}",
        style(
            tty,
            DIM,
            &format!(
                "detached; run {} continues. Resume with: runner chat --session {}",
                run.id, run.session_id
            )
        )
    );
}

pub fn end_input() {
    if io::stdout().is_terminal() {
        print!("{RESET}");
        flush();
    }
}

pub fn error(message: &str) {
    if io::stderr().is_terminal() {
        eprintln!("{RED}{message}{RESET}");
    } else {
        eprintln!("{message}");
    }
}

fn style(tty: bool, code: &str, text: &str) -> String {
    if tty {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// `delegated to NAME · run ID`, and the like, for a delegation event.
pub(crate) fn delegation_label(data: &Value) -> String {
    match data["kind"].as_str() {
        Some("delegated") => format!(
            "delegated to {} · run {}",
            data["agent"].as_str().unwrap_or("agent"),
            data["run_id"].as_str().unwrap_or("")
        ),
        Some("delegation_waiting") => {
            format!("waiting for run {}", data["run_id"].as_str().unwrap_or(""))
        }
        _ => format!(
            "child {} · {}",
            data["run_id"].as_str().unwrap_or(""),
            data["status"].as_str().unwrap_or("finished")
        ),
    }
}

pub(crate) fn name(data: &Value) -> &str {
    data["name"].as_str().unwrap_or("tool")
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn flush() {
    io::stdout().flush().ok();
}

/// `⚙ name(key: value, …)`, from the tool's JSON input. Falls back to the
/// raw input if it isn't a JSON object.
pub(crate) fn tool_call(name: &str, input_json: &str) -> String {
    let args = match serde_json::from_str::<Value>(input_json) {
        Ok(Value::Object(fields)) => fields
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    Value::String(s) => format!("\"{}\"", truncate(s, STRING_ARG_LIMIT)),
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
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max - 1).collect();
    format!("{kept}…")
}

fn reasoning_line(tokens: u64, max_tokens: u64) -> String {
    let percent = tokens * 100 / max_tokens.max(1);
    format!("{tokens} / {max_tokens} max tokens on reasoning ({percent}%)")
}

/// Reasoning and prompt-cache usage reported for a model call.
pub(crate) fn token_usage_line(usage: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(tokens) = usage["reasoning_tokens"].as_u64() {
        parts.push(reasoning_line(
            tokens,
            usage["max_tokens"].as_u64().unwrap_or(0),
        ));
    }
    if let Some(tokens) = usage["prompt_tokens"].as_u64() {
        let mut cache = Vec::new();
        if let Some(read) = usage["cache_read_tokens"].as_u64() {
            cache.push(format!("{read} cached"));
        }
        if let Some(created) = usage["cache_creation_tokens"].as_u64() {
            cache.push(format!("{created} cache write"));
        }
        let details = if cache.is_empty() {
            String::new()
        } else {
            format!(" ({})", cache.join(", "))
        };
        parts.push(format!("{tokens} prompt tokens{details}"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

pub(crate) fn thinking_line(elapsed: Duration) -> String {
    format!("Thinking… {:.1}s", elapsed.as_secs_f32())
}

pub(crate) fn size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f32 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn queue_wait_does_not_start_thinking_or_rewind_running_state() {
        let mut ui = Ui::start();
        ui.event(&json!({"kind":"status", "status":"queued"}));
        assert!(ui.queued);
        assert!(ui.thinking.is_none());
        ui.event(&json!({"kind":"status", "status":"running"}));
        assert!(!ui.queued);
        assert!(ui.thinking.is_some());
        // A running attachment may replay its older queued event.
        ui.event(&json!({"kind":"status", "status":"queued"}));
        assert!(!ui.queued);
        ui.event(&json!({"kind":"status", "status":"completed"}));
        assert!(ui.thinking.is_none());
    }

    #[test]
    fn replay_ignores_execution_indicators() {
        let mut ui = Ui::replay();
        for status in ["queued", "running", "completed"] {
            ui.event(&json!({"kind":"status", "status":status}));
            assert!(ui.thinking.is_none());
            assert!(!ui.queued);
        }
    }

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
    fn reasoning_line_shows_share_of_max() {
        assert_eq!(
            reasoning_line(980, 1024),
            "980 / 1024 max tokens on reasoning (95%)"
        );
        assert_eq!(
            reasoning_line(0, 1024),
            "0 / 1024 max tokens on reasoning (0%)"
        );
    }

    #[test]
    fn token_usage_line_puts_prompt_cache_next_to_reasoning() {
        assert_eq!(
            token_usage_line(&json!({"reasoning_tokens":980, "max_tokens":1024,
                "prompt_tokens":1200, "cache_read_tokens":1000, "cache_creation_tokens":null}))
            .as_deref(),
            Some("980 / 1024 max tokens on reasoning (95%) · 1200 prompt tokens (1000 cached)")
        );
        assert_eq!(
            token_usage_line(&json!({"reasoning_tokens":null, "max_tokens":1024,
                "prompt_tokens":1000, "cache_read_tokens":700, "cache_creation_tokens":100}))
            .as_deref(),
            Some("1000 prompt tokens (700 cached, 100 cache write)")
        );
        assert_eq!(
            token_usage_line(&json!({"reasoning_tokens":null, "max_tokens":1024,
                "prompt_tokens":null, "cache_read_tokens":null, "cache_creation_tokens":null})),
            None
        );
    }

    #[test]
    fn agent_details_list_provider_model_and_tools() {
        let mut agent = host::AgentInfo {
            name: "default".into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            tools: vec!["get_scores".into(), "get_weather".into()],
            delegates: vec![],
            max_tokens: host::DEFAULT_MAX_TOKENS,
            max_model_calls: None,
            version: String::new(),
            tool_access: [(
                "get_weather".to_string(),
                "can reach api.open-meteo.com; no filesystem access".to_string(),
            )]
            .into(),
        };
        assert_eq!(
            tool_access(&agent),
            ["  get_weather: can reach api.open-meteo.com; no filesystem access"]
        );
        assert_eq!(
            agent_details(&agent),
            "provider openai · model gpt-5 · tools get_scores, get_weather"
        );
        agent.tools.clear();
        assert_eq!(
            agent_details(&agent),
            "provider openai · model gpt-5 · tools none"
        );
        agent.max_tokens = 4096;
        agent.max_model_calls = Some(8);
        assert_eq!(
            agent_details(&agent),
            "provider openai · model gpt-5 · tools none · max tokens 4096 · max model calls 8"
        );
    }

    #[test]
    fn size_switches_to_kb() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(2048), "2.0 KB");
    }
}
