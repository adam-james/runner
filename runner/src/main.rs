use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use runner::{
    client::Client,
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
    let client = Client::connect(&args.data_dir)?;
    match args.command {
        Command::Serve { .. } => unreachable!(),
        Command::Run {
            prompt,
            session,
            agent,
            detach,
            deadline_secs,
        } => {
            let run = client.submit(session, agent, prompt, deadline_secs).await?;
            ui::run_header(&run.agent, &run.session_id);
            if detach {
                println!("{}", run.id);
            } else {
                attach(&client, &run.id, 0, false).await?;
            }
        }
        Command::Chat {
            session,
            agent,
            deadline_secs,
        } => {
            let agent = agent.unwrap_or_else(|| host::DEFAULT_AGENT.to_string());
            // Checked up front, so a typo doesn't wait for the first prompt to fail.
            let agents = client.agents().await?;
            if !agents.iter().any(|known| known.name == agent) {
                let names: Vec<_> = agents.iter().map(|known| known.name.as_str()).collect();
                bail!(
                    "unknown agent '{agent}'; the daemon has: {}",
                    names.join(", ")
                );
            }
            let (session, existing) = match session {
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
            };
            ui::banner(&agent, &session);
            if existing {
                resumed(&client, &session).await?;
            }
            let result = chat(&client, &session, agent, deadline_secs).await;
            // However chat ends, even on an error, say how to come back to it.
            ui::goodbye(&session);
            result?;
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
            Runs::Watch { id, after, json } => {
                let run = client.run(&id).await?;
                ui::run_header(&run.agent, &run.session_id);
                attach(&client, &id, after, json).await?
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

/// Takes prompts until the user leaves, running each as a turn in `session`.
async fn chat(client: &Client, session: &str, agent: String, deadline_secs: u64) -> Result<()> {
    let mut lines = stdin_lines();
    loop {
        ui::prompt();
        let line =
            tokio::select! { line = lines.recv() => line, _ = tokio::signal::ctrl_c() => break };
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
        if let Err(error) = attach(client, &run.id, 0, false).await {
            ui::error(&error.to_string());
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
async fn resumed(client: &Client, session: &str) -> Result<()> {
    let turns: Vec<_> = client
        .runs(Some(session))
        .await?
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

async fn attach(client: &Client, id: &str, after: i64, json: bool) -> Result<()> {
    tokio::select! {
        result = client.watch(id, after, json) => result,
        _ = tokio::signal::ctrl_c() => bail!("detached; run {id} continues. Stop it with `runner runs cancel {id}`"),
    }
}
