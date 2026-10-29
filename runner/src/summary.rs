//! `runner runs summary`: what a run, and every run it delegated to, did,
//! worked out by the host from their audit records with no model involved,
//! so a background run that nobody watched can be checked at a glance.
//!
//! Like the records it's made from, a summary is metadata only. It leaves
//! out the run's error too, since a failed turn's error is the agent's own
//! message and a prompt injection could steer it. The few guest-influenced
//! strings it keeps (hosts, tool names) are rendered JSON-quoted.
//!
//! Its alerts are what a person should look at: denials, an incomplete
//! trail (which leaves "no denials" meaningless), and, for a scheduled run,
//! a host none of the schedule's earlier runs reached.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::{AgentVersion, Event, Run};

/// Denials and errors past this many are counted, not listed.
pub const MAX_LISTED: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub run_id: String,
    pub agent: String,
    pub schedule_id: Option<String>,
    /// The run's status, e.g. `completed` or `failed`.
    pub status: String,
    /// What needs a person's attention; empty when nothing does.
    #[serde(default)]
    pub alerts: Vec<Alert>,
    /// Runs in its tree: it and every run it delegated to, transitively.
    pub runs: usize,
    /// Of those, the ones still queued or running, whose records may be
    /// incomplete.
    pub unfinished_runs: usize,
    /// Of those, the ones that ended without completing (`failed`,
    /// `interrupted` or `cancelled`), the root included, in order.
    #[serde(default)]
    pub not_completed: Vec<RunEnd>,
    pub turns: u64,
    /// From the tree's first audit record to its last; `None` without any.
    pub duration_ms: Option<u64>,
    /// How many records of each action, e.g. `tool.call`.
    pub actions: BTreeMap<String, u64>,
    /// Each host a component asked to reach, and how many of its requests
    /// the host allowed and denied.
    pub hosts: Vec<HostAccess>,
    /// Credential headers held back from requests to hosts they aren't for.
    pub credentials_withheld: u64,
    /// Records the host denied: requests, limits and calls.
    pub denials: Listed,
    /// Actions that didn't end normally, e.g. a tool that trapped or timed
    /// out, an interrupted turn, or a full audit log.
    pub errors: Listed,
    /// Why the records might not be the whole story; see `audit::check`.
    pub trail_problems: Vec<String>,
    /// What each component in the tree was allowed, from the agent versions
    /// its runs used; empty if they predate agent versions.
    #[serde(default)]
    pub grants: Vec<Grant>,
}

/// What one component an agent ran with was allowed, written by the host
/// from its config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub agent: String,
    /// `provider`, `tool`, or `host` for a tool the host provides itself.
    pub kind: String,
    pub component: String,
    /// In plain words, e.g. `can reach api.tavily.com; gets an
    /// authorization header there; no filesystem access`.
    pub access: String,
}

impl Grant {
    /// One line, e.g. `agent reader: tool "read_file": no network access;
    /// reads /notes`.
    pub fn line(&self) -> String {
        format!(
            "agent {}: {} {}: {}",
            self.agent,
            self.kind,
            json!(self.component),
            self.access
        )
    }
}

/// What each component of each agent version in `versions` was allowed.
/// These come from the config each run used, not its trail, so they hold
/// when its sandboxes were made in an earlier run of the session (and its
/// trail has no `sandbox.create`), or when an attempt never reached the
/// host (a file operation with no `dir`).
pub fn grants(versions: &[AgentVersion]) -> Vec<Grant> {
    let mut grants = Vec::new();
    for version in versions {
        let definition = &version.definition;
        let grant = |kind: &str, component: &str, access: String| Grant {
            agent: version.agent.clone(),
            kind: kind.into(),
            component: component.into(),
            access,
        };
        let components = std::iter::once(&definition["provider"])
            .chain(definition["tools"].as_array().into_iter().flatten());
        for component in components {
            grants.push(
                match serde_json::from_value::<host::inspect::Capabilities>(component.clone()) {
                    Ok(c) => grant(&c.kind.to_string(), &c.name, c.summary()),
                    Err(_) => grant(
                        "component",
                        component["name"].as_str().unwrap_or("?"),
                        "unreadable in this version".into(),
                    ),
                },
            );
        }
        let delegates: Vec<_> = definition["delegates"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        for tool in definition["host_tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let access = match tool {
                "delegate" => format!("starts runs of {}", delegates.join(", ")),
                "wait" => "waits for the runs it delegated".into(),
                "audit_trail" => "reads other runs' audit trails".into(),
                "inspect_component" => "reads every configured component's capabilities".into(),
                _ => "provided by the host".into(),
            };
            grants.push(grant("host", tool, access));
        }
    }
    // A tree can run the same agent version more than once.
    let mut seen = Vec::new();
    grants.retain(|grant| {
        let new = !seen.contains(grant);
        if new {
            seen.push(grant.clone());
        }
        new
    });
    grants
}

