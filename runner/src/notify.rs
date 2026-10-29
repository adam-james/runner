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
//!
//! Each notification is also posted to the desktop once, unless
//! `[notifications] desktop = false`, on macOS only: to Notification
//! Center, in-process through `notify-rust`, never by running a command.
//! Its text is plain strings, with no script or markup to escape from.
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::db::{Database, Notification, Run, Schedule};
use crate::summary::{Alert, RunEnd, Summary};

/// How often ended runs are looked for.
const TICK: Duration = Duration::from_secs(1);

/// When runs notify: `all` of them, only `problems` (a run that didn't
/// complete, i.e. failed, was interrupted or was cancelled, or one it
/// delegated to that didn't, or that raised an alert), or `never`. A run's own level wins, then its schedule's, then
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
    /// Runs it delegated to, transitively, that ended without completing.
    #[serde(default)]
    pub delegated_not_completed: Vec<RunEnd>,
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
    let delegated: Vec<_> = summary
        .not_completed
        .iter()
        .filter(|end| end.run_id != run.id)
        .cloned()
        .collect();
    let problem = run.status != "completed" || !delegated.is_empty() || !summary.alerts.is_empty();
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
        delegated_not_completed: delegated,
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
    match note.delegated_not_completed.len() {
        0 => {}
        1 => title.push_str(", 1 delegated run didn't complete"),
        n => title.push_str(&format!(", {n} delegated runs didn't complete")),
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
        for end in &n.note.delegated_not_completed {
            out.push_str(&format!(
                "    delegated run {} {}\n",
                end.run_id, end.status
            ));
        }
    }
    out
}

/// Checks each top-level run once its tree has ended and stores the
/// notification its level wants, then posts new ones to `desktop`, if
/// there is one, until `shutdown`. Runs that ended while the daemon was
/// stopped are checked when it starts.
pub async fn notifier(
    db: Database,
    desktop: Option<Arc<dyn Desktop>>,
    shutdown: CancellationToken,
) {
    let mut tick = tokio::time::interval(TICK);
    // Whether the last post failed, so a desktop that can't show
    // notifications is logged about once rather than on every one.
    let mut failing = false;
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
                let Some(desktop) = desktop.clone() else { continue };
                match db.blocking(move |db| deliver(&db, desktop.as_ref())).await {
                    Ok(None) => failing = false,
                    Ok(Some(error)) => {
                        if !failing {
                            tracing::warn!(error = %error, "could not post a desktop \
                                notification; it's still stored (runner notifications)");
                        }
                        failing = true;
                    }
                    Err(_) => tracing::error!("could not read notifications to post"),
                }
            }
        }
    }
}

/// Where desktop notifications are posted.
pub trait Desktop: Send + Sync {
    fn post(&self, title: &str, body: &str) -> Result<()>;
}

/// More notifications than this waiting at once, e.g. after the daemon
/// was stopped a while, are posted as one.
pub const MAX_POSTED: usize = 3;

/// The longest a notification's body gets, in characters.
const MAX_BODY: usize = 300;

/// Posts each notification not yet posted to `desktop`, unless it's already
/// been read, and marks it posted. A failed post is marked too: it's still
/// stored, and retrying a desktop that can't show notifications would only
/// fail again. Returns the last failure, if any.
pub fn deliver(db: &Database, desktop: &dyn Desktop) -> Result<Option<anyhow::Error>> {
    let waiting = db.undelivered()?;
    let ids: Vec<i64> = waiting.iter().map(|n| n.id).collect();
    let unread: Vec<_> = waiting.iter().filter(|n| n.read_at.is_none()).collect();
    let mut failure = None;
    let mut post = |title: &str, body: &str| {
        if let Err(error) = desktop.post(title, body) {
            failure = Some(error);
        }
    };
    if unread.len() > MAX_POSTED {
        post(
            &format!("runner: {} new notifications", unread.len()),
            "Run `runner notifications` to see them.",
        );
    } else {
        for n in unread {
            post(&format!("runner: {}", title(&n.note)), &body(&n.note));
        }
    }
    db.mark_delivered(&ids)?;
    Ok(failure)
}

