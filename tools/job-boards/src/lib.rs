wit_bindgen::generate!({
    world: "tool-guest",
    path: "../../wit",
});

use exports::local::agent::tool_api::{Guest, ToolDescription};
use serde_json::Value;
use wstd::http::{Body, Client, Request};

struct JobBoards;

/// Boards one call may read.
const MAX_BOARDS: usize = 30;
/// Characters of listings handed back; the rest are counted, not shown.
const MAX_OUTPUT: usize = 30_000;
/// Characters of a Who's Hiring post's text, after its first line.
const MAX_POST_TEXT: usize = 1_000;

impl Guest for JobBoards {
    fn describe() -> ToolDescription {
        ToolDescription {
            name: "job_boards".to_string(),
            description: "List open jobs from company job boards on Greenhouse, Lever or Ashby, \
                          or from the latest Hacker News \"Who is hiring?\" thread. Each listing \
                          has a company, title, location, date posted and link, newest first."
                .to_string(),
            input_schema_json: serde_json::json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "enum": ["greenhouse", "lever", "ashby", "hn"],
                        "description": "Where to look. hn is the latest Hacker News \
                                        \"Ask HN: Who is hiring?\" thread.",
                    },
                    "boards": {
                        "type": "array",
                        "items": {"type": "string"},
                        "maxItems": MAX_BOARDS,
                        "description": "Company board names for greenhouse, lever or ashby, as \
                                        in boards.greenhouse.io/<name>, jobs.lever.co/<name> or \
                                        jobs.ashbyhq.com/<name>. Not used for hn.",
                    },
                    "query": {
                        "type": "string",
                        "description": "Words that must all appear, e.g. 'rust remote'. For a \
                                        board, matched against title, team and location; for \
                                        hn, a search of each post's text.",
                    },
                    "posted_within_days": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Only listings posted in the last this many days.",
                    },
                },
                "required": ["source"],
            })
            .to_string(),
        }
    }

    fn call(input_json: String) -> Result<String, String> {
        wstd::runtime::block_on(run(input_json)).map_err(|e| e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    Greenhouse,
    Lever,
    Ashby,
    Hn,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Greenhouse => "greenhouse",
            Source::Lever => "lever",
            Source::Ashby => "ashby",
            Source::Hn => "hn",
        }
    }

    fn url(self, board: &str) -> String {
        let board = urlencoding::encode(board);
        match self {
            Source::Greenhouse => {
                format!("https://boards-api.greenhouse.io/v1/boards/{board}/jobs")
            }
            Source::Lever => format!("https://api.lever.co/v0/postings/{board}?mode=json"),
            Source::Ashby => format!(
                "https://api.ashbyhq.com/posting-api/job-board/{board}?includeCompensation=true"
            ),
            Source::Hn => unreachable!("hn has no boards"),
        }
    }

    fn parse(self, board: &str, data: &Value) -> Vec<Listing> {
        match self {
            Source::Greenhouse => greenhouse(board, data),
            Source::Lever => lever(board, data),
            Source::Ashby => ashby(board, data),
            Source::Hn => unreachable!("hn has no boards"),
        }
    }
}

#[derive(Debug, Default, PartialEq)]
struct Listing {
    company: String,
    title: String,
    /// Location, workplace type and team, as the board gives them.
    details: Vec<String>,
    /// Unix seconds.
    posted: Option<i64>,
    salary: Option<String>,
    url: String,
    /// A Who's Hiring post's text after its first line.
    text: Option<String>,
}

async fn run(input_json: String) -> anyhow::Result<String> {
    let input: Value = serde_json::from_str(&input_json)?;
    let source = match input["source"].as_str() {
        Some("greenhouse") => Source::Greenhouse,
        Some("lever") => Source::Lever,
        Some("ashby") => Source::Ashby,
        Some("hn") => Source::Hn,
        _ => anyhow::bail!("'source' must be one of greenhouse, lever, ashby or hn"),
    };
    let query = match &input["query"] {
        Value::Null => String::new(),
        value => value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'query' must be a string"))?
            .trim()
            .to_string(),
    };
    let since = match &input["posted_within_days"] {
        Value::Null => None,
        value => match value.as_u64() {
            Some(days @ 1..) => Some(now() - days as i64 * 86_400),
            _ => anyhow::bail!("'posted_within_days' must be a whole number of at least 1"),
        },
    };

    if source == Source::Hn {
        return hn(&query, since).await;
    }
    let boards = boards(&input["boards"])?;
    let mut listings = Vec::new();
    let mut problems = Vec::new();
    for board in &boards {
        match get_json(&source.url(board)).await {
            Ok(data) => listings.extend(source.parse(board, &data)),
            Err(e) => problems.push(format!("{}/{board}: {e}", source.name())),
        }
    }
    let listings = filter(listings, &query, since);
    Ok(render(&listings, &problems))
}