/// Something in a summary that needs a person's attention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// `denied`, `trail` or `new_host`.
    pub kind: String,
    /// What happened, written by the host. A guest-influenced string in it,
    /// such as a host, is JSON-quoted.
    pub message: String,
}

/// A run in a tree that ended without completing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEnd {
    pub run_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostAccess {
    /// `provider` or `tool`.
    pub kind: String,
    pub component: String,
    pub host: String,
    pub allowed: u64,
    pub denied: u64,
}

/// Matching records, the first `MAX_LISTED` of them in full (see
/// `audit::compact`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Listed {
    pub total: usize,
    pub records: Vec<Value>,
}

impl Listed {
    fn push(&mut self, record: &Event) {
        self.total += 1;
        if self.records.len() < MAX_LISTED {
            self.records.push(crate::audit::compact(record));
        }
    }
}

fn denied(data: &Value) -> bool {
    data["decision"] == "denied"
}

fn error(data: &Value) -> bool {
    !denied(data)
        && data["action"] != "turn.started"
        && !matches!(
            data["outcome"].as_str(),
            Some("ok" | "completed" | "response")
        )
}

/// Summarizes `run` from the runs in its tree, `tree` (including `run`),
/// and their audit records, `records`, a run at a time. `known_hosts` are
/// the hosts its schedule's earlier runs reached, if it's a scheduled run
/// that has any earlier ones.
pub fn summarize(
    run: &Run,
    tree: &[Run],
    records: &[Event],
    known_hosts: Option<&BTreeSet<String>>,
) -> Summary {
    let mut summary = Summary {
        run_id: run.id.clone(),
        agent: run.agent.clone(),
        schedule_id: run.schedule_id.clone(),
        status: run.status.clone(),
        alerts: Vec::new(),
        runs: tree.len(),
        unfinished_runs: tree.iter().filter(|r| !r.terminal()).count(),
        not_completed: tree
            .iter()
            .filter(|r| r.terminal() && r.status != "completed")
            .map(|r| RunEnd {
                run_id: r.id.clone(),
                status: r.status.clone(),
            })
            .collect(),
        turns: 0,
        duration_ms: None,
        actions: BTreeMap::new(),
        hosts: Vec::new(),
        credentials_withheld: 0,
        denials: Listed::default(),
        errors: Listed::default(),
        // A run cancelled, or refused, before it started has nothing to
        // record; one that completed always has.
        trail_problems: if records.is_empty() && run.status != "completed" {
            Vec::new()
        } else {
            crate::audit::check(records)
        },
        grants: Vec::new(),
    };
    let mut hosts: BTreeMap<(String, String, String), (u64, u64)> = BTreeMap::new();
    let times = records.iter().filter_map(|r| r.data["ts_ms"].as_u64());
    if let (Some(first), Some(last)) = (times.clone().min(), times.max()) {
        summary.duration_ms = Some(last - first);
    }
    for record in records {
        let data = &record.data;
        let action = data["action"].as_str().unwrap_or("?");
        *summary.actions.entry(action.to_owned()).or_default() += 1;
        if action == "turn.started" {
            summary.turns += 1;
        }
        if action == "http.request" {
            let text = |value: &Value| value.as_str().map_or(value.to_string(), String::from);
            let counts = hosts
                .entry((
                    text(&data["component"]["kind"]),
                    text(&data["component"]["name"]),
                    text(&data["host"]),
                ))
                .or_default();
            if denied(data) {
                counts.1 += 1;
            } else {
                counts.0 += 1;
            }
        }
        summary.credentials_withheld += data["credentials_withheld"].as_u64().unwrap_or(0);
        if denied(data) {
            summary.denials.push(record);
        } else if error(data) {
            summary.errors.push(record);
        }
    }
    summary.hosts = hosts
        .into_iter()
        .map(|((kind, component, host), (allowed, denied))| HostAccess {
            kind,
            component,
            host,
            allowed,
            denied,
        })
        .collect();
    summary.alerts = alerts(&summary, known_hosts);
    summary
}

