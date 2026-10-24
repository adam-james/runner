use std::{collections::HashSet, future::Future, path::PathBuf, pin::Pin};

type Interrupt = Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use runner::{
    client::{Client, RunFailed},
    service,
    ui::{self, Ui},
};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(about = "Run sandboxed agents through a local service")]
struct Args {
    #[arg(long, global = true, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("RUNNER_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
                .join(".runner/service")
        })
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon in the foreground. Keep it running for detached jobs.
    Serve {
        #[arg(long, default_value = "runner.toml")]
        config: String,
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
    },
    /// Submit one turn, optionally leaving it running without an attached client.
    Run {
        prompt: String,
        #[arg(long)]
        session: Option<String>,
        /// The agent definition to run; `default` if omitted.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        detach: bool,
        #[arg(long, default_value_t = service::default_deadline())]
        deadline_secs: u64,
    },
    /// Start or continue an interactive conversation.
    Chat {
        #[arg(long)]
        session: Option<String>,
        /// The agent definition each turn runs; `default` if omitted.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = service::default_deadline())]
        deadline_secs: u64,
    },
    /// Full-screen client: sessions, their runs, and a conversation to chat in.
    Tui {
        #[arg(long)]
        session: Option<String>,
        /// The agent definition new turns run; `default` if omitted. Can be
        /// changed from inside.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = service::default_deadline())]
        deadline_secs: u64,
    },
    Runs {
        #[command(subcommand)]
        command: Runs,
    },
    Sessions {
        #[command(subcommand)]
        command: Sessions,
    },
    Agents {
        #[command(subcommand)]
        command: Agents,
    },
    /// Look at the tool and provider components a config declares. Reads
    /// the config and components directly; no daemon needed.
    Tools {
        #[command(subcommand)]
        command: Tools,
    },
}

#[derive(Subcommand)]
enum Tools {
    /// Print a tool (or, with --provider, a provider) as WAT-style text,
    /// headed by what its config grants it.
    Wat {
        name: String,
        #[arg(long, default_value = "runner.toml")]
        config: String,
        #[arg(long)]
        provider: bool,
        /// interface: the component's own imports and exports; structure:
        /// nested modules' too; full: complete WAT, which can be huge.
        #[arg(long, default_value = "structure")]
        detail: host::wat::Detail,
    },
}

#[derive(Subcommand)]
enum Agents {
    /// The agent definitions the daemon's config declares.
    List,
}

