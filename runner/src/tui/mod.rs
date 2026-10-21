//! `runner tui`: a full-screen client for the daemon. Sessions and their runs
//! on one side, the selected session's conversation and a prompt on the
//! other. Like `chat`, it only watches and submits; runs keep going after it
//! quits.

mod app;
mod view;

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::client::Client;
use app::{Action, App, Msg};

/// How often the sessions and the selected session's runs are listed again.
const POLL: Duration = Duration::from_secs(1);
/// How often the screen redraws without other news, for the "Thinking…" timer.
const TICK: Duration = Duration::from_millis(100);

/// Runs the TUI until the user quits, and returns the session it was showing.
pub async fn run(
    client: Client,
    agents: Vec<host::AgentInfo>,
    agent: String,
    session: String,
    deadline_secs: u64,
) -> Result<String> {
    // Also installs a panic hook that puts the terminal back.
    let mut terminal = ratatui::init();
    execute!(io::stdout(), EnableBracketedPaste)?;
    let result = event_loop(
        &mut terminal,
        Arc::new(client),
        agents,
        agent,
        session,
        deadline_secs,
    )
    .await;
    execute!(io::stdout(), DisableBracketedPaste).ok();
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    client: Arc<Client>,
    agents: Vec<host::AgentInfo>,
    agent: String,
    session: String,
    deadline_secs: u64,
) -> Result<String> {
    let (send, mut receive) = mpsc::unbounded_channel();
    let (selected, selection) = watch::channel(session.clone());
    let background = [
        tokio::spawn(read_terminal(send.clone())),
        tokio::spawn(tick(send.clone())),
        tokio::spawn(poll(client.clone(), selection, send.clone())),
    ];
    let mut app = App::new(agents, agent, session);
    // Tasks for the selected session's runs, dropped when it changes.
    let mut followers: Vec<JoinHandle<()>> = Vec::new();
    let outcome = loop {
        terminal.draw(|frame| view::draw(frame, &mut app, Instant::now()))?;
        let Some(msg) = receive.recv().await else {
            break Ok(());
        };
        let mut quit = false;
        for action in app.update(msg, Instant::now()) {
            let client = client.clone();
            let send = send.clone();
            match action {
                Action::Quit => quit = true,
                Action::Submit {
                    session,
                    agent,
                    prompt,
                } => {
                    tokio::spawn(async move {
                        let msg = match client
                            .submit(Some(session), Some(agent), prompt, deadline_secs)
                            .await
                        {
                            Ok(run) => Msg::Submitted(run),
                            Err(error) => Msg::Error(format!("{error:#}")),
                        };
                        send.send(msg).ok();
                    });
                }
                Action::Cancel(run_id) => {
                    tokio::spawn(async move {
                        let msg = match client.cancel(&run_id).await {
                            Ok(run) => Msg::Notice(format!("cancel requested for run {}", run.id)),
                            Err(error) => Msg::Error(format!("{error:#}")),
                        };
                        send.send(msg).ok();
                    });
                }
                Action::CreateSession(id) => {
                    tokio::spawn(async move {
                        let msg = match client.create_session(id).await {
                            Ok(session) => Msg::SessionCreated(session),
                            Err(error) => Msg::Error(format!("{error:#}")),
                        };
                        send.send(msg).ok();
                    });
                }
                Action::SelectSession { session, .. } => {
                    for follower in followers.drain(..) {
                        follower.abort();
                    }
                    selected.send_replace(session);
                }
                Action::LoadHistory { run_id, generation } => {
                    followers.push(tokio::spawn(async move {
                        let msg = match client.events(&run_id).await {
                            Ok(events) => Msg::History {
                                generation,
                                run_id,
                                events,
                            },
                            Err(error) => Msg::Error(format!("{error:#}")),
                        };
                        send.send(msg).ok();
                    }))
                }
                Action::Watch { run_id, generation } => {
                    followers.push(tokio::spawn(follow(client, run_id, generation, send)))
                }
            }
        }
        if quit {
            break Ok(());
        }
    };
    for task in background.iter().chain(&followers) {
        task.abort();
    }
    outcome.map(|()| app.session)
}

/// Keys, pastes and resizes, as messages.
async fn read_terminal(send: mpsc::UnboundedSender<Msg>) {
    let mut events = EventStream::new();
    while let Some(Ok(event)) = events.next().await {
        let msg = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => Msg::Key(key),
            Event::Paste(text) => Msg::Paste(text),
            Event::Resize(..) => Msg::Tick,
            _ => continue,
        };
        if send.send(msg).is_err() {
            break;
        }
    }
}

async fn tick(send: mpsc::UnboundedSender<Msg>) {
    let mut interval = tokio::time::interval(TICK);
    while send.send(Msg::Tick).is_ok() {
        interval.tick().await;
    }
}

/// Lists the sessions and the selected session's runs every `POLL`, and
/// straight away when the selection changes.
async fn poll(
    client: Arc<Client>,
    mut selection: watch::Receiver<String>,
    send: mpsc::UnboundedSender<Msg>,
) {
    loop {
        let session = selection.borrow_and_update().clone();
        let msgs = match (client.sessions().await, client.runs(Some(&session)).await) {
            (Ok(sessions), Ok(runs)) => vec![Msg::Sessions(sessions), Msg::Runs(session, runs)],
            (Err(error), _) | (_, Err(error)) => vec![Msg::Error(format!("{error:#}"))],
        };
        for msg in msgs {
            if send.send(msg).is_err() {
                return;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL) => {}
            changed = selection.changed() => if changed.is_err() { return },
        }
    }
}

/// Forwards a run's events as they're recorded, then its final state.
async fn follow(
    client: Arc<Client>,
    run_id: String,
    generation: u64,
    send: mpsc::UnboundedSender<Msg>,
) {
    let events = client.stream(&run_id, 0);
    futures_util::pin_mut!(events);
    while let Some(event) = events.next().await {
        let msg = match event {
            Ok(event) => Msg::RunEvent {
                generation,
                run_id: run_id.clone(),
                data: event.data,
            },
            Err(error) => {
                send.send(Msg::Error(format!("{error:#}"))).ok();
                return;
            }
        };
        if send.send(msg).is_err() {
            return;
        }
    }
    let msg = match client.run(&run_id).await {
        Ok(run) => Msg::RunEnded { generation, run },
        Err(error) => Msg::Error(format!("{error:#}")),
    };
    send.send(msg).ok();
}
