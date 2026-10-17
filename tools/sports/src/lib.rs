wit_bindgen::generate!({
    world: "tool-guest",
    path: "../../wit",
});

use exports::local::agent::tool_api::{Guest, ToolDescription};
use wstd::http::{Body, Client, Request};

struct Sports;

/// League name the model passes in -> ESPN's `{sport}/{league}` path segment.
const LEAGUES: &[(&str, &str)] = &[
    ("nfl", "football/nfl"),
    ("college-football", "football/college-football"),
    ("nba", "basketball/nba"),
    ("wnba", "basketball/wnba"),
    ("mens-college-basketball", "basketball/mens-college-basketball"),
    ("mlb", "baseball/mlb"),
    ("nhl", "hockey/nhl"),
    ("mls", "soccer/usa.1"),
    ("epl", "soccer/eng.1"),
    ("laliga", "soccer/esp.1"),
    ("bundesliga", "soccer/ger.1"),
    ("serie-a", "soccer/ita.1"),
    ("champions-league", "soccer/uefa.champions"),
];

impl Guest for Sports {
    fn describe() -> ToolDescription {
        let leagues: Vec<&str> = LEAGUES.iter().map(|(name, _)| *name).collect();
        ToolDescription {
            name: "get_scores".to_string(),
            description: "Get scores and schedule for a sports league on a given day: \
                          final scores, live games in progress, and upcoming games. \
                          The result starts with today's date, so relative questions \
                          like 'last night' or 'this weekend' can be answered."
                .to_string(),
            input_schema_json: serde_json::json!({
                "type": "object",
                "properties": {
                    "league": {
                        "type": "string",
                        "enum": leagues,
                        "description": "Which league to look up",
                    },
                    "date": {
                        "type": "string",
                        "description": "Specific day to look up as YYYY-MM-DD. Takes precedence over days_from_today.",
                    },
                    "days_from_today": {
                        "type": "integer",
                        "description": "Day to look up relative to today (US Eastern time): 0 = today, \
                                        -1 = yesterday, 1 = tomorrow. Defaults to 0.",
                    },
                    "team": {
                        "type": "string",
                        "description": "Only return games involving this team, e.g. 'Seahawks' or 'SEA'",
                    },
                },
                "required": ["league"],
            })
            .to_string(),
        }
    }

    fn call(input_json: String) -> Result<String, String> {
        wstd::runtime::block_on(run(input_json)).map_err(|e| e.to_string())
    }
}

async fn run(input_json: String) -> anyhow::Result<String> {
    let input: serde_json::Value = serde_json::from_str(&input_json)?;
    let league = input["league"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'league' field"))?;
    let path = LEAGUES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(league))
        .map(|(_, path)| *path)
        .ok_or_else(|| anyhow::anyhow!("unsupported league '{league}'"))?;

    let today = today_eastern()?;
    let day = match input["date"].as_str() {
        Some(date) => parse_date(date)?,
        None => today + input["days_from_today"].as_i64().unwrap_or(0),
    };

    let (y, m, d) = civil_from_days(day);
    let url = format!(
        "https://site.api.espn.com/apis/site/v2/sports/{path}/scoreboard?dates={y:04}{m:02}{d:02}"
    );
    let header = format!(
        "Today is {}. {} games on {}:",
        format_day(today),
        league.to_uppercase(),
        format_day(day)
    );
    let team = input["team"].as_str().map(str::to_lowercase);

    let scoreboard = get_json(&url).await?;
    let events = scoreboard["events"].as_array().cloned().unwrap_or_default();

    let lines: Vec<String> = events
        .iter()
        .filter(|event| {
            team.as_deref()
                .is_none_or(|t| event_mentions_team(event, t))
        })
        .map(format_event)
        .collect();

    if lines.is_empty() {
        let who = input["team"]
            .as_str()
            .map(|t| format!(" involving {t}"))
            .unwrap_or_default();
        return Ok(format!("{header}\nNo games found{who}."));
    }
    Ok(format!("{header}\n{}", lines.join("\n")))
}

// Dates below are plain day counts since 1970-01-01, which makes "N days
// from today" simple integer arithmetic.

/// Today's date in US Eastern time, which is what ESPN keys its scoreboard
/// days on -- a game at 7pm Pacific is still "today", even though it's
/// already tomorrow in UTC. Reads the host's wall clock via `wasi:clocks`.
fn today_eastern() -> anyhow::Result<i64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let offset_hours = if in_us_dst(now) { -4 } else { -5 };
    Ok((now + offset_hours * 3600).div_euclid(86_400))
}

