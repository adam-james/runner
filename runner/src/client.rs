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

    pub async fn sessions(&self) -> Result<Vec<Session>> {
        self.json(Method::GET, "/v1/sessions", None).await
    }

    pub async fn submit(
        &self,
        session: Option<String>,
        prompt: String,
        deadline: u64,
    ) -> Result<Run> {
        self.json(
            Method::POST,
            "/v1/runs",
            Some(json!({"session_id":session,"prompt":prompt,"deadline_secs":deadline})),
        )
        .await
    }

    pub async fn runs(&self) -> Result<Vec<Run>> {
        self.json(Method::GET, "/v1/runs", None).await
    }

    pub async fn run(&self, id: &str) -> Result<Run> {
        self.json(Method::GET, &format!("/v1/runs/{id}"), None)
            .await
    }

    pub async fn cancel(&self, id: &str) -> Result<Run> {
        self.json(Method::POST, &format!("/v1/runs/{id}/cancel"), None)
            .await
    }

    /// The model text of a finished run, read from its event replay.
    pub async fn reply(&self, id: &str) -> Result<String> {
        let mut bytes = self
            .request(Method::GET, &format!("/v1/runs/{id}/events"), None)
            .await?
            .bytes_stream();
        let mut pending = Vec::new();
        let mut text = String::new();
        while let Some(chunk) = bytes.next().await {
            pending.extend_from_slice(&chunk?);
            for event in frames(&mut pending)? {
                if event.data["kind"] == "text" {
                    text.push_str(event.data["text"].as_str().unwrap_or_default());
                }
            }
        }
        Ok(text)
    }

    pub async fn watch(&self, id: &str, mut after: i64, json_output: bool) -> Result<()> {
        if after < 0 {
            bail!("event cursor must be nonnegative");
        }
        let mut ui = (!json_output).then(Ui::start);
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
                    match &mut ui {
                        Some(ui) => ui.event(&event.data),
                        None => println!("{}", serde_json::to_string(&event)?),
                    }
                    after = event.id;
                }
            }
            if !disconnected {
                let run = self.run(id).await?;
                if run.terminal() {
                    if let Some(ui) = &mut ui {
                        ui.finish();
                    }
                    if run.status != "completed" {
                        match (run.status.as_str(), run.error) {
                            // Already a full sentence, e.g. "API error 401: invalid x-api-key".
                            ("failed", Some(error)) => bail!("{error}"),
                            (status, error) => {
                                bail!("run {status}: {}", error.as_deref().unwrap_or(""))
                            }
                        }
                    }
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
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
