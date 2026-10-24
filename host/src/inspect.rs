//! Metadata-only inspection: never instantiate a guest or resolve header values.
use super::*;

/// Imports describe available interfaces; config describes host grants.
/// These are separate because an imported interface does not itself grant access.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    pub imports: Vec<String>,
    pub allowed_hosts: Vec<String>,
    pub injected_headers: BTreeMap<String, Vec<String>>,
    pub directory: Option<String>,
    pub directory_writable: bool,
    pub timeout_secs: Option<u64>,
    pub max_memory_mb: Option<u64>,
    pub fresh_per_call: bool,
}

impl Capabilities {
    pub(super) fn new(engine: &Engine, component: &Component, config: &ComponentConfig) -> Self {
        let mut imports: Vec<_> = component
            .component_type()
            .imports(engine)
            .map(|(name, _)| name.to_string())
            .collect();
        imports.sort();
        Self {
            imports,
            allowed_hosts: config.allowed_hosts.clone(),
            injected_headers: config
                .headers
                .iter()
                .map(|(name, header)| (name.clone(), header.hosts.clone()))
                .collect(),
            directory: config.dir.clone(),
            directory_writable: config.dir.is_some() && config.dir_writable,
            timeout_secs: config.timeout_secs,
            max_memory_mb: config.max_memory_mb,
            fresh_per_call: config.fresh_per_call,
        }
    }

    pub fn summary(&self) -> String {
        let network = if self.allowed_hosts.is_empty() {
            "none".into()
        } else {
            self.allowed_hosts.join(", ")
        };
        let files = match &self.directory {
            Some(dir) => format!(
                "{dir} ({})",
                if self.directory_writable {
                    "writable"
                } else {
                    "read-only"
                }
            ),
            None => "none".into(),
        };
        let headers = if self.injected_headers.is_empty() {
            "none".into()
        } else {
            self.injected_headers
                .iter()
                .map(|(name, hosts)| format!("{name} → {}", hosts.join(", ")))
                .collect::<Vec<_>>()
                .join("; ")
        };
        format!("network: {network}; files: {files}; injected headers: {headers}")
    }
}

/// Inspect an offline config without requiring credentials or a running daemon.
pub fn tool(path: &str, name: &str) -> Result<Capabilities> {
    let text = std::fs::read_to_string(path)?;
    // Do not include TOML source lines in errors: they may contain credentials.
    let config: RunnerConfig =
        toml::from_str(&text).map_err(|_| Error::msg("invalid runner config"))?;
    let config = config
        .tools
        .get(name)
        .ok_or_else(|| Error::msg(format!("unknown tool '{name}'")))?;
    let mut options = Config::new();
    options
        .wasm_component_model(true)
        .wasm_component_model_async(true);
    let engine = Engine::new(&options)?;
    let component = Component::from_file(&engine, &config.path)?;
    let mut report = Capabilities::new(&engine, &component, config);
    report.directory = config.dir.as_deref().map(expand_env).transpose()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_reports_imports_and_grants_without_credentials() {
        let engine = Engine::default();
        let component = Component::new(
            &engine,
            r#"(component (import "wasi:example/api@0.2.0" (instance)))"#,
        )
        .unwrap();
        let config: ComponentConfig = toml::from_str(
            r#"
path = "unused.wasm"
allowed_hosts = ["api.example.com"]
headers = { authorization = { value = "SECRET ${MISSING_KEY}", hosts = ["api.example.com"] } }
dir_writable = true
"#,
        )
        .unwrap();
        let report = Capabilities::new(&engine, &component, &config);
        assert_eq!(report.imports, ["wasi:example/api@0.2.0"]);
        assert!(!report.directory_writable);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("SECRET"));
        assert!(!json.contains("MISSING_KEY"));
        assert!(report.summary().contains("authorization → api.example.com"));
        assert!(report.summary().contains("files: none"));
    }
}