/// A notification's alerts and delegated runs that didn't complete, a line
/// each, cut to `MAX_BODY` characters; or its duration if it has neither.
pub fn body(note: &Note) -> String {
    let mut lines: Vec<String> = note
        .alerts
        .iter()
        .map(|alert| format!("{}: {}", alert.kind, alert.message))
        .collect();
    lines.extend(
        note.delegated_not_completed
            .iter()
            .map(|end| format!("delegated run {} {}", end.run_id, end.status)),
    );
    if lines.is_empty() {
        lines.push(match note.duration_ms {
            Some(ms) => format!("took {}.{:03}s, no alerts", ms / 1000, ms % 1000),
            None => "no alerts".into(),
        });
    }
    let body = lines.join("\n");
    match body.char_indices().nth(MAX_BODY) {
        Some((cut, _)) => format!("{}…", &body[..cut]),
        None => body,
    }
}

/// The app notifications post as, which a click brings forward: the
/// configured one, else the terminal the daemon was started from (macOS
/// gives an app's processes its bundle ID in `__CFBundleIdentifier`, and
/// terminals name themselves in `TERM_PROGRAM`), else Terminal. Always set
/// explicitly, since the crate's default looks one up by running an
/// AppleScript.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn desktop_app(configured: Option<&str>, env: impl Fn(&str) -> Option<String>) -> String {
    if let Some(app) = configured {
        return app.to_string();
    }
    if let Some(app) = env("__CFBundleIdentifier").filter(|app| !app.is_empty()) {
        return app;
    }
    let known = match env("TERM_PROGRAM").as_deref() {
        Some("ghostty") => "com.mitchellh.ghostty",
        Some("iTerm.app") => "com.googlecode.iterm2",
        Some("WezTerm") => "com.github.wez.wezterm",
        _ => "com.apple.Terminal",
    };
    known.to_string()
}

/// macOS Notification Center, posted in-process as `app` (a bundle ID),
/// playing `sound` (`default` or a sound's name) if any.
#[cfg(target_os = "macos")]
pub struct SystemDesktop {
    pub app: String,
    pub sound: Option<String>,
}

#[cfg(target_os = "macos")]
impl Desktop for SystemDesktop {
    fn post(&self, title: &str, body: &str) -> Result<()> {
        // The crate can only set the app once per process.
        static APP: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        if let Err(error) =
            APP.get_or_init(|| notify_rust::set_application(&self.app).map_err(|e| e.to_string()))
        {
            bail!("posting as {}: {error}", self.app);
        }
        let mut notification = notify_rust::Notification::new();
        notification.appname("runner").summary(title).body(body);
        match self.sound.as_deref() {
            Some("default") => notification.sound_name("NSUserNotificationDefaultSoundName"),
            Some(sound) => notification.sound_name(sound),
            None => &mut notification,
        };
        // This posts when the handle drops, and the crate keeps any error
        // to itself.
        notification.show()?;
        Ok(())
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
        // A run that completed, with a delegate that didn't, is a problem.
        let mut tree = summary(&[]);
        tree.not_completed = vec![RunEnd {
            run_id: "c".into(),
            status: "failed".into(),
        }];
        let delegated = super::note(&run("completed"), None, &tree, None).unwrap();
        assert_eq!(
            title(&delegated),
            "run (agent researcher): completed, 1 delegated run didn't complete"
        );
        let n = Notification {
            note: delegated,
            ..n
        };
        assert!(render(&[n]).ends_with("\n    delegated run c failed\n"));
        // The root's own end isn't counted as a delegate's.
        tree.not_completed[0].run_id = "r".into();
        let own = super::note(&run("failed"), None, &tree, None).unwrap();
        assert!(own.delegated_not_completed.is_empty());
    }

    /// Records each post, failing them all if `fail` is set.
    #[derive(Default)]
    struct FakeDesktop {
        posts: std::sync::Mutex<Vec<(String, String)>>,
        fail: bool,
    }

    impl Desktop for FakeDesktop {
        fn post(&self, title: &str, body: &str) -> Result<()> {
            self.posts.lock().unwrap().push((title.into(), body.into()));
            if self.fail {
                bail!("no notification server");
            }
            Ok(())
        }
    }