#[derive(Subcommand)]
enum Runs {
    List,
    Inspect {
        id: String,
    },
    Watch {
        id: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long)]
        json: bool,
    },
    Cancel {
        id: String,
    },
    /// What the run's components did, and what the host allowed or denied.
    Audit {
        id: String,
        /// One JSON record per line, instead of a table.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum Sessions {
    List,
    Create { id: Option<String> },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Command::Serve {
        config,
        concurrency,
    } = args.command
    {
        use std::io::IsTerminal;
        use tracing_subscriber::EnvFilter;
        let (writer, _guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
            .buffered_lines_limit(4096)
            .lossy(true)
            .finish(std::io::stderr());
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("warn,host=info,runner=info")),
            )
            .with_ansi(std::io::stderr().is_terminal())
            .with_writer(writer)
            .init();
        return service::serve(&config, &args.data_dir, concurrency).await;
    }
    if let Command::Tools {
        command:
            Tools::Wat {
                name,
                config,
                provider,
                detail,
            },
    } = args.command
    {
        use host::wat::Kind;
        let kind = if provider { Kind::Provider } else { Kind::Tool };
        print!("{}", host::wat::print(&config, kind, &name, detail)?);
        return Ok(());
    }
    let client = Client::connect(&args.data_dir)?;
    // Register before printing headers. Retain this future so a signal arriving
    // during API requests or replay stays pending until the next exit point.
    let mut interrupt: Interrupt = Box::pin(tokio::signal::ctrl_c());
    tokio::select! {
        biased;
        result = &mut interrupt => { result?; return Ok(()); },
        _ = std::future::ready(()) => {},
    }
    match args.command {
        Command::Serve { .. } | Command::Tools { .. } => unreachable!(),
        Command::Run {
            prompt,
            session,
            agent,
            detach,
            deadline_secs,
        } => {
            let run = client.submit(session, agent, prompt, deadline_secs).await?;
            header(&client, &run).await?;
            if detach {
                println!("{}", run.id);
            } else {
                let _ = attach(&client, &run.id, 0, false, &mut interrupt).await?;
            }
        }
        Command::Chat {
            session,
            agent,
            deadline_secs,
        } => {
            let agent = agent.unwrap_or_else(|| host::DEFAULT_AGENT.to_string());
            let agents = client.agents().await?;
            let details = known_agent(&agents, &agent)?;
            let (session, existing) = open_session(&client, session).await?;
            ui::banner(details, &session);
            let result = chat(
                &client,
                &session,
                agent,
                deadline_secs,
                existing,
                &mut interrupt,
            )
            .await;
            // However chat ends, even on an error, say how to come back to it.
            ui::goodbye(&session);
            result?;
        }
        Command::Tui {
            session,
            agent,
            deadline_secs,
        } => {
            let agent = agent.unwrap_or_else(|| host::DEFAULT_AGENT.to_string());
            let agents = client.agents().await?;
            known_agent(&agents, &agent)?;
            // Without --session it starts on a new session, which its first
            // prompt creates, so just looking leaves no empty sessions behind.
            let session = match session {
                Some(id) => Some(open_session(&client, Some(id)).await?.0),
                None => None,
            };
            let session = runner::tui::run(client, agents, agent, session, deadline_secs).await?;
            ui::goodbye_tui(session.as_deref());
        }
        Command::Runs { command } => match command {
            Runs::List => println!(
                "{}",
                serde_json::to_string_pretty(&client.runs(None).await?)?
            ),
            Runs::Inspect { id } => {
                println!("{}", serde_json::to_string_pretty(&client.run(&id).await?)?)
            }
            Runs::Cancel { id } => println!(
                "{}",
                serde_json::to_string_pretty(&client.cancel(&id).await?)?
            ),
            Runs::Audit { id, json } => {
                let records = client.audit(&id).await?;
                if json {
                    for record in &records {
                        println!("{}", serde_json::to_string(&record.data)?);
                    }
                } else {
                    print!("{}", runner::audit::render(&records));
                }
                for problem in runner::audit::check(&records) {
                    eprintln!("warning: {problem}");
                }
            }
            Runs::Watch { id, after, json } => {
                header(&client, &client.run(&id).await?).await?;
                let _ = attach(&client, &id, after, json, &mut interrupt).await?;
            }
        },
        Command::Agents {
            command: Agents::List,
        } => println!("{}", serde_json::to_string_pretty(&client.agents().await?)?),
        Command::Sessions { command } => match command {
            Sessions::List => println!(
                "{}",
                serde_json::to_string_pretty(&client.sessions().await?)?
            ),
            Sessions::Create { id } => println!(
                "{}",
                serde_json::to_string_pretty(&client.create_session(id).await?)?
            ),
        },
    }
    Ok(())
}

/// The daemon's definition of `agent`, checked up front so a typo doesn't
/// wait for the first prompt to fail.
fn known_agent<'a>(agents: &'a [host::AgentInfo], agent: &str) -> Result<&'a host::AgentInfo> {
    match agents.iter().find(|known| known.name == agent) {
        Some(details) => Ok(details),
        None => {
            let names: Vec<_> = agents.iter().map(|known| known.name.as_str()).collect();
            bail!(
                "unknown agent '{agent}'; the daemon has: {}",
                names.join(", ")
            )
        }
    }
}

/// The named session, created if it isn't saved yet, or a new one; and
/// whether it already existed.
async fn open_session(client: &Client, session: Option<String>) -> Result<(String, bool)> {
    Ok(match session {
        Some(id) => {
            let existing = client
                .sessions()
                .await?
                .iter()
                .any(|session| session.id == id);
            if !existing {
                client.create_session(Some(id.clone())).await?;
            }
            (id, existing)
        }
        None => (client.create_session(None).await?.id, false),
    })
}

/// Prints a run's agent, with the provider, model and tools the daemon has
/// configured for it, and its session.
async fn header(client: &Client, run: &runner::db::Run) -> Result<()> {
    let agents = client.agents().await?;
    let details = agents.iter().find(|known| known.name == run.agent);
    ui::run_header(&run.agent, details, &run.session_id);
    Ok(())
}

