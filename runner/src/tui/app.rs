//! The TUI's state, and how keys and daemon messages change it. Nothing here
//! talks to the daemon: whatever needs it comes back from `update` as an
//! `Action` for the event loop to carry out.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::db::{Event, Run, Session, is_terminal};
use crate::ui::{ARGS_LIMIT, delegation_label, name, token_usage_line, tool_call, truncate};

/// How far PgUp/PgDn scroll the conversation.
const PAGE: u16 = 10;

pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    /// Redraws the "Thinking…" timer.
    Tick,
    Sessions(Vec<Session>),
    /// A session's runs, in submission order.
    Runs(String, Vec<Run>),
    /// Every event of a run that had already finished when it was listed.
    History {
        generation: u64,
        run_id: String,
        events: Vec<Event>,
    },
    /// One event of a run being followed live.
    RunEvent {
        generation: u64,
        run_id: String,
        data: Value,
    },
    /// A followed run's final state, once its events are all in.
    RunEnded {
        generation: u64,
        run: Run,
    },
    Submitted(Run),
    SessionCreated(Session),
    Notice(String),
    Error(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Submit {
        session: String,
        agent: String,
        prompt: String,
    },
    Cancel(String),
    CreateSession(Option<String>),
    /// Stop following the old session's runs and poll this one's.
    SelectSession {
        session: String,
        generation: u64,
    },
    /// Follow a run's events as they happen.
    Watch {
        run_id: String,
        generation: u64,
    },
    /// Read a finished run's events.
    LoadHistory {
        run_id: String,
        generation: u64,
    },
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Input,
    Sessions,
    Runs,
    Conversation,
}

const FOCUS_ORDER: [Focus; 4] = [
    Focus::Input,
    Focus::Sessions,
    Focus::Runs,
    Focus::Conversation,
];

#[derive(Debug, PartialEq, Eq)]
pub enum Popup {
    Help,
    /// The agent picker, with the highlighted agent's index.
    Agents(usize),
    /// The new session's name, as typed so far.
    NewSession(String),
    ConfirmCancel(String),
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Running,
    Ok { secs: f32, bytes: usize },
    Failed(String),
}

#[derive(Debug, PartialEq)]
pub enum Block {
    /// The model's reply text.
    Text(String),
    Tool {
        call: String,
        outcome: Outcome,
    },
    /// Timings, usage and delegation notes.
    Dim(String),
}

/// A "Thinking…" wait in progress.
struct Thinking {
    start: Instant,
    /// Started after a tool finished, so the model may not be called next.
    after_tool: bool,
}

/// One run of the session, with its events folded into what's shown, by the
/// same rules `ui::Ui` prints them with.
pub struct Turn {
    pub run: Run,
    pub blocks: Vec<Block>,
    /// Followed as it happens, rather than read after it finished: only then
    /// are there waits to show or time.
    live: bool,
    pub queued: bool,
    running: bool,
    thinking: Option<Thinking>,
}

impl Turn {
    fn new(run: Run, live: bool) -> Self {
        Self {
            run,
            blocks: Vec::new(),
            live,
            queued: false,
            running: false,
            thinking: None,
        }
    }

    /// How long the current "Thinking…" wait has lasted, if there is one.
    pub fn thinking(&self, now: Instant) -> Option<Duration> {
        self.thinking.as_ref().map(|t| now.duration_since(t.start))
    }

