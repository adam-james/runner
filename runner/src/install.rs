//! `runner tools add`: download a tool component pinned by its SHA-256,
//! show what its manifest asks for, and on confirmation add it to the
//! config. Works without the daemon, which loads the tool when it restarts.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Url;

/// The largest component accepted. Bundled tools are a few MiB; an
/// interpreter like the python tool is far bigger.
const MAX_COMPONENT: u64 = 256 * 1024 * 1024;
/// The largest manifest accepted.
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_REDIRECTS: usize = 5;

pub struct Add {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub manifest: Option<String>,
    pub config: PathBuf,
    pub store: PathBuf,
    pub yes: bool,
}

/// `~/.runner/components`, where installed components are kept by digest.
pub fn default_store() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".runner/components")
}

pub async fn add(add: Add) -> Result<()> {
    let expected = host::install::normalize_digest(&add.sha256)?;
    let url = parse_url(&add.url)?;
    let manifest_url = match &add.manifest {
        Some(manifest) => parse_url(manifest)?,
        None => manifest_url(&url)?,
    };
    let text = std::fs::read_to_string(&add.config)
        .with_context(|| format!("reading config {}", add.config.display()))?;
    let mut config: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing config {}", add.config.display()))?;
    if config
        .get("tools")
        .and_then(|tools| tools.get(&add.name))
        .is_some()
    {
        bail!(
            "{} already has a tool named '{}'",
            add.config.display(),
            add.name
        );
    }

    let client = client()?;
    let bytes = fetch(&client, &url, MAX_COMPONENT).await?;
    let actual = host::install::digest(&bytes);
    if actual != expected {
        bail!("{url} is {actual}, not the {expected} you asked for; nothing was installed");
    }
    let manifest = fetch(&client, &manifest_url, MAX_MANIFEST).await?;
    let manifest = String::from_utf8(manifest).context("the manifest isn't UTF-8")?;

    let (path, created) = store(&add.store, &expected, &bytes)?;
    let remove = || {
        if created {
            let _ = std::fs::remove_file(&path);
        }
    };
    let preview = match host::install::preview(&add.name, &path, &manifest).await {
        Ok(preview) => preview,
        Err(e) => {
            remove();
            return Err(anyhow::anyhow!(e.to_string()));
        }
    };
    println!("Tool '{}' from {url}", add.name);
    println!("{expected} (matches --sha256)");
    println!(
        "Permissions requested by {manifest_url}; the manifest isn't covered by the digest, \
         so check them:\n"
    );
    print!("{}", preview.render());
    let confirmed = confirm(&add, &add.config.display().to_string());
    if !matches!(confirmed, Ok(true)) {
        remove();
        confirmed?;
        println!("Not added.");
        return Ok(());
    }

    let table = tool_table(&path, &expected, &manifest)?;
    if config.get("tools").is_none() {
        let mut tools = toml_edit::Table::new();
        tools.set_implicit(true);
        config.insert("tools", toml_edit::Item::Table(tools));
    }
    let tools = config["tools"]
        .as_table_mut()
        .context("`tools` in the config isn't a table")?;
    tools.insert(&add.name, toml_edit::Item::Table(table));
    std::fs::write(&add.config, config.to_string())
        .with_context(|| format!("writing config {}", add.config.display()))?;
    println!(
        "Added [tools.{}] to {}. Add it to an agent's `tools` to use it, then restart the daemon.",
        add.name,
        add.config.display()
    );
    Ok(())
}

fn confirm(add: &Add, config: &str) -> Result<bool> {
    if add.yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal, so nothing to confirm with; pass --yes to add it anyway");
    }
    print!(
        "\nAdd tool '{}' to {config} with these permissions? [y/N] ",
        add.name
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// HTTPS, or plain HTTP to this machine for tools served locally. The
/// digest pins the component either way; HTTPS keeps the manifest honest.
fn parse_url(text: &str) -> Result<Url> {
    let url = Url::parse(text).with_context(|| format!("'{text}' isn't a URL"))?;
    if !allowed(&url) {
        bail!("{url}: only https:// URLs, or http:// to localhost, are accepted");
    }
    Ok(url)
}

fn allowed(url: &Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    }
}

/// `…/tool.wasm` → `…/tool.manifest.toml`.
fn manifest_url(url: &Url) -> Result<Url> {
    let Some(stem) = url.path().strip_suffix(".wasm") else {
        bail!("{url} doesn't end in .wasm, so there's no manifest URL to guess; pass --manifest");
    };
    let mut manifest = url.clone();
    manifest.set_path(&format!("{stem}.manifest.toml"));
    manifest.set_query(None);
    Ok(manifest)
}

fn client() -> Result<reqwest::Client> {
    // Follows redirects (release downloads usually redirect to a CDN), but
    // never off HTTPS.
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if !allowed(attempt.url()) {
            attempt.error("redirected off https")
        } else {
            attempt.follow()
        }
    });
    Ok(reqwest::Client::builder()
        .redirect(policy)
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(10 * 60))
        .build()?)
}

