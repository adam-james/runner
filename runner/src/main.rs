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
        /// The agent to run; resumes the session agent, or `default` for a new session.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        detach: bool,
        #[arg(long, default_value_t = service::default_deadline())]
        deadline_secs: u64,
        /// When it notifies once it ends: all, problems (the default:
        /// failed or interrupted, or with alerts) or never.
        #[arg(long, value_parser = runner::notify::LEVELS.to_vec())]
        notify: Option<String>,
    },
    /// Start or continue an interactive conversation.
    Chat {
        #[arg(long)]
        session: Option<String>,
        /// The session agent; uses the saved agent, or `default` for a new session.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = service::default_deadline())]
        deadline_secs: u64,
    },
    /// Full-screen client: sessions, their runs, and a conversation to chat in.
    Tui {
        #[arg(long)]
        session: Option<String>,
        /// The session agent; uses the saved agent, or `default` for a new session. Can be
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
    /// Prompts that run later, each pinned to the agent versions it was
    /// approved for. A schedule whose agent changes pauses until approved.
    Schedules {
        #[command(subcommand)]
        command: Schedules,
    },
    /// What scheduled runs did once they ended, kept by the daemon until
    /// read; host-written, never the model's reply.
    Notifications {
        #[command(subcommand)]
        command: Notifications,
    },
    /// What configured tools can reach: network hosts, injected headers
    /// (names only), files and limits.
    Tools {
        #[command(subcommand)]
        command: Tools,
    },
    /// What configured model providers can reach, like `tools`.
    Providers {
        #[command(subcommand)]
        command: Components,
    },
}

#[derive(Subcommand)]
enum Components {
    /// Each one's name and a one-line summary of its access.
    List,
    /// One in full: hosts, headers, files, limits and its component's imports.
    Inspect {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum Tools {
    #[command(flatten)]
    Components(Components),
    /// Print a tool (or, with --provider, a provider) as WAT-style text,
    /// headed by what its config grants it. Reads the config and components
    /// directly; no daemon needed.
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
    /// The agent definitions the daemon's config declares, with each one's
    /// current version.
    List,
    /// What runs of an agent version were allowed: the definition it
    /// snapshots, as JSON. Runs' audit trails name their version.
    Version { id: String },
}

#[derive(Subcommand)]
enum Schedules {
    /// Run a prompt once, at a local time or after a delay, or on a cron
    /// schedule, each time in a fresh session.
    #[command(group(clap::ArgGroup::new("when").required(true).args(["at", "delay", "cron"])))]
    Add {
        prompt: String,
        /// When, e.g. 2026-10-29T07:00.
        #[arg(long)]
        at: Option<String>,
        /// How long from now instead, e.g. 2s, 5m or 1h30m.
        #[arg(long = "in", value_name = "DELAY", value_parser = runner::schedule::parse_delay)]
        delay: Option<u64>,
        /// Recur on a cron expression instead, e.g. "0 7 * * MON-FRI":
        /// minute, hour, day of month, month, day of week, in --tz.
        #[arg(long)]
        cron: Option<String>,
        /// An IANA time zone, e.g. Europe/Warsaw; this system's if omitted.
        #[arg(long)]
        tz: Option<String>,
        #[arg(long, default_value = host::DEFAULT_AGENT)]
        agent: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value_t = service::default_deadline())]
        deadline_secs: u64,
        /// When its runs notify: all of them, only problems (failed or
        /// interrupted, or with alerts), or never.
        #[arg(long, default_value = "problems", value_parser = runner::notify::LEVELS.to_vec())]
        notify: String,
    },
    List,
    /// A schedule and its history: what was done to it and what it ran.
    Show {
        id: String,
    },
    Pause {
        id: String,
    },
    /// Resume a paused schedule whose agents haven't changed.
    Resume {
        id: String,
    },
    /// Pin a schedule to its agents' current versions and make it active,
    /// e.g. after a config change paused it. Inspect what changed first
    /// with `schedules show` and `agents version`.
    Approve {
        id: String,
    },
    Rm {
        id: String,
    },
    /// Change when a schedule's runs notify: all, problems or never.
    Notify {
        id: String,
        #[arg(value_parser = runner::notify::LEVELS.to_vec())]
        level: String,
    },
}