    fn apply(&mut self, data: &Value, now: Instant) {
        match data["kind"].as_str() {
            Some("delegated" | "delegation_waiting" | "delegation_finished") => {
                self.stop_thinking(false, now);
                self.blocks.push(Block::Dim(delegation_label(data)));
            }
            Some("status") if self.live => match data["status"].as_str() {
                Some("queued") if !self.running => {
                    self.stop_thinking(false, now);
                    self.queued = true;
                }
                Some("running") => {
                    self.running = true;
                    self.queued = false;
                    if self.thinking.is_none() {
                        self.start_thinking(false, now);
                    }
                }
                Some(status) if is_terminal(status) => self.finish(now),
                _ => {}
            },
            Some("text") => {
                self.stop_thinking(true, now);
                let text = data["text"].as_str().unwrap_or_default();
                match self.blocks.last_mut() {
                    Some(Block::Text(reply)) => reply.push_str(text),
                    _ => self.blocks.push(Block::Text(text.to_string())),
                }
            }
            Some("usage") => {
                self.stop_thinking(true, now);
                if let Some(line) = token_usage_line(data) {
                    self.blocks.push(Block::Dim(line));
                }
            }
            Some("tool_started") => {
                // Only a model call with no text or usage ends in a tool; between tools nothing was thought.
                let model_responded = self.thinking.as_ref().is_some_and(|t| !t.after_tool);
                self.stop_thinking(model_responded, now);
                self.blocks.push(Block::Tool {
                    call: tool_call(name(data), data["input"].as_str().unwrap_or_default()),
                    outcome: Outcome::Running,
                });
            }
            Some("tool_finished") => {
                let finished = if data["ok"] == true {
                    Outcome::Ok {
                        secs: data["elapsed_ms"].as_u64().unwrap_or(0) as f32 / 1000.0,
                        bytes: data["bytes"].as_u64().unwrap_or(0) as usize,
                    }
                } else {
                    Outcome::Failed(truncate(
                        data["error"].as_str().unwrap_or("failed"),
                        ARGS_LIMIT,
                    ))
                };
                match self.blocks.last_mut() {
                    Some(Block::Tool { outcome, .. }) if *outcome == Outcome::Running => {
                        *outcome = finished
                    }
                    // A replay that starts mid-call never saw tool_started.
                    _ => self.blocks.push(Block::Tool {
                        call: tool_call(name(data), ""),
                        outcome: finished,
                    }),
                }
                self.start_thinking(true, now);
            }
            _ => {}
        }
    }

    fn finish(&mut self, now: Instant) {
        self.stop_thinking(false, now);
        self.queued = false;
    }

    fn start_thinking(&mut self, after_tool: bool, now: Instant) {
        if self.live {
            self.thinking = Some(Thinking {
                start: now,
                after_tool,
            });
        }
    }

    /// Replaces the indicator with how long the wait took, if the model was
    /// what we waited for.
    fn stop_thinking(&mut self, model_responded: bool, now: Instant) {
        if let Some(thinking) = self.thinking.take()
            && model_responded
        {
            let secs = now.duration_since(thinking.start).as_secs_f32();
            self.blocks
                .push(Block::Dim(format!("Thought for {secs:.1}s")));
        }
    }
}

pub struct App {
    pub agents: Vec<host::AgentInfo>,
    /// The agent the next submitted turn runs.
    pub agent: String,
    /// The session shown and submitted to.
    pub session: String,
    pub sessions: Vec<Session>,
    pub session_cursor: usize,
    /// The shown session's runs, in submission order.
    pub turns: Vec<Turn>,
    pub run_cursor: usize,
    pub focus: Focus,
    pub input: String,
    /// The input cursor, in chars.
    pub cursor: usize,
    /// Conversation lines scrolled up from the bottom; 0 follows new output.
    pub scroll: u16,
    pub popup: Option<Popup>,
    /// A notice, or an error if `true`, shown in the status bar until the next key.
    pub status: Option<(bool, String)>,
    /// Bumped on every session switch, so messages for the old session's
    /// runs that are still in flight get dropped.
    generation: u64,
}

impl App {
    pub fn new(agents: Vec<host::AgentInfo>, agent: String, session: String) -> Self {
        Self {
            agents,
            agent,
            session,
            sessions: Vec::new(),
            session_cursor: 0,
            turns: Vec::new(),
            run_cursor: 0,
            focus: Focus::Input,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            popup: None,
            status: None,
            generation: 0,
        }
    }

