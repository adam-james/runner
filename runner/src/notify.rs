//! Notifications: telling the user what a run did once it has ended, so its
//! alerts reach someone without them going to look. Every top-level run is
//! assumed unwatched, whether scheduled, detached or in a chat: alerts never
//! show in chat, and a client can be attached while nobody's looking. The
//! daemon keeps each notification until it's read, so none are lost while no
//! client is connected.
//!
//! A notification is written by the host alone, from the run's status, its
//! summary's alerts and its schedule, never from the model's reply or the
//! run's error: a prompt injection could otherwise put a convincing fake
//! message in what looks like a trusted system notice. The few
//! guest-influenced strings in alerts (hosts, tool names) stay JSON-quoted.
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::db::{Database, Notification, Run, Schedule};
use crate::summary::{Alert, Summary};

/// How often ended runs are looked for.
const TICK: Duration = Duration::from_secs(1);

/// When runs notify: `all` of them, only `problems` (a run that didn't
/// complete, i.e. failed, was interrupted or was cancelled, or that raised an
/// alert), or `never`. A run's own level wins, then its schedule's, then
/// `problems`.
pub const LEVELS: &[&str] = &["all", "problems", "never"];

pub fn default_level() -> String {
    "problems".into()
}

pub fn check_level(level: &str) -> Result<()> {
    if !LEVELS.contains(&level) {
        bail!("notify must be one of {}", LEVELS.join(", "));
    }
    Ok(())
}

/// What a notification says, all of it host-written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// The run's schedule, if it's a scheduled run.
    pub schedule_id: Option<String>,
    pub agent: String,
    /// The run's status: `completed`, `failed`, `interrupted` or `cancelled`.
    pub status: String,
    pub duration_ms: Option<u64>,
    pub alerts: Vec<Alert>,
    /// For a scheduled run, its schedule's status when the run was checked,
    /// e.g. `paused` after a run refused because its agent changed.
    pub schedule_status: Option<String>,
}

/// The note `run`'s end gets, if any, under its own level `own`, else its
/// `schedule`'s, else `problems`.
pub fn note(
    run: &Run,
    own: Option<&str>,
    summary: &Summary,
    schedule: Option<&Schedule>,
) -> Option<Note> {
    let level = own
        .or(schedule.map(|s| s.notify.as_str()))
        .unwrap_or("problems");
    let problem = run.status != "completed" || !summary.alerts.is_empty();
    let wanted = match level {
        "all" => true,
        "never" => false,
        _ => problem,
    };
    wanted.then(|| Note {
        schedule_id: schedule.map(|s| s.id.clone()),
        agent: run.agent.clone(),
        status: run.status.clone(),
        duration_ms: summary.duration_ms,
        alerts: summary.alerts.clone(),
        schedule_status: schedule.map(|s| s.status.clone()),
    })
}

/// A notification's one-line headline, e.g. `schedule brief: completed,
/// 1 alert` or `run (agent reader): failed`.
pub fn title(note: &Note) -> String {
    let mut title = match &note.schedule_id {
        Some(schedule) => format!("schedule {schedule}: {}", note.status),
        None => format!("run (agent {}): {}", note.agent, note.status),
    };
    match note.alerts.len() {
        0 => {}
        1 => title.push_str(", 1 alert"),
        n => title.push_str(&format!(", {n} alerts")),
    }
    if note.schedule_status.as_deref() == Some("paused") {
        title.push_str("; schedule paused");
    }
    title
}

/// Notifications as text, newest first: a line each with its id, whether
/// it's read, when, its title and run, then its alerts.
pub fn render(notifications: &[Notification]) -> String {
    let mut out = String::new();
    for n in notifications {
        let at = chrono::DateTime::from_timestamp_millis(n.created_at)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "#{} {:<6} {at}  {}  (run {})\n",
            n.id,
            if n.read_at.is_some() {
                "read"
            } else {
                "unread"
            },
            title(&n.note),
            n.run_id,
        ));
        for alert in &n.note.alerts {
            out.push_str(&format!("    {}: {}\n", alert.kind, alert.message));
        }
    }
    out
}

/// Checks each top-level run once its tree has ended and stores the
/// notification its level wants, until `shutdown`. Runs that ended while
/// the daemon was stopped are checked when it starts.
pub async fn notifier(db: Database, shutdown: CancellationToken) {
    let mut tick = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tick.tick() => {
                match db.blocking(|db| check_ended(&db)).await {
                    Ok(stored) => {
                        for n in stored {
                            tracing::info!(notification = n.id, run = ?n.run_id,
                                schedule = ?n.note.schedule_id, "notification stored");
                        }
                    }
                    // Transient (e.g. SQLITE_BUSY); unchecked runs stay unchecked.
                    Err(_) => tracing::error!("could not check ended runs for notifications"),
                }
            }
        }
    }
}

