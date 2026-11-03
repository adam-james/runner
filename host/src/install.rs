//! Installing a tool from a downloaded component and the manifest published
//! with it: checking the manifest, and the permission prompt shown before
//! the tool is added to a config. The manifest isn't covered by the
//! component's digest, so it's only the author's proposal; the prompt shows
//! it next to what the component actually imports, and nothing is granted
//! until someone confirms.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use wasmparser::{Parser, Payload};
use wasmtime::{Error, Result, bail};

use crate::wat::{Detail, Kind};
use crate::{ComponentConfig, HeaderConfig, SandboxHooks};

/// The interface every tool exports.
const TOOL_EXPORT: &str = "local:agent/tool-api@";

/// A file's digest as configs pin it: `sha256:<hex>`.
pub fn digest(bytes: &[u8]) -> String {
    crate::audit::digest(bytes)
}

/// `value` as `sha256:<lowercase hex>`, from either that or bare hex.
pub fn normalize_digest(value: &str) -> Result<String> {
    let hex = value.strip_prefix("sha256:").unwrap_or(value);
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("'{value}' isn't a SHA-256 digest: use 64 hex digits, optionally after 'sha256:'");
    }
    Ok(format!("sha256:{}", hex.to_ascii_lowercase()))
}

/// A tool's requested grants: the fields of a `[tools.<name>]` table except
/// `path` and `sha256`, which the installer sets.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[serde(default)]
    allowed_hosts: Vec<String>,
    #[serde(default)]
    headers: BTreeMap<String, HeaderConfig>,
    dir: Option<String>,
    #[serde(default)]
    dir_writable: bool,
    timeout_secs: Option<u64>,
    max_memory_mb: Option<u64>,
    #[serde(default)]
    fresh_per_call: bool,
}

/// What the person installing a tool is asked to approve.
pub struct Preview {
    /// The grants, host enforcement, imports and the tool's own
    /// `describe()` text, as `runner tools wat` prints them.
    pub report: String,
    /// Env vars the requested headers and `dir` reference, which must be set
    /// where the daemon runs.
    pub env_vars: Vec<String>,
    /// Requests worth a second look, e.g. hosts the component can't use.
    pub warnings: Vec<String>,
}

/// Checks the manifest `text` for a tool stored at `wasm`, and describes
/// what adding it as `name` would grant.
pub async fn preview(name: &str, wasm: &Path, text: &str) -> Result<Preview> {
    crate::validate_table_key("tool", name)?;
    if matches!(
        name,
        "delegate" | "wait" | crate::wat::TOOL | crate::trails::TOOL
    ) {
        bail!("'{name}' is the name of a host tool; pick another");
    }
    let bytes = std::fs::read(wasm)
        .map_err(|e| Error::msg(format!("reading component {}: {e}", wasm.display())))?;
    let imports = crate::wat::imports(&bytes)?;
    if !exports_tool(&bytes)? {
        bail!(
            "{} isn't a runner tool: it doesn't export {TOOL_EXPORT}*",
            wasm.display()
        );
    }
    let mut config = parse(text)?;
    config.path = wasm.to_string_lossy().into_owned();
    config.sha256 = Some(digest(&bytes));
    let env_vars = env_vars(&config)?;
    // The same checks the daemon makes at startup, short of the env vars.
    SandboxHooks::new(&config)?;
    let warnings = warnings(&config, &imports);
    let report = crate::wat::describe(&config, Kind::Tool, name, Detail::Interface).await?;
    Ok(Preview {
        report,
        env_vars,
        warnings,
    })
}

fn parse(text: &str) -> Result<ComponentConfig> {
    let manifest: Manifest =
        toml::from_str(text).map_err(|e| Error::msg(format!("parsing manifest: {e}")))?;
    for (name, header) in &manifest.headers {
        // A literal credential in a published file would be someone else's
        // key, and sent with every request.
        if !header.value.contains("${") {
            bail!(
                "manifest header '{name}' has a literal value; it must name the env var that \
                 holds the credential, e.g. \"Bearer ${{EXAMPLE_API_KEY}}\""
            );
        }
    }
    Ok(ComponentConfig {
        path: String::new(),
        sha256: None,
        allowed_hosts: manifest.allowed_hosts,
        headers: manifest.headers,
        dir: manifest.dir,
        dir_writable: manifest.dir_writable,
        timeout_secs: manifest.timeout_secs,
        max_memory_mb: manifest.max_memory_mb,
        fresh_per_call: manifest.fresh_per_call,
    })
}