#[derive(Subcommand)]
enum Notifications {
    /// Newest first: unread ones, or every one with --all.
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Mark a notification read, or every unread one with --all.
    #[command(group(clap::ArgGroup::new("which").required(true).args(["id", "all"])))]
    Read {
        id: Option<i64>,
        #[arg(long)]
        all: bool,
    },
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
    /// What the run and every run it delegated to did, from their audit
    /// records: hosts reached, denials, errors and trail problems.
    Summary {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum Sessions {
    List,
    Create {
        id: Option<String>,
        /// Agent this session will use for its lifetime.
        #[arg(long, default_value = host::DEFAULT_AGENT)]
        agent: String,
    },
    /// The audit records of every run in the session, a run at a time.
    Audit {
        id: String,
        /// One JSON record per line, instead of a table.
        #[arg(long)]
        json: bool,
    },
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
        print!("{}", host::wat::print(&config, kind, &name, detail).await?);
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
        Command::Serve { .. }
        | Command::Tools {
            command: Tools::Wat { .. },
        } => unreachable!(),
        Command::Run {
            prompt,
            session,
            agent,
            detach,
            deadline_secs,
            notify,
        } => {
            let run = client
                .submit_notifying(session, agent, prompt, deadline_secs, notify)
                .await?;
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
            let agent = session_agent(&client, session.as_deref(), agent).await?;
            let agents = client.agents().await?;
            let details = known_agent(&agents, &agent)?;
            let (session, existing) = open_session(&client, session, &agent).await?;
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
            let agent = session_agent(&client, session.as_deref(), agent).await?;
            let agents = client.agents().await?;
            known_agent(&agents, &agent)?;
            // Without --session it starts on a new session, which its first
            // prompt creates, so just looking leaves no empty sessions behind.
            let session = match session {
                Some(id) => Some(open_session(&client, Some(id), &agent).await?.0),
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
            Runs::Audit { id, json } => print_audit(&client.audit(&id).await?, json)?,
            Runs::Summary { id, json } => {
                let summary = client.summary(&id).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&summary)?);
                } else {
                    print!("{}", runner::summary::render(&summary));
                }
            }
            Runs::Watch { id, after, json } => {
                header(&client, &client.run(&id).await?).await?;
                let _ = attach(&client, &id, after, json, &mut interrupt).await?;
            }
        },
        Command::Agents { command } => match command {
            Agents::List => println!("{}", serde_json::to_string_pretty(&client.agents().await?)?),
            Agents::Version { id } => println!(
                "{}",
                serde_json::to_string_pretty(&client.agent_version(&id).await?)?
            ),
        },
        Command::Schedules { command } => schedules(&client, command).await?,
        Command::Notifications { command } => match command {
            Notifications::List { all, json } => {
                let notifications = client.notifications(!all).await?;
                if json {
                    print(&notifications)?
                } else if notifications.is_empty() {
                    eprintln!("no {}notifications", if all { "" } else { "unread " });
                } else {
                    print!("{}", runner::notify::render(&notifications));
                }
            }
            Notifications::Read { id, .. } => {
                let marked = client.read_notifications(id).await?;
                eprintln!("marked {marked} read");
            }
        },
        Command::Tools {
            command: Tools::Components(command),
        } => components(&client, host::inspect::ComponentKind::Tool, command).await?,
        Command::Providers { command } => {
            components(&client, host::inspect::ComponentKind::Provider, command).await?
        }
        Command::Sessions { command } => match command {
            Sessions::List => println!(
                "{}",
                serde_json::to_string_pretty(&client.sessions().await?)?
            ),
            Sessions::Create { id, agent } => println!(
                "{}",
                serde_json::to_string_pretty(&client.create_session(id, agent).await?)?
            ),
            Sessions::Audit { id, json } => print_audit(&client.session_audit(&id).await?, json)?,
        },
    }
    Ok(())
}

