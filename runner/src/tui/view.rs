//! Draws the `App`: sessions and runs on the left, the conversation and the
//! prompt on the right, and a status bar along the bottom.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block as Border, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::app::{App, Block, Focus, Outcome, Popup, Turn};
use crate::ui::{agent_details, size, thinking_line};

const SIDEBAR_WIDTH: u16 = 30;
/// The most lines the prompt box grows to before it scrolls.
const INPUT_ROWS: usize = 6;

const HELP: &[(&str, &str)] = &[
    ("Tab / Shift-Tab", "move between panes"),
    ("Enter", "send the prompt · open a session"),
    ("Esc", "leave the prompt box"),
    ("PgUp / PgDn", "scroll the conversation (End follows)"),
    ("↑ / ↓", "move in lists · scroll the conversation"),
    ("n", "new session"),
    ("a", "choose the agent for the next turns"),
    ("c", "cancel the selected run (Runs pane)"),
    ("q / Ctrl-C", "quit; runs keep going in the daemon"),
];

pub fn draw(frame: &mut Frame, app: &mut App, now: Instant) {
    let [main, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    let [side, right] =
        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)]).areas(main);
    let [sessions, runs] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(side);
    let input_width = right.width.saturating_sub(2).max(1) as usize;
    let (input_lines, (cursor_row, cursor_col)) = wrap_input(&app.input, app.cursor, input_width);
    let rows = input_lines.len().max(cursor_row + 1);
    let [conversation, input] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(rows.min(INPUT_ROWS) as u16 + 2),
    ])
    .areas(right);

    draw_sessions(frame, app, sessions);
    draw_runs(frame, app, runs);
    draw_conversation(frame, app, conversation, now);

    let top = cursor_row.saturating_sub(INPUT_ROWS - 1);
    let text = if app.input.is_empty() {
        Text::from(Line::from("type a prompt…".dim()))
    } else {
        Text::from(input_lines.into_iter().map(Line::from).collect::<Vec<_>>())
    };
    frame.render_widget(
        Paragraph::new(text).scroll((top as u16, 0)).block(pane(
            format!(" Prompt · agent {} ", app.agent),
            app.focus == Focus::Input,
        )),
        input,
    );
    if app.focus == Focus::Input && app.popup.is_none() {
        frame.set_cursor_position((
            input.x + 1 + cursor_col as u16,
            input.y + 1 + (cursor_row - top) as u16,
        ));
    }

    let bar = match &app.status {
        Some((true, message)) => Line::from(message.as_str().red()),
        Some((false, message)) => Line::from(message.as_str()),
        None => {
            let details = app
                .agents
                .iter()
                .find(|agent| agent.name == app.agent)
                .map_or_else(|| format!("agent {}", app.agent), agent_details);
            Line::from(vec![
                details.dim(),
                "  ·  ? help  ·  Tab panes  ·  Ctrl-C quit".dim(),
            ])
        }
    };
    frame.render_widget(Paragraph::new(bar), status);

    if let Some(popup) = &app.popup {
        draw_popup(frame, app, popup);
    }
}

fn pane(title: String, focused: bool) -> Border<'static> {
    let border = Border::bordered().title(title);
    if focused {
        border.border_style(Style::new().cyan())
    } else {
        border.border_style(Style::new().dark_gray())
    }
}

