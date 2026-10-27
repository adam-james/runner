//! The TUI's state, and how keys and daemon messages change it. Nothing here
//! talks to the daemon: whatever needs it comes back from `update` as an
//! `Action` for the event loop to carry out.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::db::{Event, Run, Session};
use crate::ui::{ARGS_LIMIT, delegation_label, name, token_usage_line, tool_call, truncate};

/// How far PgUp/PgDn scroll the conversation.
const PAGE: u16 = 10;

pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    /// Conversation lines to scroll up (or down, if negative), from the mouse wheel.
    Scroll(i16),
    /// Redraws the elapsed-time indicators.
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
    /// The session a prompt typed into a new session was submitted to.
    Created {
        generation: u64,
        session: Session,
    },
    Submitted(Run),
    /// Submitting failed; the prompt is handed back to the input.
    SubmitFailed {
        prompt: String,
        error: String,
    },
    Notice(String),
    Error(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Submit a turn, to a new session if `session` is `None`.
    Submit {
        session: Option<String>,
        agent: String,
        prompt: String,
        generation: u64,
    },
    Cancel(String),
    /// Stop following the old session's runs and poll this one's, if any.
    SelectSession {
        session: Option<String>,
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
    /// Usage and delegation notes.
    Dim(String),
}

/// One run of the session, with its events folded into what's shown. A run
/// followed live and one read back after it finished look the same.
pub struct Turn {
    pub run: Run,
    pub blocks: Vec<Block>,
}

impl Turn {
    fn new(run: Run) -> Self {
        Self {
            run,
            blocks: Vec::new(),
        }
    }

    fn apply(&mut self, data: &Value) {
        match data["kind"].as_str() {
            Some("delegated" | "delegation_waiting" | "delegation_finished") => {
                self.blocks.push(Block::Dim(delegation_label(data)));
            }
            Some("status") => {
                // Events arrive in order, so the latest says where the run is,
                // ahead of the next listing.
                if let Some(status) = data["status"].as_str() {
                    self.run.status = status.to_string();
                }
            }
            Some("text") => {
                let text = data["text"].as_str().unwrap_or_default();
                match self.blocks.last_mut() {
                    Some(Block::Text(reply)) => reply.push_str(text),
                    _ => self.blocks.push(Block::Text(text.to_string())),
                }
            }
            Some("usage") => {
                if let Some(line) = token_usage_line(data) {
                    self.blocks.push(Block::Dim(line));
                }
            }
            Some("tool_started") => self.blocks.push(Block::Tool {
                call: tool_call(name(data), data["input"].as_str().unwrap_or_default()),
                outcome: Outcome::Running,
            }),
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
            }
            _ => {}
        }
    }
}

pub struct App {
    pub agents: Vec<host::AgentInfo>,
    /// The agent the next submitted turn runs.
    pub agent: String,
    /// The session shown and submitted to; `None` for a new one, created by
    /// its first prompt.
    pub session: Option<String>,
    /// Sessions clients created, most recently active first.
    pub sessions: Vec<Session>,
    /// The highlighted session in the Sessions pane.
    pub session_cursor: Option<String>,
    /// The shown session's runs, in submission order.
    pub turns: Vec<Turn>,
    pub run_cursor: usize,
    pub focus: Focus,
    pub input: String,
    /// The input cursor, in chars.
    pub cursor: usize,
    /// Conversation lines scrolled up from the bottom; 0 follows new output.
    pub scroll: u16,
    /// The conversation pane's width, height and furthest scroll when last
    /// drawn, so lines that arrive while scrolled up don't move the view.
    pub layout: (u16, u16, u16),
    pub popup: Option<Popup>,
    /// A notice, or an error if `true`, shown in the status bar until the next key.
    pub status: Option<(bool, String)>,
    /// A new session's first prompt is being submitted.
    creating: bool,
    /// Bumped on every session switch, so messages for the old session's
    /// runs that are still in flight get dropped.
    generation: u64,
}