/// US daylight saving time: from 2am local on the second Sunday of March
/// to 2am local on the first Sunday of November.
fn in_us_dst(unix_secs: i64) -> bool {
    let (year, _, _) = civil_from_days(unix_secs.div_euclid(86_400));
    let nth_sunday = |month: u32, n: i64| {
        let first = days_from_civil(year, month, 1);
        first + (7 - weekday(first)) % 7 + 7 * (n - 1)
    };
    // 2am EST = 07:00 UTC; 2am EDT = 06:00 UTC.
    let start = nth_sunday(3, 2) * 86_400 + 7 * 3600;
    let end = nth_sunday(11, 1) * 86_400 + 6 * 3600;
    (start..end).contains(&unix_secs)
}

fn parse_date(date: &str) -> anyhow::Result<i64> {
    let invalid = || anyhow::anyhow!("invalid date '{date}', expected YYYY-MM-DD");
    let mut parts = date.trim().splitn(3, '-').map(str::parse::<u32>);
    let (Some(Ok(y)), Some(Ok(m)), Some(Ok(d))) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid());
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(invalid());
    }
    Ok(days_from_civil(y as i64, m, d))
}

/// e.g. "Saturday 2026-10-17".
fn format_day(day: i64) -> String {
    const NAMES: [&str; 7] = [
        "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
    ];
    let (y, m, d) = civil_from_days(day);
    format!("{} {y:04}-{m:02}-{d:02}", NAMES[weekday(day) as usize])
}

/// 0 = Sunday. 1970-01-01 was a Thursday.
fn weekday(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

// Proleptic Gregorian <-> day-count conversions, from Howard Hinnant's
// "chrono-Compatible Low-Level Date Algorithms".

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        let day = days_from_civil(2026, 10, 17);
        assert_eq!(civil_from_days(day), (2026, 10, 17));
        assert_eq!(format_day(day), "Saturday 2026-10-17");
        assert_eq!(civil_from_days(days_from_civil(2024, 3, 1) - 1), (2024, 2, 29));
    }

    #[test]
    fn dst_boundaries() {
        // 2026: DST runs from Mar 8 07:00 UTC to Nov 1 06:00 UTC.
        let at = |y, m, d, h: i64| days_from_civil(y, m, d) * 86_400 + h * 3600;
        assert!(!in_us_dst(at(2026, 3, 8, 6)));
        assert!(in_us_dst(at(2026, 3, 8, 7)));
        assert!(in_us_dst(at(2026, 11, 1, 5)));
        assert!(!in_us_dst(at(2026, 11, 1, 6)));
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_date("2026-10-17").unwrap(), days_from_civil(2026, 10, 17));
        assert!(parse_date("20261017").is_err());
        assert!(parse_date("2026-13-01").is_err());
    }
}

fn competitors(event: &serde_json::Value) -> Vec<serde_json::Value> {
    event["competitions"][0]["competitors"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn event_mentions_team(event: &serde_json::Value, team: &str) -> bool {
    competitors(event).iter().any(|c| {
        ["displayName", "shortDisplayName", "abbreviation", "location", "name"]
            .iter()
            .filter_map(|field| c["team"][*field].as_str())
            .any(|value| value.to_lowercase().contains(team))
    })
}

/// One line per game, e.g. "Seattle Seahawks 24 @ San Francisco 49ers 17 (Final)".
fn format_event(event: &serde_json::Value) -> String {
    let status = &event["status"]["type"];
    let detail = status["detail"]
        .as_str()
        .or_else(|| status["description"].as_str())
        .unwrap_or("");
    // Scores are meaningless before kickoff (ESPN reports "0"), so drop them.
    let show_score = status["state"] != "pre";

    let side = |home_away: &str| {
        let Some(c) = competitors(event)
            .into_iter()
            .find(|c| c["homeAway"] == home_away)
        else {
            return "?".to_string();
        };
        let name = c["team"]["displayName"].as_str().unwrap_or("?");
        match c["score"].as_str() {
            Some(score) if show_score => format!("{name} {score}"),
            _ => name.to_string(),
        }
    };

    format!("{} @ {} ({detail})", side("away"), side("home"))
}

async fn get_json(url: &str) -> anyhow::Result<serde_json::Value> {
    let request = Request::get(url).body(Body::empty())?;
    let response = Client::new().send(request).await?;
    let mut body = response.into_body();
    let bytes = body.contents().await?;
    Ok(serde_json::from_slice(bytes)?)
}

export!(Sports);
