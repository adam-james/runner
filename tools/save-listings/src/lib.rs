wit_bindgen::generate!({
    world: "tool-guest",
    path: "../../wit",
});

use std::collections::HashSet;
use std::path::Path;

use exports::local::agent::tool_api::{Guest, ToolDescription};

struct SaveListings;

/// The one file this tool writes, under its config's `dir`, mounted as `/`.
const FILE: &str = "/listings.md";
const HEADING: &str = "# Job listings";
/// Most listings in one call, and the longest a field or URL gets, so one
/// call can't grow the file without bound.
const MAX_LISTINGS: usize = 100;
const MAX_FIELD_CHARS: usize = 200;
const MAX_URL_CHARS: usize = 2000;

impl Guest for SaveListings {
    fn describe() -> ToolDescription {
        ToolDescription {
            name: "save_listings".to_string(),
            description: format!(
                "Save job listings to {FILE}, newest first, one line each. A listing whose \
                 URL is already in the file is skipped, so there's no need to read the file \
                 first. Existing entries are never changed. Says how many were added and \
                 which URLs were already there."
            ),
            input_schema_json: serde_json::json!({
                "type": "object",
                "properties": {
                    "listings": {
                        "type": "array",
                        "maxItems": MAX_LISTINGS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "url": {
                                    "type": "string",
                                    "description": "The listing's own http(s) link.",
                                },
                                "company": { "type": "string" },
                                "location": { "type": "string" },
                                "posted": {
                                    "type": "string",
                                    "description": "When it was posted, e.g. '2026-10-30'.",
                                },
                            },
                            "required": ["title", "url", "company"],
                        },
                    },
                },
                "required": ["listings"],
            })
            .to_string(),
        }
    }

    fn call(input_json: String) -> Result<String, String> {
        run(&input_json).map_err(|e| e.to_string())
    }
}

struct Listing {
    title: String,
    url: String,
    company: String,
    location: Option<String>,
    posted: Option<String>,
}

fn run(input_json: &str) -> anyhow::Result<String> {
    let input: serde_json::Value = serde_json::from_str(input_json)?;
    let items = input["listings"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing 'listings' array"))?;
    if items.len() > MAX_LISTINGS {
        anyhow::bail!(
            "at most {MAX_LISTINGS} listings per call; got {}",
            items.len()
        );
    }
    let listings = items
        .iter()
        .enumerate()
        .map(|(i, item)| listing(item).map_err(|e| anyhow::anyhow!("listing {i}: {e}")))
        .collect::<anyhow::Result<Vec<_>>>()?;
    save(Path::new(FILE), &listings, &today())
}

fn listing(item: &serde_json::Value) -> anyhow::Result<Listing> {
    let text = |name: &str| -> anyhow::Result<Option<String>> {
        match &item[name] {
            serde_json::Value::Null => Ok(None),
            value => {
                let s = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("'{name}' must be a string"))?;
                let s = one_line(s);
                Ok((!s.is_empty()).then(|| truncate(&s, MAX_FIELD_CHARS)))
            }
        }
    };
    let required = |name: &str| text(name)?.ok_or_else(|| anyhow::anyhow!("missing '{name}'"));
    let url = item["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'url'"))?
        .trim();
    Ok(Listing {
        title: required("title")?,
        url: check_url(url)?.to_string(),
        company: required("company")?,
        location: text("location")?,
        posted: text("posted")?,
    })
}

/// `url` if it's a plain http(s) link that can sit in a markdown link as is.
fn check_url(url: &str) -> anyhow::Result<&str> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        anyhow::bail!("'url' must start with https:// or http://");
    }
    if url.chars().count() > MAX_URL_CHARS {
        anyhow::bail!("'url' is longer than {MAX_URL_CHARS} characters");
    }
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '(' | ')' | '<' | '>'))
    {
        anyhow::bail!("'url' contains a space, control character or one of ( ) < >");
    }
    Ok(url)
}

