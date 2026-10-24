//! Configured components as text, for people and for agents auditing them.
//!
//! The structural summary is an s-expression in WAT's shape: every import
//! and export, nested component by nested component and core module by core
//! module, without types, code or data. Imports are what a component can ask
//! the host for, so they're printed in full; that's what an audit is about.
//! Full WAT (via `wasmprinter`) is opt-in: the python tool's runs to
//! hundreds of megabytes.
use std::{fmt::Write, str::FromStr};

use wasmparser::{Encoding, ExternalKind, Parser, Payload, TypeRef};
use wasmtime::{Error, Result, bail};

use crate::{ComponentConfig, RunnerConfig};

/// The host tool agents with `inspect_components` get.
pub(crate) const TOOL: &str = "inspect_component";

/// The most of a summary the host tool returns, like `MAX_TOOL_OUTPUT`.
const MAX_SUMMARY: usize = 64 * 1024;
/// Core module exports past this many are counted, not named; a module
/// can export thousands of functions, and its imports matter more.
const MAX_EXPORT_NAMES: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Tool,
    Provider,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Tool => "tool",
            Kind::Provider => "provider",
        }
    }
}

/// How much of a component to print.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Detail {
    /// The outer component's imports and exports: its interface to the host.
    Interface,
    /// Every nested component and core module, with their imports and exports.
    Structure,
    /// Complete WAT, code and data included.
    Full,
}

impl FromStr for Detail {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "interface" => Ok(Detail::Interface),
            "structure" => Ok(Detail::Structure),
            "full" => Ok(Detail::Full),
            _ => Err(format!(
                "unknown detail '{s}': use interface, structure or full"
            )),
        }
    }
}

/// Prints the tool or provider `name` from the config at `path`, headed by
/// what its config lets it do. Doesn't need the env vars headers and `dir`
/// reference, so it works away from the daemon.
pub fn print(path: &str, kind: Kind, name: &str, detail: Detail) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::msg(format!("reading config {path}: {e}")))?;
    let config: RunnerConfig =
        toml::from_str(&text).map_err(|e| Error::msg(format!("parsing config {path}: {e}")))?;
    let component = find(&config, kind, name)
        .ok_or_else(|| Error::msg(format!("no {} named '{name}' in {path}", kind.as_str())))?;
    describe(component, kind, name, detail)
}

fn find<'a>(config: &'a RunnerConfig, kind: Kind, name: &str) -> Option<&'a ComponentConfig> {
    match kind {
        Kind::Tool => config.tools.get(name),
        Kind::Provider => config.providers.get(name).map(|p| &p.component),
    }
}

fn describe(component: &ComponentConfig, kind: Kind, name: &str, detail: Detail) -> Result<String> {
    let bytes = std::fs::read(&component.path)
        .map_err(|e| Error::msg(format!("reading component {}: {e}", component.path)))?;
    let mut out = capabilities(component, kind, name, bytes.len());
    match detail {
        Detail::Full => out.push_str(
            &wasmprinter::print_bytes(&bytes)
                .map_err(|e| Error::msg(format!("printing {}: {e}", component.path)))?,
        ),
        _ => out.push_str(&summary(&bytes, detail == Detail::Interface)?),
    }
    Ok(out)
}

/// What the config grants, as WAT comments. Header names and the hosts they
/// go to, never their values.
fn capabilities(component: &ComponentConfig, kind: Kind, name: &str, size: usize) -> String {
    let mut out = format!(
        ";; {} {name}: {}, {size} bytes\n",
        kind.as_str(),
        component.path
    );
    let list = |items: &[String]| {
        if items.is_empty() {
            "none".to_string()
        } else {
            items.join(", ")
        }
    };
    let _ = writeln!(out, ";; allowed_hosts: {}", list(&component.allowed_hosts));
    for (header, config) in &component.headers {
        let _ = writeln!(
            out,
            ";; header {header} (value hidden) sent to: {}",
            list(&config.hosts)
        );
    }
    let _ = writeln!(
        out,
        ";; dir: {}",
        match &component.dir {
            None => "none".to_string(),
            Some(dir) if component.dir_writable => format!("{dir} (read-write)"),
            Some(dir) => format!("{dir} (read-only)"),
        }
    );
    let limit = |value: Option<u64>, unit: &str| {
        value.map_or("none".to_string(), |value| format!("{value}{unit}"))
    };
    let _ = writeln!(
        out,
        ";; timeout: {}, max_memory: {}, fresh_per_call: {}",
        limit(component.timeout_secs, "s"),
        limit(component.max_memory_mb, " MiB"),
        component.fresh_per_call
    );
    out
}

