//! Schedules: tasks that run later without anyone watching, each pinned to
//! the agent versions it was approved for. A schedule whose agent, or an
//! agent it delegates to, has changed pauses until it's approved again, so
//! an edit made for chat can't quietly widen what an unattended run can do.
//!
//! The pin is checked when the daemon starts, when a schedule fires, and
//! again when its run starts, since a run queued before a restart starts
//! under the new config.
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio_util::sync::CancellationToken;

use crate::db::Database;
use crate::service::Executor;

/// How often due schedules are looked for.
const TICK: Duration = Duration::from_secs(1);

/// When local time `at` in time zone `tz` happens: `at` normalized, e.g.
/// `2026-10-29T07:00:00`, and Unix milliseconds. A time a clock change
/// skips or repeats is refused rather than guessed at.
pub fn fire_time(at: &str, tz: &str) -> Result<(String, i64)> {
    let zone = jiff::tz::TimeZone::get(tz).map_err(|_| anyhow!("unknown time zone '{tz}'"))?;
    let civil: jiff::civil::DateTime = at
        .parse()
        .with_context(|| format!("invalid time '{at}': use a local time like 2026-10-29T07:00"))?;
    let zoned = zone.to_ambiguous_zoned(civil).unambiguous().map_err(|_| {
        anyhow!("{civil} is skipped or repeated in {tz} by a clock change; pick another time")
    })?;
    Ok((civil.to_string(), zoned.timestamp().as_millisecond()))
}

/// The longest delay `fire_after` takes: a year.
pub const MAX_DELAY_SECS: u64 = 366 * 86400;

/// When `secs` seconds from now is: the local time in `tz`, to the second,
/// and Unix milliseconds. A delay always names one instant, so unlike
/// `fire_time` it's never ambiguous across a clock change.
pub fn fire_after(secs: u64, tz: &str) -> Result<(String, i64)> {
    if !(1..=MAX_DELAY_SECS).contains(&secs) {
        bail!("delay must be between 1 second and a year");
    }
    let zone = jiff::tz::TimeZone::get(tz).map_err(|_| anyhow!("unknown time zone '{tz}'"))?;
    let instant = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(secs as i64);
    let at = instant
        .to_zoned(zone)
        .datetime()
        .round(jiff::Unit::Second)
        .context("rounding a fire time")?;
    Ok((at.to_string(), instant.as_millisecond()))
}

/// A delay like `2s`, `90s`, `5m` or `1h30m`, in whole seconds.
pub fn parse_delay(text: &str) -> Result<u64> {
    let delay: jiff::SignedDuration = text
        .parse()
        .with_context(|| format!("invalid delay '{text}': use e.g. 2s, 5m or 1h30m"))?;
    if delay.subsec_nanos() != 0 || delay.as_secs() < 1 {
        bail!("invalid delay '{text}': use a whole number of seconds, at least 1");
    }
    Ok(delay.as_secs() as u64)
}

/// Pauses schedules whose agents changed while the daemon was stopped,
/// then enqueues schedules as they fall due, until `shutdown`. A one-off
/// that fell due while the daemon was stopped fires once when it starts.
pub async fn scheduler(db: Database, executor: Arc<dyn Executor>, shutdown: CancellationToken) {
    let pins = executor.clone();
    match db
        .blocking(move |db| db.pause_changed_schedules(&|agent| pins.pin(agent)))
        .await
    {
        Ok(paused) => {
            for schedule in paused {
                tracing::warn!(schedule = ?schedule.id, agents = ?schedule.paused_reason,
                    "schedule paused: its agent changed");
            }
        }
        Err(_) => tracing::error!("could not check schedules' agent versions"),
    }
    let mut tick = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tick.tick() => {
                let pins = executor.clone();
                match db.blocking(move |db| db.fire_due(crate::db::now(), &|agent| pins.pin(agent))).await {
                    Ok(runs) => {
                        for run in runs {
                            tracing::info!(schedule = ?run.schedule_id, run = ?run.id, "schedule fired");
                        }
                    }
                    // Transient (e.g. SQLITE_BUSY); due schedules stay due.
                    Err(_) => tracing::error!("could not fire due schedules"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fire_times_are_local_to_their_zone_and_never_guessed() {
        let (at, ms) = fire_time("2026-10-29T07:00", "Europe/Warsaw").unwrap();
        assert_eq!(at, "2026-10-29T07:00:00");
        // Warsaw is UTC+1 after the last Sunday of October.
        assert_eq!(ms, 1_793_253_600_000);
        // 02:30 doesn't happen on 2026-03-29 in Warsaw, and happens twice on
        // 2026-10-25.
        assert!(fire_time("2026-03-29T02:30", "Europe/Warsaw").is_err());
        assert!(fire_time("2026-10-25T02:30", "Europe/Warsaw").is_err());
        assert!(fire_time("2026-10-29T07:00", "Mars/Olympus").is_err());
        assert!(fire_time("tomorrow", "UTC").is_err());
    }

    #[test]
    fn delays_are_whole_seconds_from_now() {
        assert_eq!(parse_delay("2s").unwrap(), 2);
        assert_eq!(parse_delay("1m30s").unwrap(), 90);
        assert_eq!(parse_delay("1h").unwrap(), 3600);
        for bad in ["0s", "1.5s", "-2s", "soon", ""] {
            assert!(parse_delay(bad).is_err(), "{bad}");
        }
        let before = crate::db::now();
        let (at, ms) = fire_after(2, "UTC").unwrap();
        assert!((before + 2000..before + 3000).contains(&ms), "{ms}");
        assert!(!at.contains('.'), "{at}");
        assert!(fire_after(0, "UTC").is_err());
        assert!(fire_after(MAX_DELAY_SECS + 1, "UTC").is_err());
        assert!(fire_after(2, "Mars/Olympus").is_err());
    }
}
