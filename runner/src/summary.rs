//! `runner runs summary`: what a run, and every run it delegated to, did,
//! worked out by the host from their audit records with no model involved,
//! so a background run that nobody watched can be checked at a glance.
//!
//! Like the records it's made from, a summary is metadata only. It leaves
//! out the run's error too, since a failed turn's error is the agent's own
//! message and a prompt injection could steer it. The few guest-influenced
//! strings it keeps (hosts, tool names) are rendered JSON-quoted.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::{Event, Run};

/// Denials and errors past this many are counted, not listed.
pub const MAX_LISTED: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub run_id: String,
    pub agent: String,
    pub schedule_id: Option<String>,
    /// The run's status, e.g. `completed` or `failed`.
    pub status: String,
    /// Runs in its tree: it and every run it delegated to, transitively.
    pub runs: usize,
    /// Of those, the ones still queued or running, whose records may be
    /// incomplete.
    pub unfinished_runs: usize,
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
/// and their audit records, `records`, a run at a time.
pub fn summarize(run: &Run, tree: &[Run], records: &[Event]) -> Summary {
    let mut summary = Summary {
        run_id: run.id.clone(),
        agent: run.agent.clone(),
        schedule_id: run.schedule_id.clone(),
        status: run.status.clone(),
        runs: tree.len(),
        unfinished_runs: tree.iter().filter(|r| !r.terminal()).count(),
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
    summary
}

/// `summary` as text: a headline, then the hosts reached, denials, errors
/// and trail problems. Guest-influenced strings stay JSON-quoted, so none
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
    if !summary.actions.is_empty() {
        let actions: Vec<_> = summary
            .actions
            .iter()
            .map(|(action, n)| format!("{action} {n}"))
            .collect();
        out.push_str(&format!("actions: {}\n", actions.join(", ")));
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
        let summary = summarize(&root, std::slice::from_ref(&root), &records("r", &turn()));
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
        let summary = summarize(&root, &[root.clone(), child], &trail);
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

    #[test]
    fn only_a_completed_run_needs_audit_records() {
        let cancelled = run("r", "cancelled");
        let summary = summarize(&cancelled, std::slice::from_ref(&cancelled), &[]);
        assert!(summary.trail_problems.is_empty());
        assert_eq!(summary.duration_ms, None);
        let completed = run("r", "completed");
        assert_eq!(
            summarize(&completed, std::slice::from_ref(&completed), &[])
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
        let summary = summarize(&root, std::slice::from_ref(&root), &records("r", &trail));
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
    }
}