fn alerts(summary: &Summary, known_hosts: Option<&BTreeSet<String>>) -> Vec<Alert> {
    let quoted = |text: &str| json!(text).to_string();
    let alert = |kind: &str, message: String| Alert {
        kind: kind.into(),
        message,
    };
    let mut alerts = Vec::new();
    if summary.denials.total > 0 {
        alerts.push(alert(
            "denied",
            format!(
                "the host denied {} action{}",
                summary.denials.total,
                if summary.denials.total == 1 { "" } else { "s" }
            ),
        ));
    }
    // A run still going always looks unfinished; its trail is checked once
    // the whole tree has ended.
    if summary.unfinished_runs == 0 && !summary.trail_problems.is_empty() {
        alerts.push(alert(
            "trail",
            format!(
                "the audit trail is incomplete ({} problem{}), so this summary may be too",
                summary.trail_problems.len(),
                if summary.trail_problems.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ),
        ));
    }
    if let Some(known) = known_hosts {
        let mut new = BTreeSet::new();
        for access in &summary.hosts {
            if access.allowed > 0 && !known.contains(&access.host) && new.insert(&access.host) {
                alerts.push(alert(
                    "new_host",
                    format!(
                        "{} {} reached {}, which no earlier run of this schedule did",
                        access.kind,
                        quoted(&access.component),
                        quoted(&access.host)
                    ),
                ));
            }
        }
    }
    alerts
}

