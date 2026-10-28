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
use chrono::{DateTime, LocalResult, NaiveDateTime, SubsecRound, TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use tokio_util::sync::CancellationToken;

use crate::db::Database;
use crate::service::Executor;

/// How often due schedules are looked for.
const TICK: Duration = Duration::from_secs(1);

/// How late a recurring occurrence may fire; one later than this, e.g.
/// because the daemon was stopped or the machine slept, is skipped as
/// missed. A cron schedule fires at most once a minute.
pub const LATE_LIMIT_MS: i64 = 60_000;

fn zone(tz: &str) -> Result<Tz> {
    tz.parse().map_err(|_| anyhow!("unknown time zone '{tz}'"))
}

/// A local time as schedules store it, e.g. `2026-10-29T07:00:00`.
fn local(at: &DateTime<Tz>) -> String {
    at.naive_local().format("%Y-%m-%dT%H:%M:%S%.f").to_string()
}

/// When local time `at` in time zone `tz` happens: `at` normalized, e.g.
/// `2026-10-29T07:00:00`, and Unix milliseconds. A time a clock change
/// skips or repeats is refused rather than guessed at.
pub fn fire_time(at: &str, tz: &str) -> Result<(String, i64)> {
    let zone = zone(tz)?;
    let civil = NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S%.f"))
        .with_context(|| format!("invalid time '{at}': use a local time like 2026-10-29T07:00"))?;
    let LocalResult::Single(zoned) = zone.from_local_datetime(&civil) else {
        bail!(
            "{} is skipped or repeated in {tz} by a clock change; pick another time",
            civil.format("%Y-%m-%dT%H:%M:%S%.f")
        );
    };
    Ok((local(&zoned), zoned.timestamp_millis()))
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
    let zone = zone(tz)?;
    let instant = Utc::now() + chrono::Duration::seconds(secs as i64);
    let at = instant.with_timezone(&zone).round_subsecs(0);
    Ok((local(&at), instant.timestamp_millis()))
}

/// A delay like `2s`, `90s`, `5m` or `1h30m`, in whole seconds: whole
/// numbers of days, hours, minutes and seconds, largest first.
pub fn parse_delay(text: &str) -> Result<u64> {
    let invalid = || anyhow!("invalid delay '{text}': use e.g. 2s, 5m or 1h30m");
    let mut rest = text;
    let mut units: &[(char, u64)] = &[('d', 86400), ('h', 3600), ('m', 60), ('s', 1)];
    let mut secs: u64 = 0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(invalid)?;
        let n: u64 = rest[..digits].parse().map_err(|_| invalid())?;
        let unit = rest[digits..].chars().next().ok_or_else(invalid)?;
        let at = units
            .iter()
            .position(|(u, _)| *u == unit)
            .ok_or_else(invalid)?;
        secs = n
            .checked_mul(units[at].1)
            .and_then(|s| s.checked_add(secs))
            .ok_or_else(invalid)?;
        units = &units[at + 1..];
        rest = &rest[digits + 1..];
    }
    if secs < 1 {
        bail!("invalid delay '{text}': use a whole number of seconds, at least 1");
    }
    Ok(secs)
}

/// A five-field cron expression, e.g. `0 7 * * MON-FRI`: minute, hour, day
/// of month, month and day of week, as Vixie cron reads them.
pub fn parse_cron(text: &str) -> Result<Cron> {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(text)
        .map_err(|e| anyhow!("invalid cron expression '{text}': {e}"))
}