fn boards(value: &Value) -> anyhow::Result<Vec<String>> {
    let boards = value
        .as_array()
        .filter(|boards| !boards.is_empty())
        .ok_or_else(|| anyhow::anyhow!("'boards' must list at least one board name"))?;
    if boards.len() > MAX_BOARDS {
        anyhow::bail!("'boards' lists more than {MAX_BOARDS} boards");
    }
    boards
        .iter()
        .map(|board| match board.as_str() {
            Some(name)
                if !name.is_empty()
                    && name.len() <= 100
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) =>
            {
                Ok(name.to_string())
            }
            _ => anyhow::bail!("board {board} isn't a board name like 'acme' or 'acme-inc'"),
        })
        .collect()
}

/// Greenhouse's `GET /v1/boards/{board}/jobs`.
fn greenhouse(board: &str, data: &Value) -> Vec<Listing> {
    jobs(&data["jobs"])
        .map(|job| Listing {
            company: text(&job["company_name"]).unwrap_or_else(|| board.to_string()),
            title: text(&job["title"]).unwrap_or_default(),
            details: [text(&job["location"]["name"])]
                .into_iter()
                .chain(
                    job["departments"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|d| text(&d["name"])),
                )
                .flatten()
                .collect(),
            posted: job["first_published"]
                .as_str()
                .or(job["updated_at"].as_str())
                .and_then(parse_time),
            salary: None,
            url: text(&job["absolute_url"]).unwrap_or_default(),
            text: None,
        })
        .collect()
}

/// Lever's `GET /v0/postings/{board}?mode=json`.
fn lever(board: &str, data: &Value) -> Vec<Listing> {
    jobs(data)
        .map(|job| {
            let categories = &job["categories"];
            let range = &job["salaryRange"];
            let salary = match (range["min"].as_f64(), range["max"].as_f64()) {
                (Some(min), Some(max)) => Some(format!(
                    "{min}-{max} {}{}",
                    text(&range["currency"]).unwrap_or_default(),
                    text(&range["interval"])
                        .map(|i| format!(" {i}"))
                        .unwrap_or_default()
                )),
                _ => None,
            };
            Listing {
                company: board.to_string(),
                title: text(&job["text"]).unwrap_or_default(),
                details: [
                    text(&categories["location"]),
                    text(&job["workplaceType"]),
                    text(&categories["commitment"]),
                    text(&categories["team"]),
                ]
                .into_iter()
                .flatten()
                .collect(),
                posted: job["createdAt"].as_i64().map(|ms| ms / 1000),
                salary,
                url: text(&job["hostedUrl"]).unwrap_or_default(),
                text: None,
            }
        })
        .collect()
}

/// Ashby's `GET /posting-api/job-board/{board}`.
fn ashby(board: &str, data: &Value) -> Vec<Listing> {
    jobs(&data["jobs"])
        .filter(|job| job["isListed"].as_bool() != Some(false))
        .map(|job| Listing {
            company: board.to_string(),
            title: text(&job["title"]).unwrap_or_default(),
            details: [
                text(&job["location"]),
                text(&job["workplaceType"])
                    .or_else(|| (job["isRemote"] == true).then(|| "Remote".to_string())),
                text(&job["employmentType"]),
                text(&job["department"]),
            ]
            .into_iter()
            .flatten()
            .collect(),
            posted: job["publishedAt"].as_str().and_then(parse_time),
            salary: text(&job["compensation"]["compensationTierSummary"]),
            url: text(&job["jobUrl"]).unwrap_or_default(),
            text: None,
        })
        .collect()
}