/// The structural summary; `interface` stops at the outer component's own
/// imports and exports.
fn summary(bytes: &[u8], interface: bool) -> Result<String> {
    let mut out = String::new();
    // Nesting depth, and the size of the module or component about to open.
    let mut depth = 0usize;
    let mut size = None;
    let mut modules = 0;
    let mut components = 0;
    // Exports of the core module being printed, named once it ends.
    let mut exports: Vec<String> = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| Error::msg(format!("parsing component: {e}")))?;
        let indent = "  ".repeat(depth);
        let nested = depth > 1;
        if interface && nested && !matches!(payload, Payload::End(_) | Payload::Version { .. }) {
            continue;
        }
        match payload {
            Payload::Version { encoding, .. } => {
                if interface && depth >= 1 {
                    depth += 1;
                    continue;
                }
                let size = size
                    .take()
                    .map_or(String::new(), |n| format!(", {n} bytes"));
                match encoding {
                    Encoding::Component if depth == 0 => out.push_str("(component\n"),
                    Encoding::Component => {
                        let _ = writeln!(out, "{indent}(component ;; component {components}{size}");
                        components += 1;
                    }
                    Encoding::Module if depth == 0 => {
                        bail!("a core module, not a component")
                    }
                    Encoding::Module => {
                        let _ = writeln!(out, "{indent}(core module ;; module {modules}{size}");
                        modules += 1;
                    }
                }
                depth += 1;
            }
            Payload::End(_) => {
                depth -= 1;
                if interface && depth >= 1 {
                    continue;
                }
                let indent = "  ".repeat(depth);
                if !exports.is_empty() {
                    let named = exports.len().min(MAX_EXPORT_NAMES);
                    let more = exports.len() - named;
                    let _ = write!(
                        out,
                        "{indent}  ;; {} exports: {}",
                        exports.len(),
                        exports[..named].join(", ")
                    );
                    if more > 0 {
                        let _ = write!(out, ", ... {more} more");
                    }
                    out.push('\n');
                    exports.clear();
                }
                let _ = writeln!(out, "{indent})");
            }
            Payload::ModuleSection {
                unchecked_range, ..
            }
            | Payload::ComponentSection {
                unchecked_range, ..
            } => {
                let bytes = unchecked_range.end - unchecked_range.start;
                if interface && depth == 1 {
                    let _ = writeln!(out, "{indent};; nested, {bytes} bytes");
                }
                size = Some(bytes);
            }
            Payload::ComponentImportSection(reader) => {
                for import in reader {
                    let import = import.map_err(|e| Error::msg(e.to_string()))?;
                    let _ = writeln!(
                        out,
                        "{indent}(import \"{}\" ({}))",
                        import.name.name,
                        component_kind(&import.ty)
                    );
                }
            }
            Payload::ComponentExportSection(reader) => {
                for export in reader {
                    let export = export.map_err(|e| Error::msg(e.to_string()))?;
                    let _ = writeln!(
                        out,
                        "{indent}(export \"{}\" ({}))",
                        export.name.name,
                        format!("{:?}", export.kind).to_lowercase()
                    );
                }
            }
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import = import.map_err(|e| Error::msg(e.to_string()))?;
                    let kind = match import.ty {
                        TypeRef::Func(_) | TypeRef::FuncExact(_) => "func",
                        TypeRef::Table(_) => "table",
                        TypeRef::Memory(_) => "memory",
                        TypeRef::Global(_) => "global",
                        TypeRef::Tag(_) => "tag",
                    };
                    let _ = writeln!(
                        out,
                        "{indent}(import \"{}\" \"{}\" ({kind}))",
                        import.module, import.name
                    );
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export.map_err(|e| Error::msg(e.to_string()))?;
                    exports.push(match export.kind {
                        ExternalKind::Func | ExternalKind::FuncExact => export.name.to_string(),
                        kind => format!("{} ({})", export.name, format!("{kind:?}").to_lowercase()),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn component_kind(ty: &wasmparser::ComponentTypeRef) -> &'static str {
    use wasmparser::ComponentTypeRef::*;
    match ty {
        Module(_) => "core module",
        Func(_) => "func",
        Value(_) => "value",
        Type(_) => "type",
        Instance(_) => "instance",
        Component(_) => "component",
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Inspect {
    component: String,
    kind: Option<String>,
    detail: Option<String>,
}

/// The host tool: a configured component's summary and config capabilities.
/// Names are looked up in the config, so an agent can never point it at an
/// arbitrary file, and full WAT isn't offered.
pub(crate) async fn call(
    config: std::sync::Arc<RunnerConfig>,
    input: &str,
) -> Result<String, String> {
    let request: Inspect =
        serde_json::from_str(input).map_err(|_| "invalid inspect_component arguments")?;
    let detail = match request.detail.as_deref() {
        None | Some("structure") => Detail::Structure,
        Some("interface") => Detail::Interface,
        Some(_) => return Err("detail must be interface or structure".into()),
    };
    let kind = match request.kind.as_deref() {
        Some("tool") => Kind::Tool,
        Some("provider") => Kind::Provider,
        Some(_) => return Err("kind must be tool or provider".into()),
        None => match (
            config.tools.contains_key(&request.component),
            config.providers.contains_key(&request.component),
        ) {
            (true, false) => Kind::Tool,
            (false, true) => Kind::Provider,
            (true, true) => {
                return Err("both a tool and a provider have that name; set kind".into());
            }
            (false, false) => return Err("no configured component has that name".into()),
        },
    };
    let name = request.component;
    tokio::task::spawn_blocking(move || {
        let component =
            find(&config, kind, &name).ok_or("no configured component has that name")?;
        describe(component, kind, &name, detail)
            .map(|mut text| {
                if text.len() > MAX_SUMMARY {
                    let mut end = MAX_SUMMARY;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                    text.push_str("\n;; ... cut by host; ask for detail \"interface\"");
                }
                text
            })
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "inspection failed".to_string())?
}

pub(crate) fn definitions(config: &RunnerConfig) -> Vec<crate::ToolDefinition> {
    use serde_json::json;
    let names: std::collections::BTreeSet<&String> =
        config.tools.keys().chain(config.providers.keys()).collect();
    vec![crate::ToolDefinition {
        name: TOOL.into(),
        description: "Show a configured tool or provider component as WAT-style text: what its \
            config grants (allowed hosts, which headers are injected and where, directory access, \
            limits) and every import and export of it and its nested core modules. Imports are \
            what it can ask the host for. Header values are never shown."
            .into(),
        input_schema_json: json!({"type":"object","properties":{
            "component":{"type":"string","enum":names},
            "kind":{"type":"string","enum":["tool","provider"],
                "description":"Only needed if a tool and a provider share the name."},
            "detail":{"type":"string","enum":["interface","structure"],
                "description":"interface: the outer component's imports and exports only. \
                structure (default): nested modules too."}},
            "required":["component"],"additionalProperties":false})
        .to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> RunnerConfig {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn capabilities_name_headers_but_never_their_values() {
        let config = config(
            r#"
            [tools.search]
            path = "search.wasm"
            allowed_hosts = ["api.example.com"]
            headers = { authorization = { value = "Bearer sk-secret-value", hosts = ["api.example.com"] } }
            dir = "/data"
            timeout_secs = 5
            "#,
        );
        let text = capabilities(&config.tools["search"], Kind::Tool, "search", 10);
        assert!(text.contains(";; tool search: search.wasm, 10 bytes"));
        assert!(text.contains("allowed_hosts: api.example.com"));
        assert!(text.contains("header authorization (value hidden) sent to: api.example.com"));
        assert!(text.contains("dir: /data (read-only)"));
        assert!(text.contains("timeout: 5s, max_memory: none"));
        assert!(!text.contains("secret"));
    }

    #[test]
    fn summary_lists_imports_exports_and_nested_modules() {
        // An outer component importing an interface, wrapping a core module
        // that imports a function and exports two.
        let wasm = wat_component();
        let text = summary(&wasm, false).unwrap();
        assert!(text.starts_with("(component\n"), "{text}");
        assert!(
            text.contains("(import \"wasi:cli/environment@0.2.0\" (instance))"),
            "{text}"
        );
        assert!(text.contains("(core module ;; module 0"), "{text}");
        assert!(text.contains("(import \"env\" \"get\" (func))"), "{text}");
        assert!(
            text.contains(";; 2 exports: run, memory (memory)"),
            "{text}"
        );
        let interface = summary(&wasm, true).unwrap();
        assert!(
            interface.contains("wasi:cli/environment@0.2.0"),
            "{interface}"
        );
        assert!(!interface.contains("\"env\" \"get\""), "{interface}");
        assert!(interface.contains(";; nested,"), "{interface}");
    }

    #[test]
    fn core_modules_are_not_components() {
        // The smallest core module: magic and version only.
        assert!(summary(b"\0asm\x01\0\0\0", false).is_err());
    }

    #[tokio::test]
    async fn host_tool_only_reads_configured_components() {
        let dir = std::env::temp_dir().join(format!("runner-wat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.wasm");
        std::fs::write(&path, wat_component()).unwrap();
        let config = std::sync::Arc::new(config(&format!(
            "[tools.thing]\npath = {:?}\n[providers.thing]\npath = {:?}\n[providers.main]\npath = {:?}",
            path, path, path
        )));
        let ok = call(config.clone(), r#"{"component":"main"}"#)
            .await
            .unwrap();
        assert!(ok.starts_with(";; provider main:"), "{ok}");
        for input in [
            r#"{"component":"thing"}"#,
            r#"{"component":"missing"}"#,
            r#"{"component":"main","detail":"full"}"#,
            r#"{"component":"main","path":"/etc/passwd"}"#,
        ] {
            assert!(call(config.clone(), input).await.is_err(), "{input}");
        }
        let tool = call(config.clone(), r#"{"component":"thing","kind":"tool"}"#)
            .await
            .unwrap();
        assert!(tool.starts_with(";; tool thing:"), "{tool}");
        let schema: serde_json::Value =
            serde_json::from_str(&definitions(&config)[0].input_schema_json).unwrap();
        assert_eq!(
            schema["properties"]["component"]["enum"],
            serde_json::json!(["main", "thing"])
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A small component, so tests don't depend on built tools.
    fn wat_component() -> Vec<u8> {
        wat::parse_str(
            r#"(component
                (import "wasi:cli/environment@0.2.0" (instance))
                (core module
                    (import "env" "get" (func))
                    (func (export "run"))
                    (memory (export "memory") 0)))"#,
        )
        .unwrap()
    }
}