    pub fn update(&mut self, msg: Msg, now: Instant) -> Vec<Action> {
        match msg {
            Msg::Key(key) => return self.key(key),
            Msg::Paste(text) => {
                if let Some(Popup::NewSession(name)) = &mut self.popup {
                    name.extend(text.chars().filter(|c| session_char(*c)));
                } else if self.popup.is_none() {
                    self.focus = Focus::Input;
                    self.insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
                }
            }
            Msg::Tick => {}
            Msg::Sessions(sessions) => {
                self.sessions = sessions;
                self.session_cursor = self
                    .session_cursor
                    .min(self.sessions.len().saturating_sub(1));
            }
            Msg::Runs(session, runs) => return self.runs(session, runs),
            Msg::History {
                generation,
                run_id,
                events,
            } => {
                if let Some(turn) = self.turn(generation, &run_id) {
                    for event in events {
                        turn.apply(&event.data, now);
                    }
                }
            }
            Msg::RunEvent {
                generation,
                run_id,
                data,
            } => {
                if let Some(turn) = self.turn(generation, &run_id) {
                    turn.apply(&data, now);
                }
            }
            Msg::RunEnded { generation, run } => {
                if let Some(turn) = self.turn(generation, &run.id.clone()) {
                    turn.run = run;
                    turn.finish(now);
                }
            }
            Msg::Submitted(run) => {
                if run.session_id == self.session && !self.turns.iter().any(|t| t.run.id == run.id)
                {
                    let action = Action::Watch {
                        run_id: run.id.clone(),
                        generation: self.generation,
                    };
                    self.turns.push(Turn::new(run, true));
                    return vec![action];
                }
            }
            Msg::SessionCreated(session) => {
                let id = session.id.clone();
                if !self.sessions.iter().any(|known| known.id == id) {
                    self.sessions.push(session);
                }
                return vec![self.select(id)];
            }
            Msg::Notice(text) => self.status = Some((false, text)),
            Msg::Error(text) => self.status = Some((true, text)),
        }
        Vec::new()
    }

    fn turn(&mut self, generation: u64, run_id: &str) -> Option<&mut Turn> {
        if generation != self.generation {
            return None;
        }
        self.turns.iter_mut().find(|turn| turn.run.id == run_id)
    }

    /// Takes in a fresh listing of the session's runs, starting to follow or
    /// read any it hasn't seen.
    fn runs(&mut self, session: String, runs: Vec<Run>) -> Vec<Action> {
        if session != self.session {
            return Vec::new();
        }
        let mut actions = Vec::new();
        for run in &runs {
            match self.turns.iter_mut().find(|turn| turn.run.id == run.id) {
                Some(turn) => turn.run = run.clone(),
                None => {
                    let run_id = run.id.clone();
                    let generation = self.generation;
                    actions.push(if run.terminal() {
                        Action::LoadHistory { run_id, generation }
                    } else {
                        Action::Watch { run_id, generation }
                    });
                    self.turns.push(Turn::new(run.clone(), !run.terminal()));
                }
            }
        }
        self.turns.sort_by_key(|turn| {
            runs.iter()
                .position(|run| run.id == turn.run.id)
                .unwrap_or(usize::MAX)
        });
        self.run_cursor = self.run_cursor.min(self.turns.len().saturating_sub(1));
        actions
    }

    /// Shows `session` instead of the current one.
    fn select(&mut self, session: String) -> Action {
        if let Some(index) = self.sessions.iter().position(|known| known.id == session) {
            self.session_cursor = index;
        }
        self.session = session.clone();
        self.turns.clear();
        self.run_cursor = 0;
        self.scroll = 0;
        self.focus = Focus::Input;
        self.generation += 1;
        Action::SelectSession {
            session,
            generation: self.generation,
        }
    }