/// Takes prompts until the user leaves, running each as a turn in `session`.
async fn chat(
    client: &Client,
    session: &str,
    agent: String,
    deadline_secs: u64,
    existing: bool,
    interrupt: &mut Interrupt,
) -> Result<()> {
    let mut seen = HashSet::new();
    if existing {
        resumed(client, session, &mut seen).await?;
    }
    let mut lines = stdin_lines();
    loop {
        if catch_up(client, session, &mut seen, interrupt, None).await? == Attachment::Detached {
            break;
        }
        ui::prompt();
        let line = tokio::select! { biased; result = &mut *interrupt => { result?; break; }, line = lines.recv() => line };
        ui::end_input();
        let Some(line) = line else {
            break;
        };
        let line = line?;
        let prompt = line.trim();
        if matches!(prompt, "exit" | "quit") {
            break;
        }
        if prompt.is_empty() {
            continue;
        }
        let run = client
            .submit(
                Some(session.to_string()),
                Some(agent.clone()),
                prompt.to_string(),
                deadline_secs,
            )
            .await?;
        if catch_up(client, session, &mut seen, interrupt, Some(&run.id)).await?
            == Attachment::Detached
        {
            break;
        }
    }
    Ok(())
}

/// Stdin's lines, read on a thread of their own. Tokio's stdin reads on its
/// blocking pool, which the runtime waits for on exit, so leaving with Ctrl-C
/// while a read was pending hung until the user pressed Enter. A plain thread
/// doesn't hold up the process's exit.
fn stdin_lines() -> mpsc::UnboundedReceiver<std::io::Result<String>> {
    let (send, receive) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    receive
}

/// Replays a picked-up session's conversation: each completed turn's prompt
/// and output. Other turns weren't saved, so the model doesn't remember them.
async fn resumed(client: &Client, session: &str, seen: &mut HashSet<String>) -> Result<()> {
    let runs = client.runs(Some(session)).await?;
    // Failed history was not saved. Mark all initial terminal turns as seen,
    // but leave pending turns to catch_up even if they finish before it lists.
    seen.extend(
        runs.iter()
            .filter(|run| run.terminal())
            .map(|run| run.id.clone()),
    );
    let turns: Vec<_> = runs
        .into_iter()
        .filter(|run| run.status == "completed")
        .collect();
    if turns.is_empty() {
        return Ok(());
    }
    ui::resumed(turns.len());
    for turn in turns {
        ui::past_prompt(&turn.prompt);
        let mut ui = Ui::replay();
        for event in client.events(&turn.id).await? {
            ui.event(&event.data);
        }
        ui.finish();
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Attachment {
    Finished,
    Detached,
}

/// Follow terminal failures visibly, while connection failures abort catch-up.
async fn follow(client: &Client, id: &str, interrupt: &mut Interrupt) -> Result<Attachment> {
    match attach(client, id, 0, false, interrupt).await {
        Err(error) if error.is::<RunFailed>() => {
            ui::error(&error.to_string());
            Ok(Attachment::Finished)
        }
        result => result,
    }
}

async fn catch_up(
    client: &Client,
    session: &str,
    seen: &mut HashSet<String>,
    interrupt: &mut Interrupt,
    submitted: Option<&str>,
) -> Result<Attachment> {
    loop {
        let pending: Vec<_> = client
            .runs(Some(session))
            .await?
            .into_iter()
            .filter(|run| !seen.contains(&run.id))
            .collect();
        if pending.is_empty() {
            return Ok(Attachment::Finished);
        }
        for run in pending {
            if submitted != Some(run.id.as_str()) {
                ui::past_prompt(&run.prompt);
                ui::pending(&run);
            }
            if follow(client, &run.id, interrupt).await? == Attachment::Detached {
                return Ok(Attachment::Detached);
            }
            seen.insert(run.id);
        }
    }
}

async fn attach(
    client: &Client,
    id: &str,
    after: i64,
    json: bool,
    interrupt: &mut Interrupt,
) -> Result<Attachment> {
    let run = client.run(id).await?;
    let outcome = tokio::select! {
        biased;
        result = &mut *interrupt => { result?; Attachment::Detached },
        result = client.watch(id, after, json) => { result?; Attachment::Finished },
    };
    if outcome == Attachment::Detached {
        ui::detached(&run);
    }
    Ok(outcome)
}