async fn schedules(client: &Client, command: Schedules) -> Result<()> {
    match command {
        Schedules::Add {
            prompt,
            at,
            delay,
            cron,
            tz,
            agent,
            id,
            deadline_secs,
            notify,
        } => {
            let tz = match tz {
                Some(tz) => tz,
                None => iana_time_zone::get_timezone().map_err(|_| {
                    anyhow::anyhow!("can't tell this system's time zone; pass --tz")
                })?,
            };
            print(
                &client
                    .create_schedule(
                        id,
                        agent,
                        prompt,
                        at,
                        delay,
                        cron,
                        tz,
                        deadline_secs,
                        notify,
                    )
                    .await?,
            )?
        }
        Schedules::List => print(&client.schedules().await?)?,
        Schedules::Show { id } => print(&serde_json::json!({
            "schedule": client.schedule(&id).await?,
            "history": client.schedule_history(&id).await?,
        }))?,
        Schedules::Pause { id } => print(&client.schedule_action(&id, "pause").await?)?,
        Schedules::Resume { id } => print(&client.schedule_action(&id, "resume").await?)?,
        Schedules::Approve { id } => {
            let approval = client.approve_schedule(&id).await?;
            for change in &approval.changes {
                let fields = change
                    .fields
                    .as_ref()
                    .map_or("definition not stored".into(), |f| f.join(", "));
                eprintln!(
                    "{}: {} -> {} ({fields})",
                    change.agent,
                    change.from.as_deref().unwrap_or("none"),
                    change.to.as_deref().unwrap_or("none"),
                );
            }
            print(&approval)?
        }
        Schedules::Rm { id } => print(&client.delete_schedule(&id).await?)?,
        Schedules::Notify { id, level } => print(&client.set_schedule_notify(&id, &level).await?)?,
    }
    Ok(())
}

fn print(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Resume with the session's owner, rejecting explicit changes before reading history.
async fn session_agent(
    client: &Client,
    session: Option<&str>,
    agent: Option<String>,
) -> Result<String> {
    let owner = client
        .sessions()
        .await?
        .into_iter()
        .find(|s| Some(s.id.as_str()) == session)
        .map(|s| s.agent);
    if let (Some(owner), Some(requested)) = (&owner, &agent)
        && owner != requested
    {
        anyhow::bail!(
            "session is locked to agent '{owner}'; create a new session to use agent '{requested}'"
        );
    }
    Ok(agent
        .or(owner)
        .unwrap_or_else(|| host::DEFAULT_AGENT.to_string()))
}

/// The daemon's definition of `agent`, checked up front so a typo doesn't
/// wait for the first prompt to fail.
/// Audit records as a table, or one JSON record per line, then a warning on
/// stderr for each sign the trail is incomplete.
fn print_audit(records: &[runner::db::Event], json: bool) -> Result<()> {
    if json {
        for record in records {
            println!("{}", serde_json::to_string(&record.data)?);
        }
    } else {
        print!("{}", runner::audit::render(records));
    }
    for problem in runner::audit::check(records) {
        eprintln!("warning: {problem}");
    }
    Ok(())
}

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

/// `tools` and `providers`: lists the daemon's components of `kind`, or
/// describes one of them.
async fn components(
    client: &Client,
    kind: host::inspect::ComponentKind,
    command: Components,
) -> Result<()> {
    let mut components = client.components().await?;
    components.retain(|component| component.kind == kind);
    match command {
        Components::List => {
            let width = components.iter().map(|c| c.name.len()).max().unwrap_or(0);
            for component in &components {
                println!("{:<width$}  {}", component.name, component.summary());
            }
        }
        Components::Inspect { name, json } => {
            let Some(component) = components.iter().find(|c| c.name == name) else {
                if kind == host::inspect::ComponentKind::Tool {
                    match name.as_str() {
                        "delegate" | "wait" => {
                            bail!("'{name}' is a host tool for delegation; it runs no component")
                        }
                        "inspect_component" => bail!(
                            "'{name}' is a host tool that reads configured components; \
                             it runs no component"
                        ),
                        "audit_trail" => bail!(
                            "'{name}' is a host tool that reads other runs' audit trails; \
                             it runs no component"
                        ),
                        _ => {}
                    }
                }
                let names: Vec<_> = components.iter().map(|c| c.name.as_str()).collect();
                bail!(
                    "unknown {kind} '{name}'; the daemon has: {}",
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    }
                );
            };
            if json {
                println!("{}", serde_json::to_string_pretty(component)?);
            } else {
                println!("{}", component.describe());
            }
        }
    }
    Ok(())
}

/// The named session, created if it isn't saved yet, or a new one; and
/// whether it already existed.
async fn open_session(
    client: &Client,
    session: Option<String>,
    agent: &str,
) -> Result<(String, bool)> {
    Ok(match session {
        Some(id) => {
            let existing = client
                .sessions()
                .await?
                .iter()
                .any(|session| session.id == id);
            if !existing {
                client
                    .create_session(Some(id.clone()), agent.to_string())
                    .await?;
            }
            (id, existing)
        }
        None => (
            client.create_session(None, agent.to_string()).await?.id,
            false,
        ),
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
