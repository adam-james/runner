//! `runner tui`: a full-screen client for the daemon. Sessions and their runs
//! on one side, the selected session's conversation and a prompt on the
//! other. Like `chat`, it only watches and submits; runs keep going after it
//! quits, and reopening a session shows all of them again.

mod app;
mod view;

use std::io;
use std::sync::Arc;
use std::time::Duration;

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
/// How often the screen redraws without other news, for elapsed times.
const TICK: Duration = Duration::from_millis(100);

/// Runs the TUI until the user quits, and returns the session it was
/// showing, if it had one. Without `session`, it starts on a new session,
/// created by its first prompt.
pub async fn run(
    client: Client,
    agents: Vec<host::AgentInfo>,
    agent: String,
    session: Option<String>,
    deadline_secs: u64,
) -> Result<Option<String>> {
    // Also installs a panic hook that puts the terminal back.
    let mut terminal = ratatui::init();
    // Leave the mouse to the terminal so dragging selects text for copying.
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
    session: Option<String>,
    deadline_secs: u64,
) -> Result<Option<String>> {
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
        terminal.draw(|frame| view::draw(frame, &mut app, crate::db::now()))?;
        let Some(msg) = receive.recv().await else {
            break Ok(());
        };
        let mut quit = false;
        followers.retain(|follower| !follower.is_finished());
        for action in app.update(msg) {
            let client = client.clone();
            let send = send.clone();
            match action {
                Action::Quit => quit = true,
                Action::OpenNotification {
                    id,
                    run_id,
                    generation,
                } => {
                    tokio::spawn(async move {
                        match client.run(&run_id).await {
                            Ok(run) => {
                                send.send(Msg::NotificationRun { generation, run }).ok();
                                if let Err(error) = client.read_notifications(Some(id)).await {
                                    send.send(Msg::Error(format!("{error:#}"))).ok();
                                }
                            }
                            Err(error) => {
                                send.send(Msg::Error(format!("{error:#}"))).ok();
                            }
                        }
                    });
                }
                Action::ReadNotifications => {
                    tokio::spawn(async move {
                        match client.read_notifications(None).await {
                            Ok(_) => {
                                send.send(Msg::Notice("all notifications marked read".into()))
                                    .ok();
                            }
                            Err(error) => {
                                send.send(Msg::Error(format!("{error:#}"))).ok();
                            }
                        }
                    });
                }
                Action::Submit {
                    session,
                    agent,
                    prompt,
                    generation,
                } => {
                    tokio::spawn(async move {
                        let msg = match submit(
                            &client,
                            session,
                            agent,
                            &prompt,
                            deadline_secs,
                            generation,
                            &send,
                        )
                        .await
                        {
                            Ok(run) => Msg::Submitted(run),
                            Err(error) => Msg::SubmitFailed {
                                prompt,
                                error: format!("{error:#}"),
                            },
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

/// Submits a turn, first creating its session if it has none yet.
async fn submit(
    client: &Client,
    session: Option<String>,
    agent: String,
    prompt: &str,
    deadline_secs: u64,
    generation: u64,
    send: &mpsc::UnboundedSender<Msg>,
) -> Result<crate::db::Run> {
    let session = match session {
        Some(session) => session,
        None => {
            let session = client.create_session(None, agent.clone()).await?;
            let id = session.id.clone();
            send.send(Msg::Created {
                generation,
                session,
            })
            .ok();
            id
        }
    };
    client
        .submit(
            Some(session),
            Some(agent),
            prompt.to_string(),
            deadline_secs,
        )
        .await
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
    mut selection: watch::Receiver<Option<String>>,
    send: mpsc::UnboundedSender<Msg>,
) {
    loop {
        let session = selection.borrow_and_update().clone();
        let mut msgs = match client.sessions().await {
            Ok(sessions) => vec![Msg::Sessions(sessions)],
            Err(error) => vec![Msg::Error(format!("{error:#}"))],
        };
        msgs.push(match client.unread_notifications().await {
            Ok(notes) => Msg::Notifications(notes),
            Err(error) => Msg::Error(format!("{error:#}")),
        });
        if let Some(session) = session {
            msgs.push(match client.runs(Some(&session)).await {
                Ok(runs) => Msg::Runs(session, runs),
                Err(error) => Msg::Error(format!("{error:#}")),
            });
        }
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