fn draw_sessions(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<_> = app
        .sessions
        .iter()
        .map(|session| {
            let mut spans = vec![if session.id == app.session {
                session.id.as_str().bold()
            } else {
                session.id.as_str().into()
            }];
            if session.running_runs > 0 {
                spans.push(format!(" ▶{}", session.running_runs).cyan());
            }
            if session.queued_runs > 0 {
                spans.push(format!(" ⏸{}", session.queued_runs).yellow());
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let focused = app.focus == Focus::Sessions;
    let list = List::new(items)
        .block(pane(" Sessions ".into(), focused))
        .highlight_symbol("▸ ")
        .highlight_style(if focused {
            Style::new().reversed()
        } else {
            Style::new()
        });
    let mut state = ListState::default().with_selected(Some(app.session_cursor));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_runs(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<_> = app
        .turns
        .iter()
        .map(|turn| {
            let run = &turn.run;
            let status = match run.status.as_str() {
                "completed" => run.status.as_str().dim(),
                "running" => run.status.as_str().cyan(),
                "queued" => run.status.as_str().yellow(),
                _ => run.status.as_str().red(),
            };
            ListItem::new(Line::from(vec![
                format!("{} ", short(&run.id)).into(),
                status,
                format!(" {}", run.agent).dim(),
            ]))
        })
        .collect();
    let focused = app.focus == Focus::Runs;
    let list = List::new(items)
        .block(pane(" Runs ".into(), focused))
        .highlight_symbol("▸ ")
        .highlight_style(if focused {
            Style::new().reversed()
        } else {
            Style::new()
        });
    let mut state =
        ListState::default().with_selected((!app.turns.is_empty()).then_some(app.run_cursor));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_conversation(frame: &mut Frame, app: &mut App, area: Rect, now: Instant) {
    let mut lines = Vec::new();
    for (index, turn) in app.turns.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        turn_lines(turn, now, &mut lines);
    }
    if lines.is_empty() {
        lines.push(Line::from(
            "No turns yet. Type a prompt below and press Enter.".dim(),
        ));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let height = area.height.saturating_sub(2) as usize;
    let total = paragraph.line_count(area.width.saturating_sub(2));
    let max = total.saturating_sub(height).min(u16::MAX as usize) as u16;
    // Clamped here, where the height is known, so scrolling back down never
    // has to work through lines past the top.
    app.scroll = app.scroll.min(max);
    let mut title = format!(" Conversation · session {} ", app.session);
    if app.scroll > 0 {
        title.push_str("· End to follow ");
    }
    frame.render_widget(
        paragraph
            .scroll((max - app.scroll, 0))
            .block(pane(title, app.focus == Focus::Conversation)),
        area,
    );
}

/// The prompt, then what the run has done so far.
fn turn_lines(turn: &Turn, now: Instant, lines: &mut Vec<Line<'static>>) {
    for (index, line) in turn.run.prompt.lines().enumerate() {
        let marker = if index == 0 { "> " } else { "  " };
        lines.push(Line::from(vec![
            marker.bold().cyan(),
            line.to_string().bold(),
        ]));
    }
    let active = !turn.run.terminal();
    for block in &turn.blocks {
        match block {
            Block::Text(text) => {
                lines.extend(text.lines().map(|line| Line::from(line.to_string())))
            }
            Block::Dim(text) => lines.push(Line::from(text.clone().dim())),
            Block::Tool { call, outcome } => {
                let result = match outcome {
                    Outcome::Running if active => " …".dim(),
                    Outcome::Running => "".into(),
                    Outcome::Ok { secs, bytes } => {
                        format!(" ✓ {secs:.1}s · {}", size(*bytes)).dim()
                    }
                    Outcome::Failed(error) => format!(" ✗ {error}").red(),
                };
                lines.push(Line::from(vec![call.clone().dim(), result]));
            }
        }
    }
    if active && turn.queued {
        lines.push(Line::from("Queued — waiting to run".dim()));
    }
    if let Some(elapsed) = turn.thinking(now) {
        lines.push(Line::from(thinking_line(elapsed).dim()));
    }
    if turn.run.terminal() && turn.run.status != "completed" {
        let what = match (turn.run.status.as_str(), turn.run.error.as_deref()) {
            (status, Some(error)) => format!("{status}: {error}"),
            (status, None) => status.to_string(),
        };
        lines.push(Line::from(
            format!("{what} · not saved to the conversation").red(),
        ));
    }
}

/// The input hard-wrapped to `width`, and the cursor's row and column in it.
fn wrap_input(input: &str, cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let mut lines = Vec::new();
    for line in input.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            lines.push(String::new());
        }
        lines.extend(chars.chunks(width).map(|chunk| chunk.iter().collect()));
    }
    let (mut row, mut col) = (0, 0);
    for c in input.chars().take(cursor) {
        if c == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
            if col == width {
                row += 1;
                col = 0;
            }
        }
    }
    (lines, (row, col))
}

fn draw_popup(frame: &mut Frame, app: &App, popup: &Popup) {
    let (title, lines): (&str, Vec<Line>) = match popup {
        Popup::Help => (
            " Keys ",
            HELP.iter()
                .map(|(keys, what)| Line::from(vec![format!("{keys:<16}").bold(), (*what).into()]))
                .collect(),
        ),
        Popup::Agents(selected) => (
            " Agent for the next turns ",
            app.agents
                .iter()
                .enumerate()
                .flat_map(|(index, agent)| {
                    let name = if index == *selected {
                        format!("▸ {}", agent.name).reversed()
                    } else {
                        format!("  {}", agent.name).into()
                    };
                    [
                        Line::from(name),
                        Line::from(format!("    {}", agent_details(agent)).dim()),
                    ]
                })
                .collect(),
        ),
        Popup::NewSession(name) => (
            " New session ",
            vec![
                Line::from(vec!["name: ".dim(), name.clone().bold(), "▏".into()]),
                Line::from("Enter creates it (blank for a generated ID) · Esc cancels".dim()),
            ],
        ),
        Popup::ConfirmCancel(run_id) => (
            " Cancel run ",
            vec![
                Line::from(format!("Cancel run {run_id}?")),
                Line::from("y cancels it · any other key keeps it running".dim()),
            ],
        ),
    };
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(72);
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Border::bordered()
                .title(title)
                .border_style(Style::new().cyan()),
        ),
        rect,
    );
}