fn jobs(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

/// The latest "Ask HN: Who is hiring?" thread's top-level posts.
async fn hn(query: &str, since: Option<i64>) -> anyhow::Result<String> {
    let stories = get_json(
        "https://hn.algolia.com/api/v1/search_by_date?tags=story,author_whoishiring&hitsPerPage=10",
    )
    .await?;
    let (id, title) = hiring_thread(&stories)
        .ok_or_else(|| anyhow::anyhow!("no recent \"Who is hiring?\" thread found"))?;
    let mut url = format!(
        "https://hn.algolia.com/api/v1/search?tags=comment,story_{id}&hitsPerPage=1000&query={}",
        urlencoding::encode(query)
    );
    if let Some(since) = since {
        url.push_str(&format!("&numericFilters=created_at_i>{since}"));
    }
    let comments = get_json(&url).await?;
    let mut listings = hn_posts(id, &comments);
    listings.sort_by_key(|l| std::cmp::Reverse(l.posted));
    Ok(format!(
        "From {}:\n{}",
        json_quoted(&title),
        render(&listings, &[])
    ))
}

fn hiring_thread(stories: &Value) -> Option<(u64, String)> {
    jobs(&stories["hits"]).find_map(|hit| {
        let title = text(&hit["title"])?;
        let id = hit["objectID"].as_str()?.parse().ok()?;
        title
            .to_lowercase()
            .contains("who is hiring?")
            .then_some((id, title))
    })
}

/// Top-level comments of story `id`: each post's first line, usually
/// "Company | Role | Location | ...", is its title.
fn hn_posts(id: u64, comments: &Value) -> Vec<Listing> {
    jobs(&comments["hits"])
        .filter(|hit| hit["parent_id"].as_u64() == Some(id))
        .filter_map(|hit| {
            let html = hit["comment_text"].as_str()?;
            let body = html2text::config::plain()
                .string_from_read(html.as_bytes(), 100)
                .ok()?;
            let body = clean(&body);
            let mut lines = body.trim().splitn(2, '\n');
            let title = lines.next()?.trim().to_string();
            let rest = lines.next().map(str::trim).unwrap_or_default();
            Some(Listing {
                company: title
                    .split('|')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                title,
                details: Vec::new(),
                posted: hit["created_at_i"].as_i64(),
                salary: None,
                url: format!(
                    "https://news.ycombinator.com/item?id={}",
                    hit["objectID"].as_str()?
                ),
                text: (!rest.is_empty()).then(|| truncate(rest, MAX_POST_TEXT)),
            })
        })
        .collect()
}

/// Listings with every word of `query` in their title or details, posted
/// at or after `since`, newest first.
fn filter(listings: Vec<Listing>, query: &str, since: Option<i64>) -> Vec<Listing> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut listings: Vec<_> = listings
        .into_iter()
        .filter(|l| {
            let haystack =
                format!("{} {} {}", l.company, l.title, l.details.join(" ")).to_lowercase();
            words.iter().all(|w| haystack.contains(w.as_str()))
        })
        .filter(|l| since.is_none_or(|since| l.posted.is_some_and(|p| p >= since)))
        .collect();
    listings.sort_by_key(|l| std::cmp::Reverse(l.posted));
    listings
}

fn render(listings: &[Listing], problems: &[String]) -> String {
    let mut out = String::new();
    for problem in problems {
        out.push_str(&format!("couldn't read {problem}\n"));
    }
    if listings.is_empty() {
        out.push_str("No listings found.");
        return out;
    }
    out.push_str(&format!(
        "{} listing{}, newest first:\n",
        listings.len(),
        if listings.len() == 1 { "" } else { "s" }
    ));
    for (shown, listing) in listings.iter().enumerate() {
        let entry = entry(listing);
        if out.len() + entry.len() > MAX_OUTPUT {
            out.push_str(&format!(
                "\n[{} more not shown; narrow the query or posted_within_days]",
                listings.len() - shown
            ));
            break;
        }
        out.push_str(&entry);
    }
    out
}