    impl FakeDesktop {
        fn take(&self) -> Vec<(String, String)> {
            std::mem::take(&mut self.posts.lock().unwrap())
        }
    }

    /// `n` notifications in a fresh database, each for a failed run that
    /// raised one alert.
    fn notified(n: usize) -> (tempfile::TempDir, Database, Vec<Notification>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("state.sqlite3")).unwrap();
        let stored = (0..n)
            .map(|_| {
                let run = db.submit(None, "default".into(), "p".into(), 60).unwrap();
                let note = super::note(&run_with(&run.id), None, &summary(&["denied"]), None);
                db.record_notification(&run.id, note).unwrap().unwrap()
            })
            .collect();
        (dir, db, stored)
    }

    fn run_with(id: &str) -> Run {
        Run {
            id: id.into(),
            schedule_id: None,
            ..run("failed")
        }
    }

    #[test]
    fn each_notification_is_posted_to_the_desktop_once() {
        let (_dir, db, _) = notified(2);
        let desktop = FakeDesktop::default();
        assert!(deliver(&db, &desktop).unwrap().is_none());
        let post = (
            "runner: run (agent researcher): failed, 1 alert".to_string(),
            "denied: denied happened".to_string(),
        );
        assert_eq!(desktop.take(), [post.clone(), post]);
        deliver(&db, &desktop).unwrap();
        assert!(desktop.take().is_empty());

        // One already read isn't posted.
        let (_dir, db, stored_read) = notified(1);
        db.mark_read(Some(stored_read[0].id)).unwrap();
        deliver(&db, &desktop).unwrap();
        assert!(desktop.take().is_empty());
        assert!(db.undelivered().unwrap().is_empty());
    }

    #[test]
    fn many_waiting_notifications_are_posted_as_one() {
        let (_dir, db, _) = notified(MAX_POSTED + 1);
        let desktop = FakeDesktop::default();
        deliver(&db, &desktop).unwrap();
        assert_eq!(
            desktop.take(),
            [(
                format!("runner: {} new notifications", MAX_POSTED + 1),
                "Run `runner notifications` to see them.".to_string()
            )]
        );
    }

    #[test]
    fn a_failed_post_is_reported_and_not_retried() {
        let (_dir, db, _) = notified(1);
        let desktop = FakeDesktop {
            fail: true,
            ..Default::default()
        };
        let error = deliver(&db, &desktop).unwrap().unwrap();
        assert_eq!(error.to_string(), "no notification server");
        assert_eq!(desktop.take().len(), 1);
        assert!(deliver(&db, &desktop).unwrap().is_none());
        assert!(desktop.take().is_empty());
        // It's still there to read.
        assert_eq!(db.notifications(true, 10).unwrap().len(), 1);
    }

    #[test]
    fn a_body_lists_alerts_is_cut_short_and_escapes_markup() {
        let mut note = super::note(&run("failed"), None, &summary(&[]), None).unwrap();
        assert_eq!(body(&note), "took 1.200s, no alerts");
        note.alerts = vec![Alert {
            kind: "new_host".into(),
            message: "tool \"t\" reached \"<a href=x>evil</a>\"".into(),
        }];
        note.delegated_not_completed = vec![RunEnd {
            run_id: "c".into(),
            status: "failed".into(),
        }];
        assert_eq!(
            body(&note),
            "new_host: tool \"t\" reached \"<a href=x>evil</a>\"\ndelegated run c failed"
        );
        note.alerts[0].message = "x".repeat(400);
        let long = body(&note);
        assert_eq!(long.chars().count(), MAX_BODY + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn desktop_notifications_post_as_the_daemons_terminal() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        let ghostty = env(&[
            ("__CFBundleIdentifier", "com.mitchellh.ghostty"),
            ("TERM_PROGRAM", "ghostty"),
        ]);
        assert_eq!(desktop_app(None, ghostty), "com.mitchellh.ghostty");
        assert_eq!(
            desktop_app(Some("com.example.app"), ghostty),
            "com.example.app"
        );
        assert_eq!(
            desktop_app(None, env(&[("TERM_PROGRAM", "iTerm.app")])),
            "com.googlecode.iterm2"
        );
        assert_eq!(desktop_app(None, env(&[])), "com.apple.Terminal");
    }
}