/// `s` with each run of whitespace or control characters as one space, so a
/// field can't start a line of its own.
fn one_line(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Backslash-escapes what would end a link's text early, so the first `](`
/// on an entry's line is always where its URL starts.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '[' | ']' | '(' | ')') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn entry(listing: &Listing, found: &str) -> String {
    let mut line = format!(
        "- [{}]({}) at {}",
        escape(&listing.title),
        listing.url,
        escape(&listing.company)
    );
    for field in [
        &listing.location,
        &listing.posted.as_ref().map(|p| format!("posted {p}")),
    ]
    .into_iter()
    .flatten()
    {
        line.push_str(", ");
        line.push_str(&escape(field));
    }
    line.push_str(&format!(", found {found}"));
    line
}

/// The URL of an entry line this tool wrote, or `None` for any other line.
fn entry_url(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("- [")?;
    let start = rest.find("](")? + 2;
    let len = rest[start..].find(')')?;
    Some(&rest[start..start + len])
}

/// What counts as the same listing: the URL without a `#fragment` or a
/// trailing slash.
fn key(url: &str) -> &str {
    let url = url.split('#').next().unwrap_or(url);
    url.trim_end_matches('/')
}

fn save(path: &Path, listings: &[Listing], found: &str) -> anyhow::Result<String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => anyhow::bail!("{}: {e}", path.display()),
    };
    let mut seen: HashSet<&str> = existing.lines().filter_map(entry_url).map(key).collect();

    let mut added = Vec::new();
    let mut skipped = Vec::new();
    for listing in listings {
        if seen.insert(key(&listing.url)) {
            added.push(entry(listing, found));
        } else {
            skipped.push(listing.url.as_str());
        }
    }

    if !added.is_empty() {
        // New entries go just under the heading, or at the top if the file
        // doesn't start with one.
        let (head, body) = match existing.split_once('\n') {
            Some((first, rest)) if first.starts_with("# ") => (first, rest),
            None if existing.starts_with("# ") => (existing.as_str(), ""),
            _ if existing.is_empty() => (HEADING, ""),
            _ => ("", existing.as_str()),
        };
        let mut text = String::new();
        if !head.is_empty() {
            text.push_str(head);
            text.push_str("\n\n");
        }
        for line in &added {
            text.push_str(line);
            text.push('\n');
        }
        text.push_str(body.trim_start_matches('\n'));

        // Written beside it and renamed over it, so a trap or timeout mid-write
        // can't leave the file half written.
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, &text).map_err(|e| anyhow::anyhow!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    }

    let mut out = format!(
        "Added {} listing{} to {}",
        added.len(),
        if added.len() == 1 { "" } else { "s" },
        path.display()
    );
    if !skipped.is_empty() {
        out.push_str(&format!(
            "; {} already there: {}",
            skipped.len(),
            skipped.join(" ")
        ));
    }
    Ok(out)
}

/// Today's UTC date as YYYY-MM-DD.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_date((secs / 86_400) as i64)
}

/// The date `days` after 1970-01-01, by Howard Hinnant's `civil_from_days`.
fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