fn entry(listing: &Listing) -> String {
    let mut line = listing.details.clone();
    if let Some(posted) = listing.posted {
        line.push(format!("posted {}", date(posted)));
    }
    if let Some(salary) = &listing.salary {
        line.push(salary.clone());
    }
    let mut out = format!("\n- {}: {}\n", listing.company, listing.title);
    if !line.is_empty() {
        out.push_str(&format!("  {}\n", line.join(" · ")));
    }
    out.push_str(&format!("  {}\n", listing.url));
    if let Some(text) = &listing.text {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            out.push_str(&format!("  > {}\n", line.trim()));
        }
    }
    out
}

/// A string field with control characters dropped and whitespace collapsed,
/// or `None` if it's missing or blank.
fn text(value: &Value) -> Option<String> {
    let text = value
        .as_str()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    (!text.is_empty()).then_some(text)
}

/// `text` with control characters other than newlines dropped.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || !c.is_control())
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

fn json_quoted(text: &str) -> String {
    serde_json::json!(text).to_string()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Unix seconds from an RFC 3339 time such as `2026-10-20T12:34:56-04:00`
/// or `2026-10-20T12:34:56.789Z`.
fn parse_time(s: &str) -> Option<i64> {
    let num = |range: std::ops::Range<usize>| s.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let rest = s.get(19..)?;
    let zone = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match zone {
        "Z" | "" => 0,
        _ => {
            let sign = match zone.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hours: i64 = zone.get(1..3)?.parse().ok()?;
            let minutes: i64 = zone.get(4..6)?.parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `YYYY-MM-DD` of unix seconds, in UTC.
fn date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    // Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

async fn get_json(url: &str) -> anyhow::Result<Value> {
    let request = Request::get(url).body(Body::empty())?;
    let response = Client::new().send(request).await?;
    let status = response.status();
    let mut body = response.into_body();
    let bytes = body.contents().await?;
    match status.as_u16() {
        200..=299 => Ok(serde_json::from_slice(bytes)?),
        404 => anyhow::bail!("not found (404); check the board name"),
        code => anyhow::bail!("returned {code}"),
    }
}

export!(JobBoards);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn times_parse_with_offsets_and_fractions() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-10-20T12:00:00Z"), Some(1_792_497_600));
        assert_eq!(
            parse_time("2026-10-20T08:00:00-04:00"),
            parse_time("2026-10-20T12:00:00Z")
        );
        assert_eq!(
            parse_time("2026-10-20T12:00:00.123+00:00"),
            parse_time("2026-10-20T12:00:00Z")
        );
        assert_eq!(parse_time("yesterday"), None);
        assert_eq!(date(1_792_497_600), "2026-10-20");
        assert_eq!(
            date(parse_time("2024-02-29T23:59:59Z").unwrap()),
            "2024-02-29"
        );
    }

    #[test]
    fn board_names_are_checked() {
        assert_eq!(
            boards(&json!(["anthropic", "acme-inc"])).unwrap(),
            ["anthropic", "acme-inc"]
        );
        assert!(boards(&json!([])).is_err());
        assert!(boards(&json!(["../admin"])).is_err());
        assert!(boards(&json!(["a?b=c"])).is_err());
        assert!(boards(&Value::Null).is_err());
    }

    #[test]
    fn greenhouse_jobs_become_listings() {
        let data = json!({"jobs": [{
            "title": "Software Engineer,\n Infrastructure",
            "company_name": "Acme",
            "absolute_url": "https://boards.greenhouse.io/acme/jobs/1",
            "location": {"name": "Remote, US"},
            "departments": [{"name": "Engineering"}],
            "first_published": "2026-10-20T08:00:00-04:00",
            "updated_at": "2026-10-25T08:00:00-04:00",
        }]});
        assert_eq!(
            greenhouse("acme", &data),
            [Listing {
                company: "Acme".into(),
                title: "Software Engineer, Infrastructure".into(),
                details: vec!["Remote, US".into(), "Engineering".into()],
                posted: parse_time("2026-10-20T12:00:00Z"),
                url: "https://boards.greenhouse.io/acme/jobs/1".into(),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn lever_postings_become_listings() {
        let data = json!([{
            "text": "Backend Engineer",
            "hostedUrl": "https://jobs.lever.co/acme/abc",
            "createdAt": 1_792_497_600_000_i64,
            "workplaceType": "remote",
            "categories": {"location": "New York", "team": "Platform", "commitment": "Full-time"},
            "salaryRange": {"min": 150000, "max": 200000, "currency": "USD", "interval": "per-year-salary"},
        }]);
        let listings = lever("acme", &data);
        assert_eq!(listings[0].title, "Backend Engineer");
        assert_eq!(
            listings[0].details,
            ["New York", "remote", "Full-time", "Platform"]
        );
        assert_eq!(listings[0].posted, Some(1_792_497_600));
        assert_eq!(
            listings[0].salary.as_deref(),
            Some("150000-200000 USD per-year-salary")
        );
    }

    #[test]
    fn ashby_skips_unlisted_jobs() {
        let data = json!({"jobs": [
            {"title": "ML Engineer", "location": "London", "isRemote": true,
             "department": "Research", "publishedAt": "2026-10-20T12:00:00.000+00:00",
             "jobUrl": "https://jobs.ashbyhq.com/acme/1", "isListed": true,
             "compensation": {"compensationTierSummary": "£100K – £140K"}},
            {"title": "Secret", "isListed": false, "jobUrl": "https://jobs.ashbyhq.com/acme/2"},
        ]});
        let listings = ashby("acme", &data);
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].details, ["London", "Remote", "Research"]);
        assert_eq!(listings[0].salary.as_deref(), Some("£100K – £140K"));
    }

    #[test]
    fn hn_finds_the_hiring_thread_and_its_top_level_posts() {
        let stories = json!({"hits": [
            {"title": "Ask HN: Who wants to be hired? (October 2026)", "objectID": "1"},
            {"title": "Ask HN: Who is hiring? (October 2026)", "objectID": "2"},
        ]});
        assert_eq!(
            hiring_thread(&stories),
            Some((2, "Ask HN: Who is hiring? (October 2026)".into()))
        );
        let comments = json!({"hits": [
            {"objectID": "10", "parent_id": 2, "created_at_i": 1_792_497_600,
             "comment_text": "Acme | Rust Engineer | Remote (US)<p>We build things.\u{7}<p>Email jobs@acme.example"},
            {"objectID": "11", "parent_id": 10, "created_at_i": 1_792_497_700,
             "comment_text": "Is this still open?"},
        ]});
        let posts = hn_posts(2, &comments);
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].company, "Acme");
        assert_eq!(posts[0].title, "Acme | Rust Engineer | Remote (US)");
        assert_eq!(posts[0].url, "https://news.ycombinator.com/item?id=10");
        let text = posts[0].text.as_deref().unwrap();
        assert!(text.contains("We build things."), "{text}");
        assert!(!text.contains('\u{7}'));
    }

    #[test]
    fn filtering_matches_every_word_and_sorts_newest_first() {
        let listing = |title: &str, posted| Listing {
            company: "Acme".into(),
            title: title.into(),
            details: vec!["Remote".into()],
            posted,
            ..Default::default()
        };
        let listings = vec![
            listing("Rust Engineer", Some(100)),
            listing("Go Engineer", Some(300)),
            listing("Senior Rust Engineer", Some(200)),
            listing("Rust Engineer, undated", None),
        ];
        let titles = |l: Vec<Listing>| l.into_iter().map(|l| l.title).collect::<Vec<_>>();
        assert_eq!(
            titles(filter(listings, "rust REMOTE", Some(150))),
            ["Senior Rust Engineer"]
        );
    }

    #[test]
    fn rendering_reports_problems_and_stops_at_the_limit() {
        let many: Vec<_> = (0..1000)
            .map(|i| Listing {
                company: "Acme".into(),
                title: format!("Engineer {i}"),
                url: format!("https://example.com/{i}"),
                posted: Some(1_792_497_600),
                ..Default::default()
            })
            .collect();
        let out = render(
            &many,
            &["lever/nope: not found (404); check the board name".into()],
        );
        assert!(out.starts_with("couldn't read lever/nope: not found (404)"));
        assert!(
            out.contains(
                "1000 listings, newest first:\n\n- Acme: Engineer 0\n  posted 2026-10-20\n"
            )
        );
        assert!(out.len() <= MAX_OUTPUT + 100);
        assert!(out.ends_with("more not shown; narrow the query or posted_within_days]"));
        assert_eq!(render(&[], &[]), "No listings found.");
    }
}
