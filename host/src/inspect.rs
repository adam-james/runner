//! What a configured provider or tool can do: what its component imports,
//! and what its config lets the host give it. For showing people before
//! they rely on it, never for enforcement, which `SandboxHooks` and
//! `sandbox_store` do.

use wasmtime::Engine;
use wasmtime::component::Component;

use super::ComponentConfig;

/// Whether a component is a provider or a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentKind {
    Provider,
    Tool,
}

impl std::fmt::Display for ComponentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Provider => "provider",
            Self::Tool => "tool",
        })
    }
}

/// A header the host adds to a component's requests. Only its name: the
/// value is a credential, so it never leaves the host.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HeaderGrant {
    pub name: String,
    pub hosts: Vec<String>,
}

/// A configured component's capabilities, from its imports and config.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    pub kind: ComponentKind,
    /// The `[tools.<name>]` or `[providers.<name>]` key.
    pub name: String,
    pub path: String,
    /// The interfaces the component imports, e.g.
    /// `wasi:http/outgoing-handler@0.2.6`, sorted.
    pub imports: Vec<String>,
    pub allowed_hosts: Vec<String>,
    pub headers: Vec<HeaderGrant>,
    pub dir: Option<String>,
    pub dir_writable: bool,
    pub timeout_secs: Option<u64>,
    /// The memory cap its stores get: its config's, or the default.
    pub max_memory_mb: u64,
    /// Whether `max_memory_mb` is the default because its config sets none.
    pub memory_default: bool,
    pub fresh_per_call: bool,
}

impl Capabilities {
    pub(crate) fn new(
        engine: &Engine,
        component: &Component,
        kind: ComponentKind,
        name: &str,
        config: &ComponentConfig,
    ) -> Self {
        let mut imports: Vec<String> = component
            .component_type()
            .imports(engine)
            .map(|(name, _)| name.to_string())
            .collect();
        imports.sort();
        Self {
            kind,
            name: name.to_string(),
            path: config.path.clone(),
            imports,
            allowed_hosts: config.allowed_hosts.clone(),
            headers: config
                .headers
                .iter()
                .map(|(name, header)| HeaderGrant {
                    name: name.clone(),
                    hosts: header.hosts.clone(),
                })
                .collect(),
            dir: config.dir.clone(),
            dir_writable: config.dir_writable,
            timeout_secs: config.timeout_secs,
            max_memory_mb: crate::memory_cap_mb(config),
            memory_default: config.max_memory_mb.is_none(),
            fresh_per_call: config.fresh_per_call,
        }
    }

    fn imports_interface(&self, package: &str) -> bool {
        self.imports
            .iter()
            .any(|import| import.starts_with(&format!("{package}/")))
    }

    /// Whether it can make HTTP requests at all: it has to import
    /// `wasi:http` and be allowed at least one host.
    pub fn has_network(&self) -> bool {
        !self.allowed_hosts.is_empty() && self.imports_interface("wasi:http")
    }

    /// The hosts it can reach, in plain words.
    pub fn network(&self) -> String {
        if self.has_network() {
            format!("can reach {}", self.allowed_hosts.join(", "))
        } else if !self.allowed_hosts.is_empty() {
            "no network access (hosts are allowed, but it doesn't import wasi:http)".into()
        } else {
            "no network access".into()
        }
    }

    /// The headers the host adds for it, by name; `None` without any.
    pub fn header_note(&self) -> Option<String> {
        if self.headers.is_empty() || !self.has_network() {
            return None;
        }
        // Group headers by where they go, so one credential reads as one clause.
        let mut groups: Vec<(&[String], Vec<&str>)> = Vec::new();
        for header in &self.headers {
            match groups.iter_mut().find(|(hosts, _)| *hosts == header.hosts) {
                Some((_, names)) => names.push(&header.name),
                None => groups.push((&header.hosts, vec![&header.name])),
            }
        }
        let clauses: Vec<String> = groups
            .into_iter()
            .map(|(hosts, names)| {
                let what = match names.as_slice() {
                    [name] => format!("{} {name} header", article(name)),
                    names => format!("{} headers", names.join(", ")),
                };
                let mut sorted = hosts.to_vec();
                sorted.sort();
                let mut allowed = self.allowed_hosts.clone();
                allowed.sort();
                if sorted == allowed {
                    format!("gets {what} there")
                } else {
                    format!("gets {what} for {}", hosts.join(", "))
                }
            })
            .collect();
        Some(clauses.join("; "))
    }

    /// Its filesystem access, in plain words.
    pub fn files(&self) -> String {
        match (&self.dir, self.dir_writable) {
            (None, _) => "no filesystem access".into(),
            (Some(dir), false) => format!("reads {dir}"),
            (Some(dir), true) => format!("reads and writes {dir}"),
        }
    }