/// When `cron` next fires in time zone `tz`, strictly after Unix
/// millisecond `after`: the local time and Unix milliseconds, or `None`
/// if it never fires again. Clock changes follow croner, i.e. Vixie cron:
/// a fixed-time job whose time a spring-forward gap skips runs right after
/// the gap, and once in a fall-back overlap; an interval job skips the gap
/// and runs in both passes of the overlap.
pub fn next_occurrence(cron: &str, tz: &str, after: i64) -> Result<Option<(String, i64)>> {
    let (cron, zone) = (parse_cron(cron)?, zone(tz)?);
    let after = zone
        .timestamp_millis_opt(after)
        .single()
        .context("time out of range")?;
    Ok(match cron.find_next_occurrence(&after, false) {
        Ok(next) => Some((local(&next), next.timestamp_millis())),
        Err(croner::errors::CronError::TimeSearchLimitExceeded) => None,
        Err(e) => bail!(
            "can't step '{cron}' in {tz}: {e}",
            cron = cron.pattern.as_str()
        ),
    })
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

    /// Unix milliseconds of a UTC time.
    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
            .timestamp_millis()
    }

    /// The next occurrences of `cron` in Warsaw after `after`, as local
    /// times and UTC hours and minutes.
    fn steps(cron: &str, mut after: i64, n: usize) -> Vec<(String, String)> {
        (0..n)
            .map(|_| {
                let (at, ms) = next_occurrence(cron, "Europe/Warsaw", after)
                    .unwrap()
                    .unwrap();
                after = ms;
                let utc = DateTime::from_timestamp_millis(ms).unwrap();
                (at, utc.format("%dT%H:%MZ").to_string())
            })
            .collect()
    }

    #[test]
    fn cron_schedules_step_in_their_zone() {
        // 06:00 in Warsaw (UTC+1) on Wednesday 2026-10-28.
        let wednesday = utc(2026, 10, 28, 5, 0);
        assert_eq!(
            steps("0 7 * * MON-FRI", wednesday, 3),
            [
                ("2026-10-28T07:00:00".into(), "28T06:00Z".into()),
                ("2026-10-29T07:00:00".into(), "29T06:00Z".into()),
                ("2026-10-30T07:00:00".into(), "30T06:00Z".into()),
            ]
        );
        // Strictly after: an occurrence at `after` itself is the last one.
        assert_eq!(
            steps("0 7 * * *", utc(2026, 10, 28, 6, 0), 1)[0].0,
            "2026-10-29T07:00:00"
        );
        assert!(next_occurrence("0 7 * * *", "Mars/Olympus", wednesday).is_err());
        // Five fields only: no seconds or years.
        for bad in ["0 0 7 * * *", "0 7 * * * 2027", "7am", "", "61 * * * *"] {
            assert!(parse_cron(bad).is_err(), "{bad}");
        }
        assert_eq!(
            next_occurrence("0 0 30 2 *", "UTC", wednesday).unwrap(),
            None
        );
    }

    #[test]
    fn cron_schedules_take_croners_clock_change_rules() {
        // Warsaw springs from 02:00 to 03:00 on 2026-03-29: a fixed-time
        // job runs right after the gap, an interval job skips it.
        let before_spring = utc(2026, 3, 28, 12, 0);
        assert_eq!(
            steps("30 2 * * *", before_spring, 2),
            [
                ("2026-03-29T03:00:00".into(), "29T01:00Z".into()),
                ("2026-03-30T02:30:00".into(), "30T00:30Z".into()),
            ]
        );
        assert_eq!(
            steps("*/30 * * * *", utc(2026, 3, 29, 0, 0), 3),
            [
                ("2026-03-29T01:30:00".into(), "29T00:30Z".into()),
                ("2026-03-29T03:00:00".into(), "29T01:00Z".into()),
                ("2026-03-29T03:30:00".into(), "29T01:30Z".into()),
            ]
        );
        // It falls back from 03:00 to 02:00 on 2026-10-25: a fixed-time job
        // runs once, an interval job in both passes.
        assert_eq!(
            steps("30 2 * * *", utc(2026, 10, 24, 12, 0), 2),
            [
                ("2026-10-25T02:30:00".into(), "25T00:30Z".into()),
                ("2026-10-26T02:30:00".into(), "26T01:30Z".into()),
            ]
        );
        assert_eq!(
            steps("*/30 2 * * *", utc(2026, 10, 24, 12, 0), 4),
            [
                ("2026-10-25T02:00:00".into(), "25T00:00Z".into()),
                ("2026-10-25T02:30:00".into(), "25T00:30Z".into()),
                ("2026-10-25T02:00:00".into(), "25T01:00Z".into()),
                ("2026-10-25T02:30:00".into(), "25T01:30Z".into()),
            ]
        );
    }

    #[test]
    fn delays_are_whole_seconds_from_now() {
        assert_eq!(parse_delay("2s").unwrap(), 2);
        assert_eq!(parse_delay("1m30s").unwrap(), 90);
        assert_eq!(parse_delay("1h").unwrap(), 3600);
        assert_eq!(parse_delay("1d2h").unwrap(), 93600);
        for bad in ["0s", "1.5s", "-2s", "soon", "", "5", "30s1m", "1m1m", "s"] {
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
