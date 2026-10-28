//! Draws the `App`: sessions and runs on the left, the conversation and the
//! prompt on the right, and a status bar along the bottom.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block as Border, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use super::app::{App, Block, Focus, Outcome, Popup, Turn};
use crate::db::{Run, Session};
use crate::ui::{agent_details, size};

const SIDEBAR_WIDTH: u16 = 32;
/// The most lines the prompt box grows to before it scrolls.
const INPUT_ROWS: usize = 6;

const HELP: &[(&str, &str)] = &[
    ("Enter", "send the prompt · open the highlighted session"),
    ("Ctrl-N", "new session, to run something alongside this one"),
    ("Ctrl-O", "pick a past session (↑/↓, Enter)"),
    ("Ctrl-A", "new session with another agent"),
    ("↑/↓ PgUp/PgDn", "scroll the conversation"),
    ("Drag to select", "copy with your terminal’s usual shortcut"),
    ("Paste", "use your terminal’s usual shortcut in the prompt"),
    ("Home / End", "top of the conversation / follow new output"),
    ("Tab / Shift-Tab", "move between panes"),
    ("Esc", "leave the prompt box, for one-letter keys:"),
    ("  n a q", "new session · agent · quit"),
    ("  c", "cancel the highlighted run (Runs pane)"),
    ("Ctrl-C", "quit; runs keep going in the daemon"),
];