    /// Its limits, e.g. `timeout 10s · memory 256 MiB`.
    pub fn limits(&self) -> String {
        let mut limits = Vec::new();
        if let Some(secs) = self.timeout_secs {
            limits.push(format!("timeout {secs}s"));
        }
        limits.push(format!(
            "memory {} MiB{}",
            self.max_memory_mb,
            if self.memory_default {
                " (default)"
            } else {
                ""
            }
        ));
        if self.fresh_per_call {
            limits.push("fresh instance per call".into());
        }
        limits.join(" · ")
    }

    /// One line: `can reach api.tavily.com; gets an authorization header
    /// there; no filesystem access`.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.network()];
        parts.extend(self.header_note());
        parts.push(self.files());
        parts.join("; ")
    }

    /// The full description `runner tools inspect` prints, one field per
    /// line.
    pub fn describe(&self) -> String {
        let mut lines = vec![
            format!("{} {}", self.kind, self.name),
            format!("  {:<10} {}", "component", self.path),
            format!("  {:<10} {}", "summary", self.summary()),
            format!(
                "  {:<10} {}",
                "network",
                if self.allowed_hosts.is_empty() {
                    "none".to_string()
                } else {
                    self.allowed_hosts.join(", ")
                }
            ),
        ];
        if self.headers.is_empty() {
            lines.push(format!("  {:<10} none", "headers"));
        }
        for header in &self.headers {
            lines.push(format!(
                "  {:<10} {} → {} (value kept by the host)",
                "headers",
                header.name,
                header.hosts.join(", ")
            ));
        }
        lines.push(format!("  {:<10} {}", "files", self.files()));
        lines.push(format!("  {:<10} {}", "limits", self.limits()));
        if self.imports_interface("wasi:sockets") {
            lines.push(format!(
                "  {:<10} imports wasi:sockets, but the host allows no socket connections",
                "sockets"
            ));
        }
        lines.push(format!("  {:<10} {}", "imports", self.imports.join(", ")));
        lines.join("\n")
    }
}

/// "a" or "an" for a header name, by how it's said: `an authorization`,
/// `an x-api-key`, `a cookie`.
fn article(name: &str) -> &'static str {
    match name.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u' | 'x' | 'A' | 'E' | 'I' | 'O' | 'U' | 'X') => "an",
        _ => "a",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search() -> Capabilities {
        Capabilities {
            kind: ComponentKind::Tool,
            name: "web_search".into(),
            path: "search.wasm".into(),
            imports: vec![
                "wasi:http/outgoing-handler@0.2.6".into(),
                "wasi:io/streams@0.2.6".into(),
            ],
            allowed_hosts: vec!["api.tavily.com".into()],
            headers: vec![HeaderGrant {
                name: "authorization".into(),
                hosts: vec!["api.tavily.com".into()],
            }],
            dir: None,
            dir_writable: false,
            timeout_secs: None,
            max_memory_mb: crate::DEFAULT_MAX_MEMORY_MB,
            memory_default: true,
            fresh_per_call: false,
        }
    }

    #[test]
    fn summary_names_hosts_headers_and_files() {
        assert_eq!(
            search().summary(),
            "can reach api.tavily.com; gets an authorization header there; no filesystem access"
        );
    }

    #[test]
    fn headers_sent_to_only_some_hosts_say_which() {
        let mut caps = search();
        caps.allowed_hosts.push("tavily.com".into());
        caps.headers.push(HeaderGrant {
            name: "x-trace".into(),
            hosts: vec!["api.tavily.com".into()],
        });
        assert_eq!(
            caps.header_note().as_deref(),
            Some("gets authorization, x-trace headers for api.tavily.com")
        );
    }

    #[test]
    fn hosts_without_wasi_http_are_no_network() {
        let mut caps = search();
        caps.imports
            .retain(|import| !import.starts_with("wasi:http/"));
        caps.dir = Some("/data".into());
        assert_eq!(
            caps.summary(),
            "no network access (hosts are allowed, but it doesn't import wasi:http); \
             reads /data"
        );
    }

    #[test]
    fn unset_memory_cap_reports_the_default() {
        assert!(
            search().describe().contains(&format!(
                "memory {} MiB (default)",
                crate::DEFAULT_MAX_MEMORY_MB
            )),
            "{}",
            search().describe()
        );
    }

    #[test]
    fn describe_lists_limits_and_never_header_values() {
        let mut caps = search();
        caps.dir = Some("/scratch".into());
        caps.dir_writable = true;
        caps.timeout_secs = Some(10);
        caps.max_memory_mb = 256;
        caps.memory_default = false;
        caps.fresh_per_call = true;
        let text = caps.describe();
        assert!(text.starts_with("tool web_search\n"), "{text}");
        assert!(text.contains("reads and writes /scratch"), "{text}");
        assert!(
            text.contains("timeout 10s · memory 256 MiB · fresh instance per call"),
            "{text}"
        );
        assert!(
            text.contains("authorization → api.tavily.com (value kept by the host)"),
            "{text}"
        );
    }
}
