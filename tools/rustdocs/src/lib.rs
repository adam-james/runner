wit_bindgen::generate!({
    world: "tool-guest",
    path: "../../wit",
});

use exports::local::agent::tool_api::{Guest, ToolDescription};
use html5ever::{QualName, local_name, ns};
use scraper::node::{Node, Text};
use scraper::{ElementRef, Html, Selector};
use wstd::http::{Body, Client, Request};

struct RustDocs;

/// Lines returned when the model doesn't ask for a number, and the most it
/// can ask for. Long pages are read a page at a time with `offset`.
const DEFAULT_LIMIT: u64 = 400;
const MAX_LIMIT: u64 = 2000;
/// Width prose is wrapped to.
const WIDTH: usize = 100;
/// Most redirects followed for one request, e.g. `latest` to a version.
const MAX_REDIRECTS: usize = 5;
/// Most near-misses suggested when a path isn't found.
const MAX_SUGGESTIONS: usize = 15;

/// Crates documented on doc.rust-lang.org rather than docs.rs.
const STD_CRATES: [&str; 5] = ["std", "core", "alloc", "proc_macro", "test"];

/// Prefixes rustdoc gives the ids of an item's members, e.g. `method.lock`.
const MEMBER_ID_PREFIXES: [&str; 6] = [
    "method",
    "tymethod",
    "variant",
    "structfield",
    "associatedtype",
    "associatedconstant",
];

/// Parts of a page that are page furniture, or too long to be worth their
/// space: blanket impls (`From<T> for T` and the like) are on every type.
const REMOVED: &str = "button, rustdoc-toolbar, script, a.src, a.anchor, a.doc-anchor, \
                       summary.hideme, #blanket-implementations, #blanket-implementations-list, \
                       #trait-implementations-list .impl-items, \
                       #synthetic-implementations-list .impl-items, \
                       #implementors-list .impl-items, #synthetic-implementors-list .impl-items";