/// A run ID's first eight characters, enough to tell runs apart in a list.
fn short(id: &str) -> &str {
    id.char_indices().nth(8).map_or(id, |(i, _)| &id[..i])
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    use super::super::app::Msg;
    use super::*;
    use crate::db::{Event, Run, Session};

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, app, Instant::now()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shows_sessions_conversation_and_agent() {
        let agent = host::AgentInfo {
            name: "default".into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            tools: vec!["get_weather".into()],
            delegates: vec![],
            max_tokens: host::DEFAULT_MAX_TOKENS,
            max_model_calls: None,
        };
        let mut app = App::new(vec![agent], "default".into(), "trip".into());
        let now = Instant::now();
        app.update(
            Msg::Sessions(vec![Session {
                id: "trip".into(),
                created_at: 0,
                running_runs: 1,
                queued_runs: 0,
            }]),
            now,
        );
        let mut failed = Run {
            id: "run-0002-abcdef".into(),
            session_id: "trip".into(),
            agent: "default".into(),
            prompt: "And Sunday?".into(),
            status: "failed".into(),
            created_at: 0,
            deadline_secs: 30,
            cancel_requested: false,
            error: Some("API error 401".into()),
            parent_run_id: None,
            root_run_id: "run-0002-abcdef".into(),
            depth: 0,
            deadline_at: None,
        };
        let mut done = failed.clone();
        done.id = "run-0001-abcdef".into();
        done.prompt = "Weather in Seattle?".into();
        done.status = "completed".into();
        done.error = None;
        failed.error = Some("API error 401".into());
        app.update(Msg::Runs("trip".into(), vec![done.clone(), failed]), now);
        let event = |id, data| Event {
            id,
            run_id: done.id.clone(),
            data,
        };
        app.update(
            Msg::History {
                generation: 0,
                run_id: done.id.clone(),
                events: vec![
                    event(1, json!({"kind":"tool_started","name":"get_weather","input":r#"{"city":"Seattle"}"#})),
                    event(2, json!({"kind":"tool_finished","name":"get_weather","ok":true,"elapsed_ms":400,"bytes":100})),
                    event(3, json!({"kind":"text","text":"Rainy, 12°C."})),
                ],
            },
            now,
        );
        let screen = screen(&mut app);
        for expected in [
            "Sessions",
            "trip ▶1",
            "run-0001 completed",
            "run-0002 failed",
            "Conversation · session trip",
            "> Weather in Seattle?",
            r#"⚙ get_weather(city: "Seattle") ✓ 0.4s · 100 B"#,
            "Rainy, 12°C.",
            "failed: API error 401 · not saved to the conversation",
            "Prompt · agent default",
            "provider openai · model gpt-5 · tools get_weather",
        ] {
            assert!(
                screen.contains(expected),
                "missing {expected:?} in\n{screen}"
            );
        }
    }

    #[test]
    fn input_wraps_and_tracks_the_cursor() {
        assert_eq!(
            wrap_input("abcdef", 6, 4),
            (vec!["abcd".into(), "ef".into()], (1, 2))
        );
        assert_eq!(
            wrap_input("ab\n\ncd", 4, 10),
            (vec!["ab".into(), "".into(), "cd".into()], (2, 0))
        );
        assert_eq!(wrap_input("abcd", 4, 4).1, (1, 0));
    }
}