/// `summary` as text: a headline and its alerts, then the hosts reached,
/// denials, errors and trail problems. Guest-influenced strings stay JSON-quoted, so none
/// can start a line of its own.
pub fn render(summary: &Summary) -> String {
    let quoted = |text: &str| json!(text).to_string();
    let mut out = format!(
        "run {} (agent {}{}): {}\n",
        summary.run_id,
        summary.agent,
        summary
            .schedule_id
            .as_ref()
            .map_or(String::new(), |s| format!(", schedule {s}")),
        summary.status,
    );
    let duration = summary.duration_ms.map_or("no audit records".into(), |ms| {
        format!("{}.{:03}s", ms / 1000, ms % 1000)
    });
    out.push_str(&format!(
        "{} run{}, {} turn{}, {duration}",
        summary.runs,
        if summary.runs == 1 { "" } else { "s" },
        summary.turns,
        if summary.turns == 1 { "" } else { "s" },
    ));
    if summary.unfinished_runs > 0 {
        out.push_str(&format!(
            "; {} still queued or running",
            summary.unfinished_runs
        ));
    }
    out.push('\n');
    let delegates: Vec<_> = summary
        .not_completed
        .iter()
        .filter(|end| end.run_id != summary.run_id)
        .map(|end| format!("{} {}", end.run_id, end.status))
        .collect();
    if !delegates.is_empty() {
        out.push_str(&format!(
            "delegated runs not completed: {}\n",
            delegates.join(", ")
        ));
    }
    if !summary.actions.is_empty() {
        let actions: Vec<_> = summary
            .actions
            .iter()
            .map(|(action, n)| format!("{action} {n}"))
            .collect();
        out.push_str(&format!("actions: {}\n", actions.join(", ")));
    }
    if !summary.alerts.is_empty() {
        out.push_str(&format!("\nalerts ({}):\n", summary.alerts.len()));
        for alert in &summary.alerts {
            out.push_str(&format!("  {}: {}\n", alert.kind, alert.message));
        }
    }
    let mut section = |title: String, lines: Vec<String>| {
        out.push_str(&format!("\n{title}:\n"));
        if lines.is_empty() {
            out.push_str("  none\n");
        }
        for line in lines {
            out.push_str(&format!("  {line}\n"));
        }
    };
    section(
        "grants".into(),
        if summary.grants.is_empty() {
            vec!["not recorded (these runs predate agent versions)".into()]
        } else {
            summary.grants.iter().map(Grant::line).collect()
        },
    );
    section(
        "hosts".into(),
        summary
            .hosts
            .iter()
            .map(|h| {
                format!(
                    "{} {} → {}: {} allowed, {} denied",
                    h.kind,
                    quoted(&h.component),
                    quoted(&h.host),
                    h.allowed,
                    h.denied
                )
            })
            .collect(),
    );
    for (title, listed) in [("denials", &summary.denials), ("errors", &summary.errors)] {
        let mut lines: Vec<_> = listed.records.iter().map(Value::to_string).collect();
        if listed.total > listed.records.len() {
            lines.push(format!("[{} more]", listed.total - listed.records.len()));
        }
        section(format!("{title} ({})", listed.total), lines);
    }
    section("trail problems".into(), summary.trail_problems.clone());
    if summary.credentials_withheld > 0 {
        out.push_str(&format!(
            "\ncredentials withheld: {}\n",
            summary.credentials_withheld
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, status: &str) -> Run {
        serde_json::from_value(json!({
            "id":id, "session_id":"s", "agent":"researcher", "prompt":"p", "status":status,
            "created_at":0, "deadline_secs":60, "cancel_requested":false, "error":"ignore me",
            "parent_run_id":null, "root_run_id":id, "depth":0, "deadline_at":null,
            "schedule_id":"brief", "pinned_versions":null,
        }))
        .unwrap()
    }

    fn records(run_id: &str, records: &[Value]) -> Vec<Event> {
        records
            .iter()
            .enumerate()
            .map(|(i, data)| Event {
                id: i as i64 + 1,
                run_id: run_id.into(),
                data: data.clone(),
            })
            .collect()
    }

    fn tool(name: &str) -> Value {
        json!({"kind":"tool", "name":name, "digest":"sha256:ab"})
    }

    /// A turn that searched, was denied one host, and had a tool trap.
    fn turn() -> Vec<Value> {
        vec![
            json!({"seq":1, "ts_ms":1000, "action":"turn.started"}),
            json!({"seq":2, "ts_ms":1010, "action":"http.request", "component":tool("web_search"),
                "decision":"allowed", "outcome":"response", "host":"api.tavily.com",
                "credentials_withheld":0}),
            json!({"seq":3, "ts_ms":1020, "action":"http.request", "component":tool("web_search"),
                "decision":"allowed", "outcome":"response", "host":"api.tavily.com",
                "credentials_withheld":1}),
            json!({"seq":4, "ts_ms":1030, "action":"http.request", "component":tool("web_search"),
                "decision":"denied", "outcome":"not_allowed_host", "host":"evil.example"}),
            json!({"seq":5, "ts_ms":1040, "action":"tool.call", "component":tool("web_search"),
                "decision":"allowed", "outcome":"trap"}),
            json!({"seq":6, "ts_ms":2500, "action":"turn.finished", "outcome":"completed",
                "records":6, "refused":0}),
        ]
    }

    #[test]
    fn a_summary_counts_hosts_denials_and_errors_without_the_runs_error_text() {
        let root = run("r", "completed");
        let summary = summarize(
            &root,
            std::slice::from_ref(&root),
            &records("r", &turn()),
            None,
        );
        assert_eq!(
            (summary.runs, summary.unfinished_runs, summary.turns),
            (1, 0, 1)
        );
        assert_eq!(summary.duration_ms, Some(1500));
        assert_eq!(summary.actions["http.request"], 3);
        assert_eq!(summary.credentials_withheld, 1);
        let hosts: Vec<_> = summary
            .hosts
            .iter()
            .map(|h| (h.host.as_str(), h.allowed, h.denied))
            .collect();
        assert_eq!(hosts, [("api.tavily.com", 2, 0), ("evil.example", 0, 1)]);
        assert_eq!(summary.denials.total, 1);
        assert_eq!(summary.denials.records[0]["seq"], 4);
        assert_eq!(summary.denials.records[0]["run"], "r");
        assert_eq!(summary.errors.total, 1);
        assert_eq!(summary.errors.records[0]["outcome"], "trap");
        assert!(summary.trail_problems.is_empty());
        let text = serde_json::to_string(&summary).unwrap();
        assert!(!text.contains("ignore me"), "{text}");
    }

    #[test]
    fn a_tree_is_summarized_whole_and_its_trail_problems_name_their_runs() {
        let (root, child) = (run("r", "completed"), run("c", "running"));
        let mut trail = records("r", &turn());
        // The child is still running: no turn.finished yet.
        trail.extend(records("c", &turn()[..2]));
        let summary = summarize(&root, &[root.clone(), child.clone()], &trail, None);
        assert!(summary.not_completed.is_empty());
        // A child that ended badly under a parent that completed.
        let failed = Run {
            status: "failed".into(),
            ..child
        };
        let ended = summarize(&root, &[root.clone(), failed], &trail, None);
        assert_eq!(
            ended.not_completed,
            [RunEnd {
                run_id: "c".into(),
                status: "failed".into()
            }]
        );
        assert!(render(&ended).contains("\ndelegated runs not completed: c failed\n"));
        assert_eq!(
            (summary.runs, summary.unfinished_runs, summary.turns),
            (2, 1, 2)
        );
        assert_eq!(summary.hosts[0].allowed, 3);
        assert_eq!(summary.trail_problems.len(), 1);
        assert!(
            summary.trail_problems[0].starts_with("run c: "),
            "{:?}",
            summary.trail_problems
        );
    }

    fn kinds(summary: &Summary) -> Vec<&str> {
        summary.alerts.iter().map(|a| a.kind.as_str()).collect()
    }

    #[test]
    fn denials_raise_an_alert() {
        let root = run("r", "completed");
        let summary = summarize(
            &root,
            std::slice::from_ref(&root),
            &records("r", &turn()),
            None,
        );
        assert_eq!(
            summary.alerts,
            [Alert {
                kind: "denied".into(),
                message: "the host denied 1 action".into()
            }]
        );
        let mut quiet = turn();
        quiet.remove(3);
        for (seq, record) in quiet.iter_mut().enumerate() {
            record["seq"] = json!(seq + 1);
        }
        quiet.last_mut().unwrap()["records"] = json!(5);
        let summary = summarize(
            &root,
            std::slice::from_ref(&root),
            &records("r", &quiet),
            None,
        );
        assert!(summary.alerts.is_empty(), "{:?}", summary.alerts);
    }

    #[test]
    fn an_incomplete_trail_raises_an_alert_once_the_tree_has_ended() {
        let (root, child) = (run("r", "completed"), run("c", "running"));
        let mut trail = records("r", &turn());
        trail.extend(records("c", &turn()[..2]));
        let tree = [root.clone(), child];
        assert_eq!(kinds(&summarize(&root, &tree, &trail, None)), ["denied"]);
        let tree = [root.clone(), run("c", "interrupted")];
        let summary = summarize(&root, &tree, &trail, None);
        assert_eq!(kinds(&summary), ["denied", "trail"]);
        assert_eq!(
            summary.alerts[1].message,
            "the audit trail is incomplete (1 problem), so this summary may be too"
        );
    }

    #[test]
    fn a_host_no_earlier_run_reached_raises_an_alert() {
        let root = run("r", "completed");
        let summarize = |known: Option<&BTreeSet<String>>| {
            summarize(
                &root,
                std::slice::from_ref(&root),
                &records("r", &turn()),
                known,
            )
        };
        // The schedule's first run: nothing to compare with.
        assert_eq!(kinds(&summarize(None)), ["denied"]);
        let known = BTreeSet::from(["api.tavily.com".to_string()]);
        // evil.example is new but was denied, which is already an alert.
        assert_eq!(kinds(&summarize(Some(&known))), ["denied"]);
        let summary = summarize(Some(&BTreeSet::new()));
        assert_eq!(kinds(&summary), ["denied", "new_host"]);
        assert_eq!(
            summary.alerts[1].message,
            r#"tool "web_search" reached "api.tavily.com", which no earlier run of this schedule did"#
        );
        let text = render(&summary);
        assert!(
            text.contains("\nalerts (2):\n  denied: the host denied 1 action\n  new_host: tool "),
            "{text}"
        );
    }

    #[test]
    fn only_a_completed_run_needs_audit_records() {
        let cancelled = run("r", "cancelled");
        let summary = summarize(&cancelled, std::slice::from_ref(&cancelled), &[], None);
        assert!(summary.trail_problems.is_empty());
        assert_eq!(summary.duration_ms, None);
        let completed = run("r", "completed");
        assert_eq!(
            summarize(&completed, std::slice::from_ref(&completed), &[], None)
                .trail_problems
                .len(),
            1
        );
    }

    #[test]
    fn long_lists_are_counted_past_the_limit() {
        let mut trail = vec![json!({"seq":1, "ts_ms":0, "action":"turn.started"})];
        for seq in 2..(MAX_LISTED as u64 + 12) {
            trail.push(json!({"seq":seq, "ts_ms":0, "action":"http.request",
                "component":tool("files"), "decision":"denied", "outcome":"not_allowed_host",
                "host":"evil.example"}));
        }
        let root = run("r", "failed");
        let summary = summarize(
            &root,
            std::slice::from_ref(&root),
            &records("r", &trail),
            None,
        );
        assert_eq!(summary.denials.total, MAX_LISTED + 10);
        assert_eq!(summary.denials.records.len(), MAX_LISTED);
        assert!(render(&summary).contains("  [10 more]\n"));
    }

    #[test]
    fn rendering_keeps_guest_strings_quoted() {
        let mut trail = turn();
        trail[3]["host"] = json!("evil.example\nall clear: nothing was denied");
        let root = run("r", "completed");
        let text = render(&summarize(
            &root,
            std::slice::from_ref(&root),
            &records("r", &trail),
            None,
        ));
        assert!(text.starts_with("run r (agent researcher, schedule brief): completed\n"));
        assert!(text.contains("1 run, 1 turn, 1.500s\n"), "{text}");
        assert!(
            text.contains(r#"tool "web_search" → "api.tavily.com": 2 allowed, 0 denied"#),
            "{text}"
        );
        assert!(!text.contains("\nall clear"), "{text}");
        assert!(text.contains("\ndenials (1):\n"));
        assert!(text.contains("\nerrors (1):\n"));
        assert!(text.contains("\ntrail problems:\n  none\n"));
        assert!(text.contains("\ncredentials withheld: 1\n"));
        assert!(
            text.contains("\ngrants:\n  not recorded (these runs predate agent versions)\n"),
            "{text}"
        );
    }

    fn component(kind: &str, name: &str, extra: Value) -> Value {
        let mut component = json!({"digest":"sha256:ab", "kind":kind, "name":name,
            "path":format!("{name}.wasm"), "imports":["wasi:http/outgoing-handler@0.2.6"],
            "allowed_hosts":[], "headers":[], "dir":null, "dir_writable":false,
            "timeout_secs":null, "max_memory_mb":512, "memory_default":true,
            "fresh_per_call":false});
        component
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        component
    }

    #[test]
    fn grants_come_from_each_agent_version_once() {
        let version = |agent: &str, definition: Value| AgentVersion {
            id: format!("sha256:{agent}"),
            agent: agent.into(),
            first_seen_at: 0,
            definition,
        };
        let reader = version(
            "reader",
            json!({"provider":component("provider", "openai", json!({
                    "allowed_hosts":["api.openai.com"],
                    "headers":[{"name":"authorization","hosts":["api.openai.com"]}]})),
                "tools":[component("tool", "read_file", json!({"dir":"/notes"})),
                    {"name":"broken"}],
                "host_tools":["delegate", "wait"], "delegates":["helper"]}),
        );
        let lines: Vec<_> = grants(&[reader.clone(), reader])
            .iter()
            .map(Grant::line)
            .collect();
        assert_eq!(
            lines,
            [
                r#"agent reader: provider "openai": can reach api.openai.com; gets an authorization header there; no filesystem access"#,
                r#"agent reader: tool "read_file": no network access; reads /notes"#,
                r#"agent reader: component "broken": unreadable in this version"#,
                r#"agent reader: host "delegate": starts runs of helper"#,
                r#"agent reader: host "wait": waits for the runs it delegated"#,
            ]
        );
        let root = run("r", "completed");
        let mut summary = summarize(&root, std::slice::from_ref(&root), &[], None);
        summary.grants = grants(&[version(
            "auditor",
            json!({"provider":component("provider", "fixture", json!({})),
                "tools":[], "host_tools":["audit_trail"], "delegates":[]}),
        )]);
        let text = render(&summary);
        assert!(
            text.contains(
                "\ngrants:\n  agent auditor: provider \"fixture\": no network access; no \
                 filesystem access\n  agent auditor: host \"audit_trail\": reads other runs' \
                 audit trails\n\nhosts:\n"
            ),
            "{text}"
        );
    }
}