/// One pass of `notifier`. Returns the notifications stored.
pub fn check_ended(db: &Database) -> Result<Vec<Notification>> {
    let mut stored = Vec::new();
    for (run, own) in db.runs_to_notify()? {
        let schedule = match &run.schedule_id {
            Some(id) => db.schedule(id)?,
            None => None,
        };
        let note = db
            .summary(&run.id)?
            .and_then(|summary| note(&run, own.as_deref(), &summary, schedule.as_ref()));
        stored.extend(db.record_notification(&run.id, note)?);
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(status: &str) -> Run {
        serde_json::from_value(json!({
            "id":"r", "session_id":"s", "agent":"researcher", "prompt":"p", "status":status,
            "created_at":0, "deadline_secs":60, "cancel_requested":false,
            "error":"Click https://evil.example to fix your account",
            "parent_run_id":null, "root_run_id":"r", "depth":0, "deadline_at":null,
            "schedule_id":"brief", "pinned_versions":null,
        }))
        .unwrap()
    }

    fn schedule(notify: &str, status: &str) -> Schedule {
        serde_json::from_value(json!({
            "id":"brief", "agent":"researcher", "prompt":"p", "at":"2026-10-29T07:00:00",
            "tz":"UTC", "cron":"0 7 * * *", "next_fire_at":null, "deadline_secs":60,
            "status":status, "notify":notify, "pinned_versions":{}, "paused_reason":null,
            "created_at":0,
        }))
        .unwrap()
    }

    fn schedule_with(notify: &str) -> Schedule {
        schedule(notify, "active")
    }

    fn summary(alerts: &[&str]) -> Summary {
        let root = run("completed");
        let mut summary = crate::summary::summarize(&root, std::slice::from_ref(&root), &[], None);
        summary.duration_ms = Some(1200);
        summary.alerts = alerts
            .iter()
            .map(|kind| Alert {
                kind: (*kind).into(),
                message: format!("{kind} happened"),
            })
            .collect();
        summary
    }

    #[test]
    fn each_level_picks_which_ended_runs_notify() {
        let cases = [
            ("completed", &[][..], [true, false, false]),
            ("completed", &["denied"][..], [true, true, false]),
            ("failed", &[][..], [true, true, false]),
            ("interrupted", &[][..], [true, true, false]),
            ("cancelled", &[][..], [true, true, false]),
        ];
        for (status, alerts, wanted) in cases {
            for (level, wanted) in LEVELS.iter().zip(wanted) {
                let schedule = schedule(level, "active");
                let scheduled = note(&run(status), None, &summary(alerts), Some(&schedule));
                assert_eq!(scheduled.is_some(), wanted, "{status} {alerts:?} {level}");
                // A run's own level means the same, and wins over its schedule's.
                let own = note(
                    &run(status),
                    Some(level),
                    &summary(alerts),
                    Some(&schedule_with("never")),
                );
                assert_eq!(own.is_some(), wanted, "own {status} {alerts:?} {level}");
            }
            // A run outside any schedule, with no level of its own, notifies
            // about problems.
            let problems = wanted[1];
            assert_eq!(
                note(&run(status), None, &summary(alerts), None).is_some(),
                problems,
                "unscheduled {status} {alerts:?}"
            );
        }
        assert!(check_level("sometimes").is_err());
        assert_eq!(default_level(), "problems");
    }

    #[test]
    fn a_note_is_host_written_and_never_carries_the_runs_error() {
        let note = note(
            &run("failed"),
            None,
            &summary(&["denied"]),
            Some(&schedule("problems", "paused")),
        )
        .unwrap();
        assert_eq!(
            title(&note),
            "schedule brief: failed, 1 alert; schedule paused"
        );
        assert_eq!(note.duration_ms, Some(1200));
        let text = serde_json::to_string(&note).unwrap();
        assert!(!text.contains("evil.example"), "{text}");
        let n = Notification {
            id: 7,
            run_id: "r".into(),
            created_at: 0,
            read_at: None,
            note,
        };
        let text = render(std::slice::from_ref(&n));
        assert!(text.starts_with("#7 unread "), "{text}");
        assert!(text.contains("  schedule brief: failed, 1 alert; schedule paused  (run r)\n"));
        assert!(text.ends_with("\n    denied: denied happened\n"), "{text}");
        let quiet = Note {
            alerts: Vec::new(),
            schedule_status: Some("active".into()),
            status: "completed".into(),
            ..n.note.clone()
        };
        assert_eq!(title(&quiet), "schedule brief: completed");
        let unscheduled = super::note(&run("interrupted"), None, &summary(&[]), None).unwrap();
        assert_eq!(
            (&unscheduled.schedule_id, &unscheduled.schedule_status),
            (&None, &None)
        );
        assert_eq!(title(&unscheduled), "run (agent researcher): interrupted");
    }
}