async fn fetch(client: &reqwest::Client, url: &Url, max: u64) -> Result<Vec<u8>> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    if response.content_length().is_some_and(|length| length > max) {
        bail!("{url} is larger than the {max}-byte limit");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("downloading {url}"))?;
        if body.len() as u64 + chunk.len() as u64 > max {
            bail!("{url} is larger than the {max}-byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Saves `bytes` as `<store>/sha256-<hex>.wasm`, unless it's already there.
/// Returns its path, and whether this call created it.
fn store(store: &Path, digest: &str, bytes: &[u8]) -> Result<(PathBuf, bool)> {
    std::fs::create_dir_all(store).with_context(|| format!("creating {}", store.display()))?;
    let store = store
        .canonicalize()
        .with_context(|| format!("resolving {}", store.display()))?;
    let path = store.join(format!("{}.wasm", digest.replace(':', "-")));
    if let Ok(existing) = std::fs::read(&path)
        && host::install::digest(&existing) == digest
    {
        return Ok((path, false));
    }
    let temp = store.join(format!(
        ".{}.{}.tmp",
        digest.replace(':', "-"),
        std::process::id()
    ));
    std::fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok((path, true))
}

/// The `[tools.<name>]` table: the stored file, its pinned digest, then the
/// manifest's grants. Round-tripped through `toml` so the author's comments
/// don't land in the config.
fn tool_table(path: &Path, digest: &str, manifest: &str) -> Result<toml_edit::Table> {
    let grants: toml::Table = toml::from_str(manifest).context("parsing manifest")?;
    let grants: toml_edit::DocumentMut = toml::to_string(&grants)?.parse()?;
    let mut table = toml_edit::Table::new();
    table.insert("path", toml_edit::value(path.to_string_lossy().as_ref()));
    table.insert("sha256", toml_edit::value(digest));
    for (key, item) in grants.iter() {
        let mut item = item.clone();
        clear_positions(&mut item);
        table.insert(key, item);
    }
    Ok(table)
}

/// Lets toml_edit place tables copied from another document after their
/// parent, rather than at their old positions.
fn clear_positions(item: &mut toml_edit::Item) {
    if let Some(table) = item.as_table_mut() {
        table.set_position(None);
        for (_, child) in table.iter_mut() {
            clear_positions(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_must_be_https_or_local() {
        for ok in [
            "https://example.com/t.wasm",
            "http://localhost:8000/t.wasm",
            "http://127.0.0.1/t.wasm",
            "http://[::1]/t.wasm",
        ] {
            assert!(parse_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.com/t.wasm",
            "file:///etc/passwd",
            "ftp://x/t.wasm",
            "t.wasm",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn manifest_url_sits_next_to_the_component() {
        let url = Url::parse("https://example.com/releases/v1/tool.wasm?sig=abc").unwrap();
        assert_eq!(
            manifest_url(&url).unwrap().as_str(),
            "https://example.com/releases/v1/tool.manifest.toml"
        );
        assert!(manifest_url(&Url::parse("https://example.com/download/1").unwrap()).is_err());
    }

    #[test]
    fn store_is_content_addressed_and_idempotent() {
        let dir = std::env::temp_dir().join(format!("runner-store-{}", uuid::Uuid::new_v4()));
        let digest = host::install::digest(b"component");
        let (path, created) = store(&dir, &digest, b"component").unwrap();
        assert!(created);
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{}.wasm", digest.replace(':', "-"))
        );
        let (again, created) = store(&dir, &digest, b"component").unwrap();
        assert_eq!((again, created), (path.clone(), false));
        // A file that no longer matches its name is replaced.
        std::fs::write(&path, b"tampered").unwrap();
        assert!(store(&dir, &digest, b"component").unwrap().1);
        assert_eq!(std::fs::read(&path).unwrap(), b"component");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tool_table_keeps_the_config_and_drops_manifest_comments() {
        let mut config: toml_edit::DocumentMut = "# my config\n[tools.old]\npath = \"old.wasm\" # keep\n\n[agents.default]\nprovider = \"p\"\n"
            .parse()
            .unwrap();
        let manifest = "# approved by security (not really)\nallowed_hosts = [\"api.example.com\"]\ntimeout_secs = 30\n[headers.Authorization]\nvalue = \"Bearer ${KEY}\"\nhosts = [\"api.example.com\"]\n";
        let table = tool_table(Path::new("/s/sha256-ab.wasm"), "sha256:ab", manifest).unwrap();
        config["tools"]
            .as_table_mut()
            .unwrap()
            .insert("new", toml_edit::Item::Table(table));
        let text = config.to_string();
        assert!(
            text.starts_with("# my config\n[tools.old]\npath = \"old.wasm\" # keep\n"),
            "{text}"
        );
        assert!(!text.contains("approved by security"), "{text}");
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        let new = &parsed["tools"]["new"];
        assert_eq!(new["path"].as_str(), Some("/s/sha256-ab.wasm"));
        assert_eq!(new["sha256"].as_str(), Some("sha256:ab"));
        assert_eq!(new["timeout_secs"].as_integer(), Some(30));
        assert_eq!(
            new["headers"]["Authorization"]["value"].as_str(),
            Some("Bearer ${KEY}")
        );
        assert_eq!(parsed["agents"]["default"]["provider"].as_str(), Some("p"));
    }
}
