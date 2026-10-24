use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client as HttpClient, Method, Response};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::db::{Event, Run, Session};
use crate::service::ConnectionInfo;
use crate::ui::Ui;

/// A terminal run failure, distinct from a transport or protocol error.
#[derive(Debug)]
pub struct RunFailed(pub Run);

impl std::fmt::Display for RunFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.0.status.as_str(), self.0.error.as_deref()) {
            ("failed", Some(error)) => f.write_str(error),
            (status, error) => write!(f, "run {status}: {}", error.unwrap_or("")),
        }
    }
}

impl std::error::Error for RunFailed {}

pub struct Client {
    http: HttpClient,
    info: ConnectionInfo,
}

impl Client {
    pub fn connect(dir: &Path) -> Result<Self> {
        let info: ConnectionInfo = serde_json::from_slice(
            &std::fs::read(dir.join("connection.json"))
                .context("daemon unavailable; start `runner serve` first")?,
        )?;
        Ok(Self {
            http: HttpClient::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(3))
                .build()?,
            info,
        })
    }

    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Response> {
        let mut request = self
            .http
            .request(method, format!("http://127.0.0.1:{}{path}", self.info.port))
            .bearer_auth(&self.info.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .context("cannot reach daemon; start `runner serve` first")?;
        if !response.status().is_success() {
            let status = response.status();
            let message = response.json::<Value>().await.unwrap_or_default();
            bail!(
                "daemon returned {status}: {}",
                message["error"].as_str().unwrap_or("request failed")
            );
        }
        Ok(response)
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T> {
        Ok(self.request(method, path, body).await?.json().await?)
    }

    pub async fn create_session(&self, id: Option<String>) -> Result<Session> {
        self.json(Method::POST, "/v1/sessions", Some(json!({"id":id})))
            .await
    }

    pub async fn agents(&self) -> Result<Vec<host::AgentInfo>> {
        self.json(Method::GET, "/v1/agents", None).await
    }

    pub async fn sessions(&self) -> Result<Vec<Session>> {
        self.json(Method::GET, "/v1/sessions", None).await
    }

    pub async fn submit(
        &self,
        session: Option<String>,
        agent: Option<String>,
        prompt: String,
        deadline: u64,
    ) -> Result<Run> {
        self.json(
            Method::POST,
            "/v1/runs",
            Some(
                json!({"session_id":session,"agent":agent,"prompt":prompt,"deadline_secs":deadline}),
            ),
        )
        .await
    }

    /// Runs in submission order, optionally only those of `session`.
    pub async fn runs(&self, session: Option<&str>) -> Result<Vec<Run>> {
        let path = match session {
            // Session IDs are letters, digits, '.', '_' and '-', so need no escaping.
            Some(session) => format!("/v1/runs?session_id={session}"),
            None => "/v1/runs".to_string(),
        };
        self.json(Method::GET, &path, None).await
    }

    pub async fn run(&self, id: &str) -> Result<Run> {
        self.json(Method::GET, &format!("/v1/runs/{id}"), None)
            .await
    }

    pub async fn cancel(&self, id: &str) -> Result<Run> {
        self.json(Method::POST, &format!("/v1/runs/{id}/cancel"), None)
            .await
    }

    /// A run's security audit records, oldest first.
    pub async fn audit(&self, id: &str) -> Result<Vec<Event>> {
        self.json(Method::GET, &format!("/v1/runs/{id}/audit"), None)
            .await
    }

    /// Every event of a finished run, read from its event replay. The replay
    /// ends once a terminal run's events are all sent.
    pub async fn events(&self, id: &str) -> Result<Vec<Event>> {
        let mut bytes = self
            .request(Method::GET, &format!("/v1/runs/{id}/events"), None)
            .await?
            .bytes_stream();
        let mut pending = Vec::new();
        let mut events = Vec::new();
        while let Some(chunk) = bytes.next().await {
            pending.extend_from_slice(&chunk?);
            events.extend(frames(&mut pending)?);
        }
        Ok(events)
    }

    /// A run's events from after `after`, live until the run is terminal.
    /// Reconnects from its cursor when the connection drops, skipping
    /// events it has already yielded.
    pub fn stream(
        &self,
        id: &str,
        mut after: i64,
    ) -> impl futures_util::Stream<Item = Result<Event>> + '_ {
        let id = id.to_string();
        async_stream::try_stream! {
            if after < 0 {
                Err(anyhow::anyhow!("event cursor must be nonnegative"))?;
            }
            loop {
                let response = self
                    .request(
                        Method::GET,
                        &format!("/v1/runs/{id}/events?after={after}"),
                        None,
                    )
                    .await?;
                let mut bytes = response.bytes_stream();
                let mut pending = Vec::new();
                let mut disconnected = false;
                while let Some(chunk) = bytes.next().await {
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(_) => {
                            disconnected = true;
                            break;
                        }
                    };
                    pending.extend_from_slice(&chunk);
                    for event in frames(&mut pending)? {
                        if event.id <= after {
                            continue;
                        }
                        after = event.id;
                        yield event;
                    }
                }
                if !disconnected && self.run(&id).await?.terminal() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    pub async fn watch(&self, id: &str, after: i64, json_output: bool) -> Result<()> {
        if after < 0 {
            bail!("event cursor must be nonnegative");
        }
        let run = self.run(id).await?;
        let mut ui = (!json_output).then(|| {
            if run.terminal() {
                Ui::replay()
            } else {
                let mut ui = Ui::start();
                ui.event(&json!({"kind":"status", "status":run.status}));
                ui
            }
        });
        let events = self.stream(id, after);
        futures_util::pin_mut!(events);
        while let Some(event) = events.next().await {
            let event = event?;
            match &mut ui {
                Some(ui) => ui.event(&event.data),
                None => println!("{}", serde_json::to_string(&event)?),
            }
        }
        if let Some(ui) = &mut ui {
            ui.finish();
        }
        let run = self.run(id).await?;
        if run.status != "completed" {
            return Err(RunFailed(run).into());
        }
        Ok(())
    }
}

/// Takes the complete SSE frames off the front of `pending`.
fn frames(pending: &mut Vec<u8>) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    while let Some(end) = pending.windows(2).position(|w| w == b"\n\n") {
        let frame = String::from_utf8(pending.drain(..end + 2).collect())?;
        if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) {
            events.push(serde_json::from_str(data)?);
        }
    }
    Ok(events)
}