impl App {
    pub fn new(agents: Vec<host::AgentInfo>, agent: String, session: Option<String>) -> Self {
        Self {
            agents,
            agent,
            session_cursor: session.clone(),
            session,
            sessions: Vec::new(),
            turns: Vec::new(),
            run_cursor: 0,
            focus: Focus::Input,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            layout: (0, 0, 0),
            popup: None,
            status: None,
            creating: false,
            generation: 0,
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        match msg {
            Msg::Key(key) => return self.key(key),
            Msg::Paste(text) => {
                if self.popup.is_none() {
                    self.focus = Focus::Input;
                    self.insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
                }
            }
            Msg::Scroll(lines) => self.scroll = self.scroll.saturating_add_signed(lines),
            Msg::Tick => {}
            Msg::Sessions(sessions) => self.sessions(sessions),
            Msg::Runs(session, runs) => return self.runs(session, runs),
            Msg::History {
                generation,
                run_id,
                events,
            } => {
                if let Some(turn) = self.turn(generation, &run_id) {
                    for event in events {
                        turn.apply(&event.data);
                    }
                }
            }
            Msg::RunEvent {
                generation,
                run_id,
                data,
            } => {
                if let Some(turn) = self.turn(generation, &run_id) {
                    turn.apply(&data);
                }
            }
            Msg::RunEnded { generation, run } => {
                if let Some(turn) = self.turn(generation, &run.id.clone()) {
                    turn.run = run;
                }
            }
            Msg::Created {
                generation,
                session,
            } => {
                self.creating = false;
                let id = session.id.clone();
                if !self.sessions.iter().any(|known| known.id == id) {
                    self.sessions.insert(0, session);
                }
                // Unless the user has moved on, the new session is the one shown.
                if generation == self.generation && self.session.is_none() {
                    self.session = Some(id.clone());
                    self.session_cursor = Some(id.clone());
                    return vec![Action::SelectSession {
                        session: Some(id),
                        generation,
                    }];
                }
            }
            Msg::Submitted(run) => {
                if self.session.as_deref() == Some(run.session_id.as_str())
                    && !self.turns.iter().any(|t| t.run.id == run.id)
                {
                    let action = Action::Watch {
                        run_id: run.id.clone(),
                        generation: self.generation,
                    };
                    self.turns.push(Turn::new(run));
                    return vec![action];
                }
            }
            Msg::SubmitFailed { prompt, error } => {
                self.creating = false;
                if self.input.is_empty() {
                    self.input = prompt;
                    self.cursor = self.input.chars().count();
                }
                self.status = Some((true, error));
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

    /// Takes in a fresh session listing: delegated children's sessions left
    /// out, unless one is shown, and the most recently active first.
    fn sessions(&mut self, mut sessions: Vec<Session>) {
        sessions.retain(|session| !session.delegated || self.session.as_ref() == Some(&session.id));
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        if let Some(owner) = sessions
            .iter()
            .find(|s| Some(&s.id) == self.session.as_ref())
            .map(|s| &s.agent)
        {
            self.agent = owner.clone();
        }
        self.sessions = sessions;
        if self.focus != Focus::Sessions {
            self.session_cursor = self.session.clone();
        }
    }

    /// Takes in a fresh listing of the session's runs, starting to follow or
    /// read any it hasn't seen.
    fn runs(&mut self, session: String, runs: Vec<Run>) -> Vec<Action> {
        if self.session.as_ref() != Some(&session) {
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
                    self.turns.push(Turn::new(run.clone()));
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

    /// Shows `session` instead of the current one: a new session if `None`.
    fn select(&mut self, session: Option<String>) -> Action {
        if let Some(owner) = self
            .sessions
            .iter()
            .find(|s| Some(&s.id) == session.as_ref())
            .map(|s| &s.agent)
        {
            self.agent = owner.clone();
        }
        self.session_cursor = session.clone();
        self.session = session.clone();
        self.turns.clear();
        self.run_cursor = 0;
        self.scroll = 0;
        self.focus = Focus::Input;
        self.creating = false;
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
            KeyCode::Char('n') if ctrl => return vec![self.select(None)],
            KeyCode::Char('a') if ctrl => self.open_agents(),
            KeyCode::Char('o') if ctrl => self.focus_sessions(),
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
        match FOCUS_ORDER[(index + step) % FOCUS_ORDER.len()] {
            Focus::Sessions => self.focus_sessions(),
            focus => self.focus = focus,
        }
    }

    fn focus_sessions(&mut self) {
        self.focus = Focus::Sessions;
        let highlighted = self
            .session_cursor
            .as_ref()
            .is_some_and(|id| self.sessions.iter().any(|s| &s.id == id));
        if !highlighted {
            self.session_cursor = self.sessions.first().map(|s| s.id.clone());
        }
    }

    fn open_agents(&mut self) {
        let index = self
            .agents
            .iter()
            .position(|a| a.name == self.agent)
            .unwrap_or(0);
        self.popup = Some(Popup::Agents(index));
    }

    /// Moves the Sessions highlight by `step` rows.
    fn move_session_cursor(&mut self, step: isize) {
        let current = self
            .session_cursor
            .as_ref()
            .and_then(|id| self.sessions.iter().position(|s| &s.id == id));
        let index = match current {
            Some(index) => index
                .saturating_add_signed(step)
                .min(self.sessions.len().saturating_sub(1)),
            None => 0,
        };
        self.session_cursor = self.sessions.get(index).map(|s| s.id.clone());
    }

    fn input_key(&mut self, key: KeyEvent) -> Vec<Action> {
        match key.code {
            KeyCode::Enter => {
                let prompt = self.input.trim().to_string();
                if prompt.is_empty() {
                    return Vec::new();
                }
                if self.creating {
                    self.status = Some((false, "creating the session…".into()));
                    return Vec::new();
                }
                self.creating = self.session.is_none();
                self.input.clear();
                self.cursor = 0;
                self.scroll = 0;
                return vec![Action::Submit {
                    session: self.session.clone(),
                    agent: self.agent.clone(),
                    prompt,
                    generation: self.generation,
                }];
            }
            KeyCode::Esc => self.focus = Focus::Conversation,
            KeyCode::Up => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
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
            KeyCode::Char('n') => return vec![self.select(None)],
            KeyCode::Char('a') => self.open_agents(),
            KeyCode::Char('i') | KeyCode::Esc => self.focus = Focus::Input,
            KeyCode::End => self.scroll = 0,
            KeyCode::Home => self.scroll = u16::MAX,
            KeyCode::Up => match self.focus {
                Focus::Sessions => self.move_session_cursor(-1),
                Focus::Runs => self.run_cursor = self.run_cursor.saturating_sub(1),
                _ => self.scroll = self.scroll.saturating_add(1),
            },
            KeyCode::Down => match self.focus {
                Focus::Sessions => self.move_session_cursor(1),
                Focus::Runs => {
                    self.run_cursor = (self.run_cursor + 1).min(self.turns.len().saturating_sub(1))
                }
                _ => self.scroll = self.scroll.saturating_sub(1),
            },
            KeyCode::Enter => match (self.focus, self.session_cursor.clone()) {
                (Focus::Sessions, Some(id)) if self.session.as_ref() != Some(&id) => {
                    return vec![self.select(Some(id))];
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
                        let action = self.select(None);
                        self.status =
                            Some((false, format!("new session with agent {}", self.agent)));
                        return vec![action];
                    }
                }
                KeyCode::Esc => {}
                _ => self.popup = Some(Popup::Agents(index)),
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

    fn session(id: &str, updated_at: i64) -> Session {
        Session {
            id: id.into(),
            agent: "default".into(),
            updated_at,
            ..Session::default()
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
            tool_access: Default::default(),
        }
    }

    fn app(session: Option<&str>) -> App {
        App::new(
            vec![agent("default"), agent("reader")],
            "default".into(),
            session.map(String::from),
        )
    }

    fn press(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent::from(code))
    }

    fn ctrl(c: char) -> Msg {
        Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.update(press(KeyCode::Char(c)));
        }
    }

    #[test]
    fn events_fold_into_tool_calls_text_and_status() {
        let mut turn = Turn::new(run("r", "queued"));
        for data in [
            json!({"kind":"status","status":"running"}),
            json!({"kind":"tool_started","name":"get_weather","input":r#"{"city":"Oslo"}"#}),
            json!({"kind":"tool_finished","name":"get_weather","ok":true,"elapsed_ms":400,"bytes":2048}),
            json!({"kind":"text","text":"Sunny"}),
            json!({"kind":"text","text":" today"}),
            json!({"kind":"status","status":"completed"}),
        ] {
            turn.apply(&data);
        }
        assert_eq!(turn.run.status, "completed");
        assert_eq!(
            turn.blocks,
            vec![
                Block::Tool {
                    call: r#"⚙ get_weather(city: "Oslo")"#.into(),
                    outcome: Outcome::Ok {
                        secs: 0.4,
                        bytes: 2048
                    },
                },
                Block::Text("Sunny today".into()),
            ]
        );
    }

    #[test]
    fn a_replay_tolerates_a_missing_tool_start() {
        let mut turn = Turn::new(run("r", "completed"));
        for data in [
            json!({"kind":"tool_finished","name":"search","ok":false,"error":"boom"}),
            json!({"kind":"delegated","agent":"helper","run_id":"c1"}),
            json!({"kind":"text","text":"done"}),
        ] {
            turn.apply(&data);
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
        let mut app = app(Some("s"));
        let actions = app.update(Msg::Runs(
            "s".into(),
            vec![run("a", "completed"), run("b", "running")],
        ));
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
        let again = app.update(Msg::Runs(
            "s".into(),
            vec![run("a", "completed"), run("b", "completed")],
        ));
        assert!(again.is_empty());
        assert_eq!(app.turns[1].run.status, "completed");
        // Another session's listing is stale.
        assert!(
            app.update(Msg::Runs("t".into(), vec![run("c", "queued")]))
                .is_empty()
        );
    }

    #[test]
    fn sessions_list_most_recent_first_without_delegated_children() {
        let mut app = app(Some("old"));
        let mut child = session("child", 5);
        child.delegated = true;
        app.update(Msg::Sessions(vec![
            session("old", 1),
            session("new", 3),
            child,
        ]));
        let ids: Vec<_> = app.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["new", "old"]);
        // The highlight stays on the open session, wherever it sorts.
        assert_eq!(app.session_cursor.as_deref(), Some("old"));
    }

    #[test]
    fn opening_a_past_session_drops_the_old_sessions_messages() {
        let mut app = app(Some("s"));
        app.update(Msg::Runs("s".into(), vec![run("a", "running")]));
        app.update(Msg::Sessions(vec![session("s", 2), session("t", 1)]));
        app.update(ctrl('o'));
        assert_eq!(app.focus, Focus::Sessions);
        app.update(press(KeyCode::Down));
        let actions = app.update(press(KeyCode::Enter));
        assert_eq!(
            actions,
            vec![Action::SelectSession {
                session: Some("t".into()),
                generation: 1
            }]
        );
        assert_eq!(app.focus, Focus::Input);
        assert!(app.turns.is_empty());
        app.update(Msg::RunEvent {
            generation: 0,
            run_id: "a".into(),
            data: json!({"kind":"text","text":"x"}),
        });
        assert!(app.turns.is_empty());
    }

    #[test]
    fn a_new_sessions_first_prompt_creates_it() {
        let mut app = app(Some("busy"));
        assert_eq!(
            app.update(ctrl('n')),
            vec![Action::SelectSession {
                session: None,
                generation: 1
            }]
        );
        type_text(&mut app, "second task");
        let actions = app.update(press(KeyCode::Enter));
        assert_eq!(
            actions,
            vec![Action::Submit {
                session: None,
                agent: "default".into(),
                prompt: "second task".into(),
                generation: 1,
            }]
        );
        // A second Enter can't create another session meanwhile.
        type_text(&mut app, "more");
        assert!(app.update(press(KeyCode::Enter)).is_empty());
        assert_eq!(app.input, "more");
        let actions = app.update(Msg::Created {
            generation: 1,
            session: session("fresh", 9),
        });
        assert_eq!(
            actions,
            vec![Action::SelectSession {
                session: Some("fresh".into()),
                generation: 1
            }]
        );
        let mut submitted = run("r", "queued");
        submitted.session_id = "fresh".into();
        assert_eq!(
            app.update(Msg::Submitted(submitted)),
            vec![Action::Watch {
                run_id: "r".into(),
                generation: 1
            }]
        );
        assert_eq!(app.sessions[0].id, "fresh");
    }

    #[test]
    fn a_new_session_created_after_moving_on_stays_in_the_background() {
        let mut app = app(None);
        type_text(&mut app, "task");
        app.update(press(KeyCode::Enter));
        app.update(Msg::Sessions(vec![session("other", 1)]));
        app.update(ctrl('o'));
        app.update(press(KeyCode::Enter));
        assert_eq!(app.session.as_deref(), Some("other"));
        assert!(
            app.update(Msg::Created {
                generation: 0,
                session: session("fresh", 2)
            })
            .is_empty()
        );
        assert_eq!(app.session.as_deref(), Some("other"));
        assert!(app.sessions.iter().any(|s| s.id == "fresh"));
    }

    #[test]
    fn enter_submits_trimmed_input_and_a_failure_restores_it() {
        let mut app = app(Some("s"));
        assert!(app.update(press(KeyCode::Enter)).is_empty());
        type_text(&mut app, " hi ");
        app.update(press(KeyCode::Left));
        app.update(press(KeyCode::Backspace));
        assert_eq!(app.input, " h ");
        let actions = app.update(press(KeyCode::Enter));
        assert_eq!(
            actions,
            vec![Action::Submit {
                session: Some("s".into()),
                agent: "default".into(),
                prompt: "h".into(),
                generation: 0,
            }]
        );
        assert!(app.input.is_empty());
        app.update(Msg::SubmitFailed {
            prompt: "h".into(),
            error: "daemon down".into(),
        });
        assert_eq!(app.input, "h");
        assert_eq!(app.status, Some((true, "daemon down".into())));
    }

    #[test]
    fn the_prompt_box_scrolls_the_conversation() {
        let mut app = app(Some("s"));
        app.update(press(KeyCode::Up));
        app.update(press(KeyCode::PageUp));
        app.update(Msg::Scroll(3));
        assert_eq!(app.scroll, 1 + PAGE + 3);
        app.update(Msg::Scroll(-100));
        assert_eq!(app.scroll, 0);
        assert!(app.input.is_empty());
    }

    #[test]
    fn cancelling_a_run_needs_confirmation() {
        let mut app = app(Some("s"));
        app.update(Msg::Runs(
            "s".into(),
            vec![run("a", "completed"), run("b", "running")],
        ));
        app.focus = Focus::Runs;
        app.update(press(KeyCode::Char('c')));
        assert!(app.popup.is_none(), "a finished run can't be cancelled");
        app.update(press(KeyCode::Down));
        app.update(press(KeyCode::Char('c')));
        assert_eq!(app.popup, Some(Popup::ConfirmCancel("b".into())));
        assert!(app.update(press(KeyCode::Char('n'))).is_empty());
        assert!(app.popup.is_none());
        app.update(press(KeyCode::Char('c')));
        assert_eq!(
            app.update(press(KeyCode::Char('y'))),
            vec![Action::Cancel("b".into())]
        );
    }

    #[test]
    fn selecting_a_session_restores_its_locked_agent() {
        let mut app = app(None);
        let mut saved = session("saved", 1);
        saved.agent = "reader".into();
        app.update(Msg::Sessions(vec![saved]));
        app.select(Some("saved".into()));
        assert_eq!(app.agent, "reader");
    }

    #[test]
    fn the_agent_can_be_changed_from_the_prompt_box() {
        let mut app = app(Some("s"));
        app.update(press(KeyCode::Char('a')));
        assert_eq!(app.input, "a", "plain letters type into the prompt");
        app.update(ctrl('a'));
        app.update(press(KeyCode::Down));
        app.update(press(KeyCode::Enter));
        assert_eq!(app.agent, "reader");
        assert!(app.session.is_none());
        assert!(app.turns.is_empty());
        assert_eq!(app.input, "a");
        assert_eq!(app.update(ctrl('c')), vec![Action::Quit]);
        app.update(press(KeyCode::Tab));
        assert_eq!(app.update(press(KeyCode::Char('q'))), vec![Action::Quit]);
    }
}