export!(SaveListings);

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "runner-save-listings-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("listings.md")
    }

    fn listings(json: &str) -> Vec<Listing> {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|item| listing(item).unwrap())
            .collect()
    }

    #[test]
    fn creates_the_file_then_prepends_only_new_urls() {
        let file = temp_file("prepend");
        let first = listings(
            r#"[{"title": "Backend Engineer", "url": "https://jobs.lever.co/acme/1",
                 "company": "Acme", "location": "Remote", "posted": "2026-10-30"}]"#,
        );
        let out = save(&file, &first, "2026-11-01").unwrap();
        assert!(out.starts_with("Added 1 listing to "), "{out}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# Job listings\n\n\
             - [Backend Engineer](https://jobs.lever.co/acme/1) at Acme, Remote, \
             posted 2026-10-30, found 2026-11-01\n"
        );

        let second = listings(
            r#"[{"title": "Platform Engineer", "url": "https://jobs.ashbyhq.com/b/2", "company": "B"},
                {"title": "Same, retitled", "url": "https://jobs.lever.co/acme/1/#apply", "company": "Acme"},
                {"title": "Twice in one call", "url": "https://jobs.ashbyhq.com/b/2", "company": "B"}]"#,
        );
        let out = save(&file, &second, "2026-11-02").unwrap();
        assert!(out.starts_with("Added 1 listing to "), "{out}");
        assert!(
            out.ends_with(
                "; 2 already there: https://jobs.lever.co/acme/1/#apply https://jobs.ashbyhq.com/b/2"
            ),
            "{out}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# Job listings\n\n\
             - [Platform Engineer](https://jobs.ashbyhq.com/b/2) at B, found 2026-11-02\n\
             - [Backend Engineer](https://jobs.lever.co/acme/1) at Acme, Remote, \
             posted 2026-10-30, found 2026-11-01\n"
        );
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn nothing_new_leaves_the_file_alone() {
        let file = temp_file("unchanged");
        std::fs::write(&file, "# Mine\n\nnotes\n- [A](https://a.example/1) at A\n").unwrap();
        let out = save(
            &file,
            &listings(r#"[{"title": "A", "url": "https://a.example/1", "company": "A"}]"#),
            "2026-11-01",
        )
        .unwrap();
        assert_eq!(
            out.split(';').next().unwrap(),
            format!("Added 0 listings to {}", file.display())
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# Mine\n\nnotes\n- [A](https://a.example/1) at A\n"
        );
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn fields_cant_break_the_line_or_the_link() {
        let [listing] = listings(
            r#"[{"title": "Evil](https://evil.example)\n- [Fake", "url": "https://a.example/1",
                 "company": "A\n\n# Heading"}]"#,
        )
        .try_into()
        .ok()
        .unwrap();
        let line = entry(&listing, "2026-11-01");
        assert_eq!(
            line,
            r"- [Evil\]\(https://evil.example\) - \[Fake](https://a.example/1) at A # Heading, found 2026-11-01"
        );
        assert_eq!(entry_url(&line), Some("https://a.example/1"));
    }

    #[test]
    fn checks_inputs() {
        assert!(run(r#"{}"#).is_err());
        let bad = [
            r#"{"url": "https://a.example", "company": "A"}"#,
            r#"{"title": "T", "url": "javascript:alert(1)", "company": "A"}"#,
            r#"{"title": "T", "url": "https://a.example/x)y", "company": "A"}"#,
            r#"{"title": "T", "url": "https://a.example/ x", "company": "A"}"#,
            r#"{"title": "T", "url": "https://a.example", "company": "  "}"#,
            r#"{"title": 3, "url": "https://a.example", "company": "A"}"#,
        ];
        for item in bad {
            let value: serde_json::Value = serde_json::from_str(item).unwrap();
            assert!(listing(&value).is_err(), "{item}");
        }
        let many = format!(
            r#"{{"listings": [{}]}}"#,
            vec![r#"{"title": "T", "url": "https://a.example", "company": "A"}"#; MAX_LISTINGS + 1]
                .join(",")
        );
        assert!(run(&many).unwrap_err().to_string().starts_with("at most"));
    }

    #[test]
    fn long_fields_are_cut() {
        let title = "x".repeat(MAX_FIELD_CHARS + 50);
        let item = serde_json::json!({"title": title, "url": "https://a.example", "company": "A"});
        let listing = listing(&item).unwrap();
        assert_eq!(listing.title.chars().count(), MAX_FIELD_CHARS + 1);
        assert!(listing.title.ends_with('…'));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(11_016), "2000-02-29");
        assert_eq!(civil_date(20_757), "2026-10-31");
    }
}