    fn key(&mut self, key: KeyEvent) -> Vec<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return vec![Action::Quit];
        }
        self.status = None;
        if let Some(popup) = self.popup.take() {
            return self.popup_key(popup, key);
        }
        match key.code {
            KeyCode::Tab => self.cycle_focus(1),
            KeyCode::BackTab => self.cycle_focus(FOCUS_ORDER.len() - 1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(PAGE),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(PAGE),
            _ if self.focus == Focus::Input => return self.input_key(key),
            _ => return self.nav_key(key),
        }
        Vec::new()
    }

    fn cycle_focus(&mut self, step: usize) {
        let index = FOCUS_ORDER
            .iter()
            .position(|f| *f == self.focus)
            .unwrap_or(0);
        self.focus = FOCUS_ORDER[(index + step) % FOCUS_ORDER.len()];
    }

    fn input_key(&mut self, key: KeyEvent) -> Vec<Action> {
        match key.code {
            KeyCode::Enter => {
                let prompt = self.input.trim().to_string();
                if prompt.is_empty() {
                    return Vec::new();
                }
                self.input.clear();
                self.cursor = 0;
                self.scroll = 0;
                return vec![Action::Submit {
                    session: self.session.clone(),
                    agent: self.agent.clone(),
                    prompt,
                }];
            }
            KeyCode::Esc => self.focus = Focus::Conversation,
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert(&c.to_string())
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.byte_index());
            }
            KeyCode::Delete if self.cursor < self.input.chars().count() => {
                self.input.remove(self.byte_index());
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            _ => {}
        }
        Vec::new()
    }

    fn insert(&mut self, text: &str) {
        let at = self.byte_index();
        self.input.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    fn byte_index(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map_or(self.input.len(), |(i, _)| i)
    }

    fn nav_key(&mut self, key: KeyEvent) -> Vec<Action> {
        match key.code {
            KeyCode::Char('q') => return vec![Action::Quit],
            KeyCode::Char('?') => self.popup = Some(Popup::Help),
            KeyCode::Char('n') => self.popup = Some(Popup::NewSession(String::new())),
            KeyCode::Char('a') => {
                let index = self
                    .agents
                    .iter()
                    .position(|a| a.name == self.agent)
                    .unwrap_or(0);
                self.popup = Some(Popup::Agents(index));
            }
            KeyCode::Char('i') | KeyCode::Esc => self.focus = Focus::Input,
            KeyCode::End => self.scroll = 0,
            KeyCode::Home => self.scroll = u16::MAX,
            KeyCode::Up => match self.focus {
                Focus::Sessions => self.session_cursor = self.session_cursor.saturating_sub(1),
                Focus::Runs => self.run_cursor = self.run_cursor.saturating_sub(1),
                _ => self.scroll = self.scroll.saturating_add(1),
            },
            KeyCode::Down => match self.focus {
                Focus::Sessions => {
                    self.session_cursor =
                        (self.session_cursor + 1).min(self.sessions.len().saturating_sub(1))
                }
                Focus::Runs => {
                    self.run_cursor = (self.run_cursor + 1).min(self.turns.len().saturating_sub(1))
                }
                _ => self.scroll = self.scroll.saturating_sub(1),
            },
            KeyCode::Enter => match self.focus {
                Focus::Sessions => {
                    if let Some(session) = self.sessions.get(self.session_cursor) {
                        return vec![self.select(session.id.clone())];
                    }
                }
                _ => self.focus = Focus::Input,
            },
            KeyCode::Char('c') if self.focus == Focus::Runs => {
                match self.turns.get(self.run_cursor) {
                    Some(turn) if !turn.run.terminal() => {
                        self.popup = Some(Popup::ConfirmCancel(turn.run.id.clone()))
                    }
                    Some(turn) => {
                        self.status = Some((false, format!("run already {}", turn.run.status)))
                    }
                    None => {}
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn popup_key(&mut self, popup: Popup, key: KeyEvent) -> Vec<Action> {
        match popup {
            Popup::Help => {}
            Popup::Agents(index) => match key.code {
                KeyCode::Up => self.popup = Some(Popup::Agents(index.saturating_sub(1))),
                KeyCode::Down => {
                    let last = self.agents.len().saturating_sub(1);
                    self.popup = Some(Popup::Agents((index + 1).min(last)));
                }
                KeyCode::Enter => {
                    if let Some(agent) = self.agents.get(index) {
                        self.agent = agent.name.clone();
                        self.status = Some((false, format!("next turns run agent {}", agent.name)));
                    }
                }
                KeyCode::Esc => {}
                _ => self.popup = Some(Popup::Agents(index)),
            },
            Popup::NewSession(mut name) => match key.code {
                KeyCode::Enter => {
                    return vec![Action::CreateSession((!name.is_empty()).then_some(name))];
                }
                KeyCode::Esc => {}
                KeyCode::Backspace => {
                    name.pop();
                    self.popup = Some(Popup::NewSession(name));
                }
                KeyCode::Char(c) if session_char(c) => {
                    name.push(c);
                    self.popup = Some(Popup::NewSession(name));
                }
                _ => self.popup = Some(Popup::NewSession(name)),
            },
            Popup::ConfirmCancel(run_id) => {
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    return vec![Action::Cancel(run_id)];
                }
            }
        }
        Vec::new()
    }
}

/// What a session ID may contain; the daemon checks the rest.
fn session_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn run(id: &str, status: &str) -> Run {
        Run {
            id: id.into(),
            session_id: "s".into(),
            agent: "default".into(),
            prompt: format!("prompt {id}"),
            status: status.into(),
            created_at: 0,
            deadline_secs: 30,
            cancel_requested: false,
            error: None,
            parent_run_id: None,
            root_run_id: id.into(),
            depth: 0,
            deadline_at: None,
        }
    }

    fn agent(name: &str) -> host::AgentInfo {
        host::AgentInfo {
            name: name.into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            tools: vec![],
            delegates: vec![],
            max_tokens: host::DEFAULT_MAX_TOKENS,
            max_model_calls: None,
        }
    }

    fn app() -> App {
        App::new(
            vec![agent("default"), agent("reader")],
            "default".into(),
            "s".into(),
        )
    }

    fn press(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent::from(code))
    }

    #[test]
    fn live_turn_times_thinking_and_pairs_tool_calls() {
        let start = Instant::now();
        let mut turn = Turn::new(run("r", "running"), true);
        turn.apply(&json!({"kind":"status","status":"queued"}), start);
        assert!(turn.queued);
        turn.apply(&json!({"kind":"status","status":"running"}), start);
        assert!(!turn.queued);
        assert!(turn.thinking(start).is_some());
        let later = start + Duration::from_millis(1500);
        turn.apply(
            &json!({"kind":"tool_started","name":"get_weather","input":r#"{"city":"Oslo"}"#}),
            later,
        );
        turn.apply(&json!({"kind":"tool_finished","name":"get_weather","ok":true,"elapsed_ms":400,"bytes":2048}), later);
        turn.apply(&json!({"kind":"text","text":"Sunny"}), later);
        turn.apply(&json!({"kind":"text","text":" today"}), later);
        turn.apply(&json!({"kind":"status","status":"completed"}), later);
        assert!(turn.thinking(later).is_none());
        assert_eq!(
            turn.blocks,
            vec![
                Block::Dim("Thought for 1.5s".into()),
                Block::Tool {
                    call: r#"⚙ get_weather(city: "Oslo")"#.into(),
                    outcome: Outcome::Ok {
                        secs: 0.4,
                        bytes: 2048
                    },
                },
                // Waiting after a tool only counts if the model was called.
                Block::Dim("Thought for 0.0s".into()),
                Block::Text("Sunny today".into()),
            ]
        );
    }

    #[test]
    fn history_has_no_timings_and_tolerates_a_missing_tool_start() {
        let now = Instant::now();
        let mut turn = Turn::new(run("r", "completed"), false);
        for data in [
            json!({"kind":"status","status":"running"}),
            json!({"kind":"tool_finished","name":"search","ok":false,"error":"boom"}),
            json!({"kind":"delegated","agent":"helper","run_id":"c1"}),
            json!({"kind":"text","text":"done"}),
        ] {
            turn.apply(&data, now);
        }
        assert_eq!(
            turn.blocks,
            vec![
                Block::Tool {
                    call: "⚙ search()".into(),
                    outcome: Outcome::Failed("boom".into()),
                },
                Block::Dim("delegated to helper · run c1".into()),
                Block::Text("done".into()),
            ]
        );
    }

    #[test]
    fn listing_follows_new_runs_and_reads_finished_ones_once() {
        let mut app = app();
        let actions = app.update(
            Msg::Runs("s".into(), vec![run("a", "completed"), run("b", "running")]),
            Instant::now(),
        );
        assert_eq!(
            actions,
            vec![
                Action::LoadHistory {
                    run_id: "a".into(),
                    generation: 0
                },
                Action::Watch {
                    run_id: "b".into(),
                    generation: 0
                },
            ]
        );
        let again = app.update(
            Msg::Runs(
                "s".into(),
                vec![run("a", "completed"), run("b", "completed")],
            ),
            Instant::now(),
        );
        assert!(again.is_empty());
        assert_eq!(app.turns[1].run.status, "completed");
        // Another session's listing is stale.
        assert!(
            app.update(
                Msg::Runs("t".into(), vec![run("c", "queued")]),
                Instant::now()
            )
            .is_empty()
        );
    }

    #[test]
    fn switching_sessions_drops_the_old_sessions_messages() {
        let mut app = app();
        app.update(
            Msg::Runs("s".into(), vec![run("a", "running")]),
            Instant::now(),
        );
        app.sessions = vec![
            Session {
                id: "s".into(),
                created_at: 0,
                running_runs: 0,
                queued_runs: 0,
            },
            Session {
                id: "t".into(),
                created_at: 0,
                running_runs: 0,
                queued_runs: 0,
            },
        ];
        app.focus = Focus::Sessions;
        app.update(press(KeyCode::Down), Instant::now());
        let actions = app.update(press(KeyCode::Enter), Instant::now());
        assert_eq!(
            actions,
            vec![Action::SelectSession {
                session: "t".into(),
                generation: 1
            }]
        );
        assert!(app.turns.is_empty());
        app.update(
            Msg::RunEvent {
                generation: 0,
                run_id: "a".into(),
                data: json!({"kind":"text","text":"x"}),
            },
            Instant::now(),
        );
        assert!(app.turns.is_empty());
    }

    #[test]
    fn enter_submits_trimmed_input_and_ignores_blank() {
        let mut app = app();
        assert!(app.update(press(KeyCode::Enter), Instant::now()).is_empty());
        for c in " hi ".chars() {
            app.update(press(KeyCode::Char(c)), Instant::now());
        }
        app.update(press(KeyCode::Left), Instant::now());
        app.update(press(KeyCode::Backspace), Instant::now());
        assert_eq!(app.input, " h ");
        let actions = app.update(press(KeyCode::Enter), Instant::now());
        assert_eq!(
            actions,
            vec![Action::Submit {
                session: "s".into(),
                agent: "default".into(),
                prompt: "h".into()
            }]
        );
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn cancelling_a_run_needs_confirmation() {
        let mut app = app();
        app.update(
            Msg::Runs("s".into(), vec![run("a", "completed"), run("b", "running")]),
            Instant::now(),
        );
        app.focus = Focus::Runs;
        app.update(press(KeyCode::Char('c')), Instant::now());
        assert!(app.popup.is_none(), "a finished run can't be cancelled");
        app.update(press(KeyCode::Down), Instant::now());
        app.update(press(KeyCode::Char('c')), Instant::now());
        assert_eq!(app.popup, Some(Popup::ConfirmCancel("b".into())));
        assert!(
            app.update(press(KeyCode::Char('n')), Instant::now())
                .is_empty()
        );
        assert!(app.popup.is_none());
        app.update(press(KeyCode::Char('c')), Instant::now());
        assert_eq!(
            app.update(press(KeyCode::Char('y')), Instant::now()),
            vec![Action::Cancel("b".into())]
        );
    }

    #[test]
    fn letters_type_into_the_prompt_but_command_elsewhere() {
        let mut app = app();
        assert!(
            app.update(press(KeyCode::Char('q')), Instant::now())
                .is_empty()
        );
        assert_eq!(app.input, "q");
        app.update(press(KeyCode::Tab), Instant::now());
        assert_eq!(app.focus, Focus::Sessions);
        app.update(press(KeyCode::Char('a')), Instant::now());
        app.update(press(KeyCode::Down), Instant::now());
        app.update(press(KeyCode::Enter), Instant::now());
        assert_eq!(app.agent, "reader");
        assert_eq!(
            app.update(press(KeyCode::Char('q')), Instant::now()),
            vec![Action::Quit]
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.focus = Focus::Input;
        assert_eq!(
            app.update(Msg::Key(ctrl_c), Instant::now()),
            vec![Action::Quit]
        );
    }
}