impl Guest for RustDocs {
    fn describe() -> ToolDescription {
        ToolDescription {
            name: "rust_docs".to_string(),
            description: "Read the documentation of a Rust crate, or of one item in it, \
                          from docs.rs (or doc.rust-lang.org for std, core and alloc). \
                          Returns the item's signature, its docs, and, for a type or trait, \
                          its methods and implementations; for a crate or module, what's \
                          in it. If the path isn't found, suggests close matches. Long pages \
                          come back a page at a time: use `offset` to read on."
                .to_string(),
            input_schema_json: serde_json::json!({
                "type": "object",
                "properties": {
                    "crate": {
                        "type": "string",
                        "description": "Crate name, e.g. 'tokio', 'serde_json' or 'std'",
                    },
                    "path": {
                        "type": "string",
                        "description": "Item within the crate, e.g. 'sync::Mutex', a \
                                        method like 'sync::Mutex::lock', or a module like \
                                        'sync'. Leave out for the crate's front page.",
                    },
                    "version": {
                        "type": "string",
                        "description": "Crate version, e.g. '1.40.0' or '1'; for std, a \
                                        Rust version, 'beta' or 'nightly'. Defaults to the \
                                        latest release.",
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Line to start reading from. Defaults to 1.",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": format!("Most lines to return. Defaults to {DEFAULT_LIMIT}."),
                    },
                },
                "required": ["crate"],
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
    let krate = input["crate"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing 'crate' field"))?;
    let version = input["version"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let path = input["path"]
        .as_str()
        .map(|p| item_path(krate, p))
        .unwrap_or_default();
    let offset = whole_number(&input["offset"], "offset", 1, u64::MAX)?.unwrap_or(1);
    let limit = whole_number(&input["limit"], "limit", 1, MAX_LIMIT)?.unwrap_or(DEFAULT_LIMIT);
    for part in [Some(krate), version].into_iter().flatten() {
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.+".contains(c))
        {
            anyhow::bail!("'{part}' isn't a valid crate name or version");
        }
    }

    let (root_url, root_page) = crate_root(krate, version).await?;
    let (url, text) = if path.is_empty() {
        (root_url, render(&root_page, None)?)
    } else {
        let (_, index) = get(&format!("{root_url}all.html")).await?;
        let items = all_items(&index);
        match locate(&path, &items)? {
            Location::Page(href) => {
                let url = format!("{root_url}{href}");
                let (url, page) = get(&url).await?;
                let text = render(&page, None)?;
                (url, text)
            }
            Location::Member(href, member) => {
                let (url, page) = get(&format!("{root_url}{href}")).await?;
                let text = render(&page, Some(member))?;
                (format!("{url}#{member}"), text)
            }
        }
    };
    Ok(format!("{url}\n\n{}", paged(&text, offset, limit)))
}

/// `path` relative to the crate: with any `crate::`-style prefix dropped,
/// so `tokio::sync::Mutex` and `sync::Mutex` are the same item.
fn item_path(krate: &str, path: &str) -> String {
    let path = path.trim().trim_matches(':');
    let lib = krate.replace('-', "_");
    for prefix in [lib.as_str(), "crate"] {
        if path == prefix {
            return String::new();
        }
        if let Some(rest) = path.strip_prefix(prefix).and_then(|p| p.strip_prefix("::")) {
            return rest.to_string();
        }
    }
    path.to_string()
}

/// The URL of the crate's front page, ending in `/` so pages' relative links
/// resolve against it, and the page itself. docs.rs redirects
/// `/tokio/latest/` to `/tokio/latest/tokio/`, which also sorts out crates
/// whose library is named differently, e.g. rust-crypto's `crypto`.
async fn crate_root(krate: &str, version: Option<&str>) -> anyhow::Result<(String, String)> {
    let url = if STD_CRATES.contains(&krate) {
        match version {
            None => format!("https://doc.rust-lang.org/{krate}/"),
            Some(version) => format!("https://doc.rust-lang.org/{version}/{krate}/"),
        }
    } else {
        format!("https://docs.rs/{krate}/{}/", version.unwrap_or("latest"))
    };
    let (url, page) = get(&url).await.map_err(|e| match version {
        None => anyhow::anyhow!("no docs for crate '{krate}': {e}"),
        Some(version) => anyhow::anyhow!("no docs for crate '{krate}' version '{version}': {e}"),
    })?;
    let root = match url.rfind('/') {
        Some(slash) => url[..=slash].to_string(),
        None => url,
    };
    Ok((root, page))
}

/// Every item listed on a crate's `all.html`, as (path, link) pairs such as
/// ("sync::Mutex", "sync/struct.Mutex.html"). Modules aren't listed.
fn all_items(index: &str) -> Vec<(String, String)> {
    let html = Html::parse_document(index);
    let links = Selector::parse("ul.all-items a").unwrap();
    html.select(&links)
        .filter_map(|a| {
            let href = a.value().attr("href")?;
            Some((a.text().collect::<String>(), href.to_string()))
        })
        .collect()
}

#[derive(Debug, PartialEq)]
enum Location<'a> {
    /// A page of its own, by its link relative to the crate's root.
    Page(String),
    /// A method, field, variant etc., on its parent's page.
    Member(String, &'a str),
}

fn locate<'a>(path: &'a str, items: &[(String, String)]) -> anyhow::Result<Location<'a>> {
    if let Some((_, href)) = items.iter().find(|(p, _)| p == path) {
        return Ok(Location::Page(href.clone()));
    }
    let module = format!("{path}::");
    if items.iter().any(|(p, _)| p.starts_with(&module)) {
        return Ok(Location::Page(format!(
            "{}/index.html",
            path.replace("::", "/")
        )));
    }
    if let Some((parent, member)) = path.rsplit_once("::")
        && let Some((_, href)) = items.iter().find(|(p, _)| p == parent)
    {
        return Ok(Location::Member(href.clone(), member));
    }
    Err(not_found(path, items))
}

/// A "not found" error listing items whose name is close to the last part
/// of `path`, which is usually what's been misremembered or misplaced.
fn not_found(path: &str, items: &[(String, String)]) -> anyhow::Error {
    let name = path.rsplit("::").next().unwrap_or(path).to_lowercase();
    let mut close: Vec<&str> = items
        .iter()
        .map(|(p, _)| p.as_str())
        .filter(|p| {
            let last = p.rsplit("::").next().unwrap_or(p).to_lowercase();
            last.contains(&name)
        })
        .collect();
    // A name can be listed more than once, e.g. trait `Hash` and derive
    // macro `Hash`.
    close.sort();
    close.dedup();
    // Exact names first, then the shortest, which are usually the most
    // central.
    close.sort_by_key(|p| (!p.to_lowercase().ends_with(&name), p.len()));
    close.truncate(MAX_SUGGESTIONS);
    if close.is_empty() {
        anyhow::anyhow!("'{path}' not found")
    } else {
        anyhow::anyhow!("'{path}' not found. Close matches:\n{}", close.join("\n"))
    }
}

/// A docs page as plain text: the whole item, or with `member`, just that
/// method, field or variant.
fn render(page: &str, member: Option<&str>) -> anyhow::Result<String> {
    let mut html = Html::parse_document(page);
    let removed = Selector::parse(REMOVED).unwrap();
    let ids: Vec<_> = html.select(&removed).map(|e| e.id()).collect();
    for id in ids {
        if let Some(mut node) = html.tree.get_mut(id) {
            node.detach();
        }
    }

    // Links would otherwise come out [bracketed], which is mostly noise,
    // especially in signatures: `Mutex<T: ?[Sized]>`. And code in a code
    // block would come out `quoted`, as if it were inline.
    let plain = Selector::parse("a, pre code").unwrap();
    let ids: Vec<_> = html.select(&plain).map(|e| e.id()).collect();
    for id in ids {
        if let Some(mut node) = html.tree.get_mut(id)
            && let Node::Element(element) = node.value()
        {
            element.name = QualName::new(None, ns!(html), local_name!("span"));
        }
    }
    // Fenced, so code examples and signatures can't be mistaken for prose.
    let pres = Selector::parse("pre").unwrap();
    let ids: Vec<_> = html.select(&pres).map(|e| e.id()).collect();
    for id in ids {
        if let Some(mut node) = html.tree.get_mut(id) {
            node.prepend(Node::Text(Text {
                text: "```rust\n".into(),
            }));
            node.append(Node::Text(Text {
                text: "\n```".into(),
            }));
        }
    }

    let main = Selector::parse("#main-content").unwrap();
    let main = html
        .select(&main)
        .next()
        .ok_or_else(|| anyhow::anyhow!("not a docs page"))?;
    let fragment = match member {
        None => main.html(),
        Some(member) => member_html(main, member).ok_or_else(|| member_not_found(main, member))?,
    };
    let text = html2text::config::plain().string_from_read(fragment.as_bytes(), WIDTH)?;
    Ok(tidy(&text))
}

/// A "not found" error listing the members `main` does have.
fn member_not_found(main: ElementRef, member: &str) -> anyhow::Error {
    let mut members: Vec<&str> = main
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter_map(|e| member_name(e.value().id()?))
        .collect();
    members.sort();
    members.dedup();
    if members.is_empty() {
        anyhow::anyhow!("'{member}' not found: this item has no methods, fields or variants")
    } else {
        anyhow::anyhow!(
            "'{member}' not found. This item has:\n{}",
            members.join("\n")
        )
    }
}

/// The member an element's id names, e.g. `lock` for `method.lock`.
/// rustdoc numbers repeats, e.g. `method.fmt-1`, which are left out.
fn member_name(id: &str) -> Option<&str> {
    let (prefix, name) = id.split_once('.')?;
    let repeat = name
        .rsplit_once('-')
        .is_some_and(|(_, n)| n.chars().all(|c| c.is_ascii_digit()));
    (MEMBER_ID_PREFIXES.contains(&prefix) && !repeat).then_some(name)
}

/// The HTML documenting `member` within `main`: the first with that name,
/// which puts inherent methods ahead of trait impls'.
fn member_html(main: ElementRef, member: &str) -> Option<String> {
    let header = main
        .descendants()
        .filter_map(ElementRef::wrap)
        .find(|e| e.value().id().and_then(member_name) == Some(member))?;
    // A member with docs is a `<details>` whose `<summary>` holds the header.
    let parent = header.parent().and_then(ElementRef::wrap);
    if let Some(summary) = parent.filter(|p| p.value().name() == "summary")
        && let Some(details) = summary.parent().and_then(ElementRef::wrap)
    {
        return Some(details.html());
    }
    // Fields and variants have their docs as the next siblings instead.
    let mut html = header.html();
    for sibling in header.next_siblings().filter_map(ElementRef::wrap) {
        let class = sibling.value().attr("class").unwrap_or("");
        if !class.contains("docblock") && !class.contains("item-info") {
            break;
        }
        html.push_str(&sibling.html());
    }
    Some(html)
}

/// `text` without trailing spaces or runs of blank lines.
fn tidy(text: &str) -> String {
    let mut out = String::new();
    let mut blank = true;
    for line in text.lines().map(str::trim_end) {
        if line.is_empty() {
            if !blank {
                out.push('\n');
            }
            blank = true;
        } else {
            out.push_str(line);
            out.push('\n');
            blank = false;
        }
    }
    out.trim_end().to_string()
}

/// `text`'s lines from `offset` (counting from 1), at most `limit` of them,
/// plus a note saying where to read on if there's more.
fn paged(text: &str, offset: u64, limit: u64) -> String {
    let total = text.lines().count() as u64;
    if offset > total.max(1) {
        return format!("(the page has {total} lines; offset {offset} is past the end)");
    }
    let end = (offset - 1 + limit).min(total);
    let mut out: String = text
        .lines()
        .skip(offset as usize - 1)
        .take(limit as usize)
        .map(|line| format!("{line}\n"))
        .collect();
    if offset > 1 || end < total {
        out.push_str(&format!("[lines {offset}-{end} of {total}"));
        if end < total {
            out.push_str(&format!("; read on with offset {}", end + 1));
        }
        out.push_str("]\n");
    }
    out
}

/// `value` as a whole number from `min` to `max`, or `None` if it's absent.
fn whole_number(
    value: &serde_json::Value,
    name: &str,
    min: u64,
    max: u64,
) -> anyhow::Result<Option<u64>> {
    match value {
        serde_json::Value::Null => Ok(None),
        value => match value.as_u64() {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ if max == u64::MAX => anyhow::bail!("'{name}' must be a whole number from {min}"),
            _ => anyhow::bail!("'{name}' must be a whole number from {min} to {max}"),
        },
    }
}

/// GETs `url`, following redirects, and returns the URL it ended up at and
/// the body.
async fn get(url: &str) -> anyhow::Result<(String, String)> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let request = Request::get(&url).body(Body::empty())?;
        let response = Client::new().send(request).await?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|l| l.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("{url} redirected without a location"))?;
            url = resolve(&url, location);
            continue;
        }
        let mut body = response.into_body();
        let bytes = body.contents().await?;
        if !status.is_success() {
            anyhow::bail!("{url} returned {}", status.as_u16());
        }
        return Ok((url, String::from_utf8_lossy(bytes).into_owned()));
    }
    anyhow::bail!("{url}: too many redirects")
}