/// Draws everything, with `now` as the wall-clock time in milliseconds.
pub fn draw(frame: &mut Frame, app: &mut App, now: i64) {
    let [main, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    let [side, right] =
        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)]).areas(main);
    let [sessions, runs] =
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(side);
    let input_width = right.width.saturating_sub(2).max(1) as usize;
    let (input_lines, (cursor_row, cursor_col)) = wrap_input(&app.input, app.cursor, input_width);
    let rows = input_lines.len().max(cursor_row + 1);
    let [conversation, input] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(rows.min(INPUT_ROWS) as u16 + 2),
    ])
    .areas(right);

    draw_sessions(frame, app, sessions, now);
    draw_runs(frame, app, runs, now);
    draw_conversation(frame, app, conversation, now);

    let top = cursor_row.saturating_sub(INPUT_ROWS - 1);
    let text = if app.input.is_empty() {
        let hint = if app.session.is_none() {
            "type a prompt to start a new session…"
        } else {
            "type a prompt…"
        };
        Text::from(Line::from(hint.dim()))
    } else {
        Text::from(input_lines.into_iter().map(Line::from).collect::<Vec<_>>())
    };
    frame.render_widget(
        Paragraph::new(text).scroll((top as u16, 0)).block(pane(
            format!(" Prompt · agent {} · Ctrl-A to change ", app.agent),
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
                "  ·  ^N new · ^O sessions · ^A agent · ? help".dim(),
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

/// `▶` running, `⏸` waiting, `✓` or `✗` for how the latest run ended.
fn session_glyph(session: &Session) -> Span<'static> {
    if session.running_runs > 0 {
        "▶ ".cyan()
    } else if session.queued_runs > 0 {
        "⏸ ".yellow()
    } else {
        match session.last_status.as_deref() {
            Some("completed") => "✓ ".green(),
            Some(_) => "✗ ".red(),
            None => "· ".dim(),
        }
    }
}

fn draw_sessions(frame: &mut Frame, app: &App, area: Rect, now: i64) {
    let items: Vec<_> = app
        .sessions
        .iter()
        .map(|session| {
            let label = session.title.clone().unwrap_or_else(|| session.id.clone());
            let label = if app.session.as_ref() == Some(&session.id) {
                label.bold()
            } else {
                label.into()
            };
            let mut spans = vec![session_glyph(session), label];
            if session.queued_runs > 0 {
                spans.push(format!(" +{} queued", session.queued_runs).yellow());
            }
            spans.push(format!(" {}", age(now - session.updated_at)).dim());
            ListItem::new(Line::from(spans))
        })
        .collect();
    let focused = app.focus == Focus::Sessions;
    let title = if app.session.is_none() {
        " Sessions · on a new one "
    } else {
        " Sessions "
    };
    let list = List::new(items)
        .block(pane(title.into(), focused))
        .highlight_symbol("▸")
        .highlight_style(if focused {
            Style::new().reversed()
        } else {
            Style::new()
        });
    let selected = app
        .session_cursor
        .as_ref()
        .and_then(|id| app.sessions.iter().position(|s| &s.id == id));
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_runs(frame: &mut Frame, app: &App, area: Rect, now: i64) {
    let items: Vec<_> = app
        .turns
        .iter()
        .map(|turn| {
            let run = &turn.run;
            let status = match run.status.as_str() {
                "completed" => run.status.clone().dim(),
                "running" => match started(run) {
                    Some(start) => format!("running {}", elapsed(now - start)).cyan(),
                    None => run.status.clone().cyan(),
                },
                "queued" => run.status.clone().yellow(),
                _ => run.status.clone().red(),
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
        .highlight_symbol("▸")
        .highlight_style(if focused {
            Style::new().reversed()
        } else {
            Style::new()
        });
    let mut state =
        ListState::default().with_selected((!app.turns.is_empty()).then_some(app.run_cursor));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_conversation(frame: &mut Frame, app: &mut App, area: Rect, now: i64) {
    let mut lines = Vec::new();
    for (index, turn) in app.turns.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        turn_lines(turn, now, &mut lines);
    }
    if lines.is_empty() {
        let hint = if app.session.is_none() {
            "New session. Its first prompt creates it; Ctrl-O opens a past one."
        } else {
            "No turns yet. Type a prompt below and press Enter."
        };
        lines.push(Line::from(hint.dim()));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let height = area.height.saturating_sub(2) as usize;
    let total = paragraph.line_count(area.width.saturating_sub(2));
    let max = total.saturating_sub(height).min(u16::MAX as usize) as u16;
    // `scroll` counts from the bottom, so while scrolled up, new lines would
    // push what's being read off the top; scroll up by as many to hold it.
    // Only for the same pane size: a resize rewraps everything anyway.
    let (width, last_height, last_max) = app.layout;
    if app.scroll > 0 && (width, last_height) == (area.width, area.height) {
        app.scroll = app.scroll.saturating_add(max.saturating_sub(last_max));
    }
    app.layout = (area.width, area.height, max);
    // Clamped here, where the height is known, so scrolling back down never
    // has to work through lines past the top.
    app.scroll = app.scroll.min(max);
    let session = app.session.as_deref().unwrap_or("new");
    let mut title = format!(" Conversation · session {session} ");
    if app.scroll > 0 {
        title.push_str(&format!("· ↑{} · End to follow ", app.scroll));
    }
    frame.render_widget(
        paragraph
            .scroll((max - app.scroll, 0))
            .block(pane(title, app.focus == Focus::Conversation)),
        area,
    );
    if max > 0 {
        let mut state = ScrollbarState::new(max as usize).position((max - app.scroll) as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

/// The prompt, then what the run has done so far.
fn turn_lines(turn: &Turn, now: i64, lines: &mut Vec<Line<'static>>) {
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
    match turn.run.status.as_str() {
        "queued" => lines.push(Line::from("Queued — waiting to run".dim())),
        "running" => lines.push(Line::from(
            match started(&turn.run) {
                Some(start) => format!("Working… {}", elapsed(now - start)),
                None => "Working…".to_string(),
            }
            .dim(),
        )),
        "completed" => {}
        status => {
            let what = match turn.run.error.as_deref() {
                Some(error) => format!("{status}: {error}"),
                None => status.to_string(),
            };
            lines.push(Line::from(
                format!("{what} · not saved to the conversation").red(),
            ));
        }
    }
}

/// When a run started executing, in milliseconds: its deadline, set on
/// claim, less its length. Children's deadlines are capped by their
/// parent's, so theirs can't say.
fn started(run: &Run) -> Option<i64> {
    let deadline = run.deadline_at?;
    (run.depth == 0).then(|| deadline - run.deadline_secs as i64 * 1000)
}

/// `4.2s`, or `3m 05s` past a minute.
fn elapsed(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m {:02}s", ms / 60_000, ms / 1000 % 60)
    }
}

/// How long ago, roughly: `now`, `5m`, `3h`, `2d`.
fn age(ms: i64) -> String {
    match ms.max(0) / 1000 {
        secs @ 0..60 => {
            if secs < 10 {
                "now".into()
            } else {
                format!("{secs}s")
            }
        }
        secs @ 60..3600 => format!("{}m", secs / 60),
        secs @ 3600..86400 => format!("{}h", secs / 3600),
        secs => format!("{}d", secs / 86400),
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
                .map(|(keys, what)| Line::from(vec![format!("{keys:<17}").bold(), (*what).into()]))
                .collect(),
        ),
        Popup::Agents(selected) => (
            " New session agent · Enter picks, Esc keeps ",
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
        Popup::ConfirmCancel(run_id) => (
            " Cancel run ",
            vec![
                Line::from(format!("Cancel run {run_id}?")),
                Line::from("y cancels it · any other key keeps it running".dim()),
            ],
        ),
    };
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(76);
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
    use crate::db::Event;

    fn screen(app: &mut App, now: i64) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal.draw(|frame| draw(frame, app, now)).unwrap();
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

    fn run(id: &str, prompt: &str, status: &str) -> Run {
        Run {
            id: id.into(),
            session_id: "trip".into(),
            agent: "default".into(),
            prompt: prompt.into(),
            status: status.into(),
            created_at: 0,
            deadline_secs: 600,
            cancel_requested: false,
            error: None,
            parent_run_id: None,
            root_run_id: id.into(),
            depth: 0,
            deadline_at: None,
        }
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
            version: String::new(),
            tool_access: Default::default(),
        };
        let mut app = App::new(vec![agent], "default".into(), Some("trip".into()));
        app.update(Msg::Sessions(vec![
            Session {
                id: "trip".into(),
                agent: "default".into(),
                running_runs: 1,
                title: Some("Weather in Seattle?".into()),
                updated_at: 2,
                ..Session::default()
            },
            Session {
                id: "older".into(),
                agent: "default".into(),
                title: Some("Plan a weekend".into()),
                last_status: Some("failed".into()),
                updated_at: 1,
                ..Session::default()
            },
        ]));
        let done = run("run-0001-abcdef", "Weather in Seattle?", "completed");
        let mut failed = run("run-0002-abcdef", "And Sunday?", "failed");
        failed.error = Some("API error 401".into());
        let mut working = run("run-0003-abcdef", "And Monday?", "running");
        // Started 75 seconds before `now`.
        working.deadline_at = Some(600_000 + 1_000);
        app.update(Msg::Runs(
            "trip".into(),
            vec![done.clone(), failed, working],
        ));
        let event = |id, data| Event {
            id,
            run_id: done.id.clone(),
            data,
        };
        app.update(Msg::History {
            generation: 0,
            run_id: done.id.clone(),
            events: vec![
                event(1, json!({"kind":"tool_started","name":"get_weather","input":r#"{"city":"Seattle"}"#})),
                event(2, json!({"kind":"tool_finished","name":"get_weather","ok":true,"elapsed_ms":400,"bytes":100})),
                event(3, json!({"kind":"text","text":"Rainy, 12°C."})),
            ],
        });
        let screen = screen(&mut app, 76_000);
        for expected in [
            "▶ Weather in Seattle? 1m",
            "✗ Plan a weekend 1m",
            "run-0001 completed",
            "run-0002 failed",
            "run-0003 running 1m 15s",
            "Conversation · session trip",
            "> Weather in Seattle?",
            r#"⚙ get_weather(city: "Seattle") ✓ 0.4s · 100 B"#,
            "Rainy, 12°C.",
            "failed: API error 401 · not saved to the conversation",
            "Working… 1m 15s",
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
    fn long_conversations_scroll_and_show_where() {
        let mut app = App::new(vec![], "default".into(), Some("trip".into()));
        let runs: Vec<_> = (0..40)
            .map(|i| run(&format!("r{i:02}"), &format!("prompt {i}"), "completed"))
            .collect();
        app.update(Msg::Runs("trip".into(), runs));
        let bottom = screen(&mut app, 0);
        assert!(bottom.contains("> prompt 39") && !bottom.contains("> prompt 0 "));
        app.scroll = u16::MAX;
        let top = screen(&mut app, 0);
        assert!(top.contains("> prompt 0 "), "{top}");
        assert!(top.contains("End to follow"));
    }

    #[test]
    fn streaming_text_doesnt_move_a_scrolled_up_view() {
        let mut app = App::new(vec![], "default".into(), Some("trip".into()));
        let runs: Vec<_> = (0..40)
            .map(|i| run(&format!("r{i:02}"), &format!("prompt {i}"), "completed"))
            .chain([run("live", "streaming", "running")])
            .collect();
        app.update(Msg::Runs("trip".into(), runs));
        let stream = |app: &mut App, text: &str| {
            app.update(Msg::RunEvent {
                generation: 0,
                run_id: "live".into(),
                data: serde_json::json!({"kind":"text","text":text}),
            })
        };
        stream(&mut app, "first line");
        screen(&mut app, 0);
        app.scroll = 10;
        let before = screen(&mut app, 0);
        stream(&mut app, "\nmore\nand more\nand more still");
        let after = screen(&mut app, 0);
        let pane = |screen: &str| -> Vec<String> {
            // The conversation's rows, without its title (which shows the offset).
            screen.lines().skip(1).take(10).map(String::from).collect()
        };
        assert_eq!(pane(&before), pane(&after), "{before}\n---\n{after}");
        assert_eq!(app.scroll, 13);

        // Following the bottom still shows new output as it comes.
        app.scroll = 0;
        screen(&mut app, 0);
        stream(&mut app, "\nthe newest line");
        assert!(screen(&mut app, 0).contains("the newest line"));
    }

    #[test]
    fn age_rounds_to_the_largest_unit() {
        assert_eq!(age(3_000), "now");
        assert_eq!(age(42_000), "42s");
        assert_eq!(age(5 * 60_000), "5m");
        assert_eq!(age(3 * 3_600_000 + 5), "3h");
        assert_eq!(age(2 * 86_400_000), "2d");
    }

    #[test]
    fn elapsed_switches_to_minutes() {
        assert_eq!(elapsed(4_200), "4.2s");
        assert_eq!(elapsed(185_000), "3m 05s");
        assert_eq!(elapsed(-5), "0.0s");
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