/// Every `${NAME}` the headers and `dir` reference, sorted and deduplicated.
fn env_vars(config: &ComponentConfig) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let templates = config
        .headers
        .values()
        .map(|header| header.value.as_str())
        .chain(config.dir.as_deref());
    for template in templates {
        let mut rest = template;
        while let Some(start) = rest.find("${") {
            let Some(end) = rest[start..].find('}') else {
                bail!("unterminated ${{ in manifest value '{template}'");
            };
            let name = &rest[start + 2..start + end];
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!("'{name}' in manifest value '{template}' isn't an env var name");
            }
            names.push(name.to_string());
            rest = &rest[start + end + 1..];
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn warnings(config: &ComponentConfig, imports: &[String]) -> Vec<String> {
    let imports_http = imports
        .iter()
        .any(|import| import.starts_with("wasi:http/"));
    let imports_files = imports
        .iter()
        .any(|import| import.starts_with("wasi:filesystem/"));
    let mut out = Vec::new();
    if imports_http && config.allowed_hosts.is_empty() {
        out.push(
            "it imports wasi:http but requests no hosts, so its requests will all be refused"
                .into(),
        );
    }
    if !imports_http && !config.allowed_hosts.is_empty() {
        out.push("it requests hosts but doesn't import wasi:http, so it can't use them".into());
    }
    match &config.dir {
        Some(_) if !imports_files => out.push(
            "it requests a dir but doesn't import wasi:filesystem, so it can't use it".into(),
        ),
        Some(dir) if config.dir_writable => {
            out.push(format!("it can change, add and delete files under {dir}"))
        }
        _ => {}
    }
    if config.dir.is_none() && config.dir_writable {
        out.push("dir_writable is set without a dir; it has no effect".into());
    }
    out
}

/// Whether the outer component exports the tool interface.
fn exports_tool(bytes: &[u8]) -> Result<bool> {
    let mut depth = 0usize;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|e| Error::msg(format!("parsing component: {e}")))? {
            Payload::Version { .. } => depth += 1,
            Payload::End(_) => depth -= 1,
            Payload::ComponentExportSection(reader) if depth == 1 => {
                for export in reader {
                    let export = export.map_err(|e| Error::msg(e.to_string()))?;
                    if export.name.name.starts_with(TOOL_EXPORT) {
                        return Ok(true);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

impl Preview {
    /// The prompt text: the report, then the env vars and warnings.
    pub fn render(&self) -> String {
        let mut out = self.report.clone();
        if !self.env_vars.is_empty() {
            let _ = writeln!(
                out,
                "\nEnv vars to set where the daemon runs: {}",
                self.env_vars.join(", ")
            );
        }
        for warning in &self.warnings {
            let _ = writeln!(out, "warning: {warning}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_normalized() {
        let hex = "AB".repeat(32);
        let want = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(normalize_digest(&hex).unwrap(), want);
        assert_eq!(normalize_digest(&format!("sha256:{hex}")).unwrap(), want);
        for bad in ["", "sha256:", "abc", &"g".repeat(64), &"a".repeat(65)] {
            assert!(normalize_digest(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn manifest_rejects_unknown_fields_and_literal_credentials() {
        let error = parse("path = \"/bin/sh\"").err().unwrap().to_string();
        assert!(error.contains("unknown field"), "{error}");
        let error = parse("sha256 = \"x\"").err().unwrap().to_string();
        assert!(error.contains("unknown field"), "{error}");
        let error = parse(
            r#"
            allowed_hosts = ["api.example.com"]
            [headers.Authorization]
            value = "Bearer sk-live-123"
            hosts = ["api.example.com"]
            "#,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("literal value"), "{error}");
    }

    #[test]
    fn env_vars_are_collected_and_checked() {
        let config = parse(
            r#"
            allowed_hosts = ["api.example.com"]
            dir = "${HOME}/.runner/example"
            [headers.Authorization]
            value = "Bearer ${EXAMPLE_KEY}"
            hosts = ["api.example.com"]
            [headers.X-Other]
            value = "${EXAMPLE_KEY}"
            hosts = ["api.example.com"]
            "#,
        )
        .unwrap();
        assert_eq!(env_vars(&config).unwrap(), ["EXAMPLE_KEY", "HOME"]);
        let config = parse("dir = \"${NOT CLOSED\"").unwrap();
        assert!(env_vars(&config).is_err());
        let config = parse("dir = \"${$(id)}\"").unwrap();
        assert!(env_vars(&config).is_err());
    }

    #[test]
    fn warnings_point_out_unusable_and_writable_grants() {
        let config =
            parse("allowed_hosts = [\"a.example\"]\ndir = \"/d\"\ndir_writable = true").unwrap();
        let none = warnings(&config, &[]);
        assert!(none.iter().any(|w| w.contains("doesn't import wasi:http")));
        assert!(
            none.iter()
                .any(|w| w.contains("doesn't import wasi:filesystem"))
        );
        let imports = [
            "wasi:http/outgoing-handler@0.2.6".into(),
            "wasi:filesystem/types@0.2.6".into(),
        ];
        let all = warnings(&config, &imports);
        assert_eq!(all, ["it can change, add and delete files under /d"]);
        let config = parse("").unwrap();
        assert!(warnings(&config, &imports[..1])[0].contains("requests no hosts"));
    }

    fn fixture() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/wasm32-wasip2/release/test_tool.wasm")
    }

    #[tokio::test]
    async fn preview_reports_grants_and_refuses_bad_names_and_non_tools() {
        let wasm = fixture();
        if !wasm.exists() {
            eprintln!("skipping: build the test components first (./build.sh)");
            return;
        }
        let shown = preview("fixture", &wasm, "allowed_hosts = [\"api.example.com\"]")
            .await
            .unwrap();
        assert!(
            shown.report.contains(";; allowed_hosts: api.example.com"),
            "{}",
            shown.report
        );
        assert!(shown.report.contains("untrusted claim"), "{}", shown.report);
        for name in ["two words", "delegate", "audit_trail"] {
            assert!(preview(name, &wasm, "").await.is_err(), "{name}");
        }
        let provider = wasm.with_file_name("test_provider.wasm");
        let error = preview("p", &provider, "").await.err().unwrap().to_string();
        assert!(error.contains("isn't a runner tool"), "{error}");
        let error = preview(
            "h",
            &wasm,
            "allowed_hosts = []\n[headers.A]\nvalue = \"${K}\"\nhosts = [\"x.example\"]",
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("not in allowed_hosts"), "{error}");
    }
}