/// A redirect's `location` as a full URL: it may be relative to `url`'s
/// host.
fn resolve(url: &str, location: &str) -> String {
    if location.starts_with("https://") || location.starts_with("http://") {
        return location.to_string();
    }
    let origin_end = url
        .find("://")
        .and_then(|scheme| url[scheme + 3..].find('/').map(|i| scheme + 3 + i))
        .unwrap_or(url.len());
    if location.starts_with('/') {
        format!("{}{location}", &url[..origin_end])
    } else {
        let dir = url
            .rfind('/')
            .filter(|&i| i >= origin_end)
            .unwrap_or(url.len());
        format!("{}/{location}", &url[..dir])
    }
}

export!(RustDocs);

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<(String, String)> {
        all_items(
            r#"<h3 id="structs">Structs</h3><ul class="all-items">
            <li><a href="sync/struct.Mutex.html">sync::Mutex</a></li>
            <li><a href="sync/struct.MutexGuard.html">sync::MutexGuard</a></li>
            <li><a href="sync/mpsc/struct.Sender.html">sync::mpsc::Sender</a></li>
            </ul><h3 id="functions">Functions</h3><ul class="all-items">
            <li><a href="fn.spawn.html">spawn</a></li></ul>"#,
        )
    }

    #[test]
    fn strips_crate_prefixes() {
        assert_eq!(item_path("tokio", "tokio::sync::Mutex"), "sync::Mutex");
        assert_eq!(item_path("tokio", "crate::sync"), "sync");
        assert_eq!(item_path("tokio", "tokio"), "");
        assert_eq!(item_path("serde-json", "serde_json::Value"), "Value");
        assert_eq!(item_path("tokio", "tokio_util::codec"), "tokio_util::codec");
    }

    #[test]
    fn locates_pages_modules_and_members() {
        let items = items();
        assert_eq!(
            locate("sync::Mutex", &items).unwrap(),
            Location::Page("sync/struct.Mutex.html".to_string())
        );
        assert_eq!(
            locate("sync::mpsc", &items).unwrap(),
            Location::Page("sync/mpsc/index.html".to_string())
        );
        assert_eq!(
            locate("sync::Mutex::lock", &items).unwrap(),
            Location::Member("sync/struct.Mutex.html".to_string(), "lock")
        );
    }

    #[test]
    fn suggests_close_matches() {
        let err = locate("Mutex", &items()).unwrap_err().to_string();
        assert_eq!(
            err,
            "'Mutex' not found. Close matches:\nsync::Mutex\nsync::MutexGuard"
        );
        assert_eq!(
            locate("zzz", &items()).unwrap_err().to_string(),
            "'zzz' not found"
        );
    }

    const PAGE: &str = r##"<html><body><nav>sidebar</nav>
        <section id="main-content"><div class="main-heading"><h1>Struct <span>Mutex</span>
        <button id="copy-path">Copy item path</button></h1><a class="src" href="#">Source</a></div>
        <pre class="rust item-decl"><code>pub struct Mutex&lt;T&gt; { /* private fields */ }</code></pre>
        <details class="toggle top-doc" open><summary class="hideme"><span>Expand description</span></summary>
        <div class="docblock"><p>An asynchronous <code>Mutex</code>-like type.</p></div></details>
        <h2 id="implementations">Implementations<a href="#implementations" class="anchor">§</a></h2>
        <h3 class="code-header">impl&lt;T&gt; <a href="struct.Mutex.html">Mutex</a>&lt;T&gt;</h3>
        <details class="toggle method-toggle" open><summary><section id="method.lock" class="method">
        <h4 class="code-header">pub async fn lock(&amp;self) -&gt; MutexGuard&lt;'_, T&gt;</h4></section></summary>
        <div class="docblock"><p>Locks this mutex.</p></div></details>
        <h2 id="trait-implementations">Trait Implementations</h2><div id="trait-implementations-list">
        <details><summary><section class="impl"><h3 class="code-header">impl&lt;T&gt; Debug for Mutex&lt;T&gt;</h3></section></summary>
        <div class="impl-items"><section id="method.fmt"><h4>fn fmt(&amp;self)</h4></section></div></details></div>
        <h2 id="blanket-implementations">Blanket Implementations</h2>
        <div id="blanket-implementations-list"><h3>impl&lt;T&gt; From&lt;T&gt; for T</h3></div>
        </section></body></html>"##;

    #[test]
    fn renders_a_page_without_furniture() {
        assert_eq!(
            render(PAGE, None).unwrap(),
            "# Struct Mutex\n\n\
             ```rust\n\
             pub struct Mutex<T> { /* private fields */ }\n\
             ```\n\n\
             An asynchronous `Mutex`-like type.\n\n\
             ## Implementations\n\n\
             ### impl<T> Mutex<T>\n\n\
             #### pub async fn lock(&self) -> MutexGuard<'_, T>\n\n\
             Locks this mutex.\n\n\
             ## Trait Implementations\n\n\
             ### impl<T> Debug for Mutex<T>"
        );
    }

    #[test]
    fn renders_one_member() {
        assert_eq!(
            render(PAGE, Some("lock")).unwrap(),
            "#### pub async fn lock(&self) -> MutexGuard<'_, T>\n\nLocks this mutex."
        );
        assert_eq!(
            render(PAGE, Some("nope")).unwrap_err().to_string(),
            "'nope' not found. This item has:\nlock"
        );
    }

    #[test]
    fn names_members_by_id() {
        assert_eq!(member_name("method.lock"), Some("lock"));
        assert_eq!(member_name("tymethod.next"), Some("next"));
        assert_eq!(member_name("method.fmt-1"), None);
        assert_eq!(member_name("implementations"), None);
        assert_eq!(member_name("impl-Debug-for-Mutex%3CT%3E"), None);
    }

    #[test]
    fn pages_long_text() {
        let text = "a\nb\nc";
        assert_eq!(paged(text, 1, 10), "a\nb\nc\n");
        assert_eq!(
            paged(text, 1, 2),
            "a\nb\n[lines 1-2 of 3; read on with offset 3]\n"
        );
        assert_eq!(paged(text, 3, 2), "c\n[lines 3-3 of 3]\n");
        assert!(paged(text, 4, 2).contains("past the end"));
    }

    #[test]
    fn resolves_redirects() {
        let url = "https://docs.rs/tokio/latest/";
        assert_eq!(
            resolve(url, "/tokio/1.54.0/tokio/"),
            "https://docs.rs/tokio/1.54.0/tokio/"
        );
        assert_eq!(
            resolve(url, "tokio/"),
            "https://docs.rs/tokio/latest/tokio/"
        );
        assert_eq!(
            resolve(url, "https://example.com/x"),
            "https://example.com/x"
        );
    }
}
