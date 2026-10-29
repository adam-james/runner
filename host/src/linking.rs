//! Which WASI a provider or tool gets: only what its config can grant.
//!
//! wasmtime's `add_to_linker` links every WASI interface it ships, and policy
//! is then whatever its context allows, so a refused socket or file was a
//! plain error in the guest that no audit record mentioned. Instead, this
//! links an explicit list, and every other interface a component imports
//! gets a host stub that records `capability.denied` and traps. A component
//! that imports, say, `wasi:sockets` still loads; it only fails if it uses it,
//! and then the trail says so.
//!
//! Linked:
//! - `wasi:http` (0.2 and 0.3), through the host's allow-list hook;
//! - `wasi:io` (0.2), `wasi:clocks` (0.2 and 0.3) and `wasi:random` (0.2),
//!   which carry no policy;
//! - `wasi:filesystem` (0.2), only when the config mounts a `dir`, with each
//!   refusal recorded (see `filesystem`);
//! - `wasi:cli/stdin`, `stdout` and `stderr` (0.2): stdin is empty and output
//!   is discarded (the sandbox never inherits the host's). Unlinked, a
//!   panic's message or a stray print would trap and read as a denial.
//!
//! Not linked: the rest of `wasi:cli` (environment, arguments, terminals,
//! exit), `wasi:sockets` (HTTP doesn't need them: the host makes the
//! connection), the 0.3 versions of `cli`, `filesystem`, `random` and
//! `sockets`, and any interface a future wasmtime adds.
//!
//! A stub answers in kind where it can, so a guest sees what an ungranted
//! capability would give it rather than dying: a call that returns a result
//! whose error can say "not permitted" is refused with it (and recorded); a
//! query returning a list or an option gets an empty one (not recorded:
//! there's nothing to deny, e.g. reading an empty environment); anything else
//! traps (and is recorded).
use std::sync::Arc;

use serde_json::{Map, Value};
use wasmtime::component::types::{ComponentFunc, ComponentItem};
use wasmtime::component::{Component, Linker, LinkerInstance, ResourceType, Type, Val};
use wasmtime::{Engine, Error, Result};
use wasmtime_wasi::cli::{WasiCli, WasiCliView};
use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
use wasmtime_wasi::random::{WasiRandom, WasiRandomView};

use crate::activity::bounded_name;
use crate::{ComponentConfig, SandboxState, audit};

/// Interfaces linked for every component, as a whole package (`wasi:io/`)
/// or one interface (`wasi:cli/stdout`), with a version prefix.
const LINKED: &[(&str, &str)] = &[
    ("wasi:io/", "0.2."),
    ("wasi:clocks/", "0.2."),
    ("wasi:clocks/", "0.3."),
    ("wasi:random/", "0.2."),
    ("wasi:http/", "0.2."),
    ("wasi:http/", "0.3."),
    ("wasi:cli/stdin", "0.2."),
    ("wasi:cli/stdout", "0.2."),
    ("wasi:cli/stderr", "0.2."),
];

/// Linked only for a component with a mounted `dir`.
const FILESYSTEM: (&str, &str) = ("wasi:filesystem/", "0.2.");

/// Whether `import`, e.g. `wasi:clocks/monotonic-clock@0.2.12`, is among
/// `linked`. Interfaces outside `wasi:` (the tool and provider worlds' own)
/// are left to the linker as before.
fn is_linked(import: &str, linked: &[(&str, &str)]) -> bool {
    let Some((interface, version)) = import.split_once('@') else {
        return !import.starts_with("wasi:");
    };
    !import.starts_with("wasi:")
        || linked.iter().any(|(name, prefix)| {
            version.starts_with(prefix)
                && match name.strip_suffix('/') {
                    Some(package) => interface
                        .strip_prefix(package)
                        .is_some_and(|rest| rest.starts_with('/')),
                    None => interface == *name,
                }
        })
}

/// The interfaces among `imports` that `config` leaves unlinked, so stubbed.
pub(crate) fn unlinked<'a>(imports: &'a [String], config: &ComponentConfig) -> Vec<&'a str> {
    let linked = linked_for(config);
    imports
        .iter()
        .map(String::as_str)
        .filter(|import| !is_linked(import, &linked))
        .collect()
}

fn linked_for(config: &ComponentConfig) -> Vec<(&'static str, &'static str)> {
    let mut linked = LINKED.to_vec();
    if config.dir.is_some() {
        linked.push(FILESYSTEM);
    }
    linked
}

/// Links what `config` grants for `component` and stubs the rest of the
/// WASI it imports.
pub(crate) fn link(
    engine: &Engine,
    linker: &mut Linker<SandboxState>,
    component: &Component,
    config: &ComponentConfig,
) -> Result<()> {
    use wasmtime_wasi::p2::bindings::{cli, clocks, random};
    wasmtime_wasi_io::add_to_linker_async(linker)?;
    cli::stdin::add_to_linker::<_, WasiCli>(linker, SandboxState::cli)?;
    cli::stdout::add_to_linker::<_, WasiCli>(linker, SandboxState::cli)?;
    cli::stderr::add_to_linker::<_, WasiCli>(linker, SandboxState::cli)?;
    clocks::wall_clock::add_to_linker::<_, WasiClocks>(linker, SandboxState::clocks)?;
    clocks::monotonic_clock::add_to_linker::<_, WasiClocks>(linker, SandboxState::clocks)?;
    random::random::add_to_linker::<_, WasiRandom>(linker, SandboxState::random)?;
    random::insecure::add_to_linker::<_, WasiRandom>(linker, SandboxState::random)?;
    random::insecure_seed::add_to_linker::<_, WasiRandom>(linker, SandboxState::random)?;
    wasmtime_wasi::p3::clocks::add_to_linker(linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(linker)?;
    wasmtime_wasi_http::p3::add_to_linker(linker)?;
    let linked = linked_for(config);
    if config.dir.is_some() {
        crate::filesystem::add_to_linker(linker)?;
    }
    let component_type = component.component_type();
    let imports: Vec<_> = component_type.imports(engine).collect();
    // A stubbed interface can re-export a linked one's resource, e.g.
    // `wasi:cli/stdout`'s `output-stream` is `wasi:io/streams`'. That one is
    // the linked interface's to define, so stubs leave it alone.
    let mut linked_resources = Vec::new();
    for (name, item) in &imports {
        if let ComponentItem::ComponentInstance(instance) = &item.ty
            && is_linked(name, &linked)
        {
            for (_, export) in instance.exports(engine) {
                if let ComponentItem::Resource(resource) = export.ty {
                    linked_resources.push(resource);
                }
            }
        }
    }
    let stubs = Stubs {
        engine,
        linked_resources,
    };
    for (name, item) in imports {
        if !is_linked(name, &linked) {
            stubs.define(&mut linker.root(), name, None, item.ty)?;
        }
    }
    Ok(())
}

struct Stubs<'a> {
    engine: &'a Engine,
    linked_resources: Vec<ResourceType>,
}

impl Stubs<'_> {
    /// Defines `item`, imported as `name`, as host stubs: a function
    /// records `capability.denied` and traps, and a resource of the stubbed
    /// interface's own is an empty host type.
    fn define(
        &self,
        linker: &mut LinkerInstance<'_, SandboxState>,
        name: &str,
        interface: Option<&str>,
        item: ComponentItem,
    ) -> Result<()> {
        match item {
            ComponentItem::ComponentInstance(instance) => {
                let mut inner = linker.instance(name)?;
                for (export, item) in instance.exports(self.engine) {
                    self.define(&mut inner, export, Some(name), item.ty)?;
                }
            }
            ComponentItem::ComponentFunc(func) => stub_func(linker, name, interface, &func)?,
            ComponentItem::Resource(resource) if !self.linked_resources.contains(&resource) => {
                linker.resource(name, ResourceType::host::<()>(), |_, _| Ok(()))?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn stub_func(
    linker: &mut LinkerInstance<'_, SandboxState>,
    name: &str,
    interface: Option<&str>,
    func: &ComponentFunc,
) -> Result<()> {
    let denial = Arc::new(Denial {
        interface: interface.unwrap_or_default().to_string(),
        function: name.to_string(),
        reply: reply(func),
    });
    if func.async_() {
        linker.func_new_concurrent(name, move |accessor, _, _, results| {
            let denial = denial.clone();
            Box::pin(async move {
                if denial.recorded() {
                    accessor.with(|mut access| denial.record(access.get()));
                }
                denial.answer(results)
            })
        })
    } else {
        linker.func_new(name, move |mut store, _, _, results| {
            if denial.recorded() {
                denial.record(store.data_mut());
            }
            denial.answer(results)
        })
    }
}

/// What a stub gives back when called.
#[derive(Debug, Clone, PartialEq)]
enum Reply {
    /// Refused, with this error value, as an ungranted capability would be:
    /// e.g. `wasi:sockets`' `access-denied`.
    Refuse(Val),
    /// Nothing there, e.g. no environment variables or preopened
    /// directories: a query, not an attempt to use anything.
    Empty(Val),
    /// No value fits, e.g. a stream or `exit`.
    Trap,
}

/// Error cases that mean "not permitted", most specific first:
/// `wasi:sockets`' and `wasi:filesystem`'s.
const DENIED_CASES: &[&str] = &["access-denied", "not-permitted", "access"];

/// How a stub for `func` replies: a refusal if it returns a result whose
/// error can say "not permitted", nothing if it returns a list or an
/// option, and a trap otherwise.
fn reply(func: &ComponentFunc) -> Reply {
    let mut results = func.results();
    let (Some(result), None) = (results.next(), results.next()) else {
        return Reply::Trap;
    };
    match result {
        Type::List(_) => Reply::Empty(Val::List(Vec::new())),
        Type::Option(_) => Reply::Empty(Val::Option(None)),
        Type::Result(result) => match result.err() {
            None => Reply::Refuse(Val::Result(Err(None))),
            Some(Type::Enum(cases)) => {
                let names: Vec<_> = cases.names().collect();
                match DENIED_CASES.iter().find(|case| names.contains(case)) {
                    Some(case) => Reply::Refuse(Val::Result(Err(Some(Box::new(Val::Enum(
                        case.to_string(),
                    )))))),
                    None => Reply::Trap,
                }
            }
            Some(_) => Reply::Trap,
        },
        _ => Reply::Trap,
    }
}

/// A call to an interface the component wasn't granted.
struct Denial {
    interface: String,
    function: String,
    reply: Reply,
}

impl Denial {
    /// Whether a call is recorded: refusals and traps are, an empty answer
    /// to a query isn't, since nothing was there to deny. The exception is
    /// asking for preopened directories, which is how a guest starts any use
    /// of a filesystem: with nothing mounted, its file operations then fail
    /// inside the guest (`ENOENT`) and never reach the host, so this is the
    /// only record of the attempt. libc asks once per instance.
    fn recorded(&self) -> bool {
        !matches!(self.reply, Reply::Empty(_)) || self.is_preopens_query()
    }

    fn is_preopens_query(&self) -> bool {
        self.interface.starts_with("wasi:filesystem/preopens@")
            && self.function == "get-directories"
    }

    fn answer(&self, results: &mut [Val]) -> Result<()> {
        match &self.reply {
            Reply::Refuse(value) | Reply::Empty(value) => {
                results[0] = value.clone();
                Ok(())
            }
            Reply::Trap => Err(self.error()),
        }
    }

    /// Writes the `capability.denied` record. The names come from the
    /// component's own imports, so they're bounded like any guest-influenced
    /// string.
    fn record(&self, state: &mut SandboxState) {
        let audit = &state.hooks.audit;
        let fields = Map::from_iter([
            (
                "interface".to_string(),
                Value::String(bounded_name(&self.interface)),
            ),
            (
                "function".to_string(),
                Value::String(bounded_name(&self.function)),
            ),
        ]);
        let outcome = match self.reply {
            Reply::Trap => "trap",
            _ => "refused",
        };
        audit.cell.note(
            &audit.id,
            "capability.denied",
            audit::Decision::Denied,
            outcome,
            fields,
        );
    }

    fn error(&self) -> Error {
        Error::msg(format!(
            "{}#{} isn't granted to this component",
            bounded_name(&self.interface),
            bounded_name(&self.function)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_wasi_versions_count_as_linked() {
        let linked = LINKED.to_vec();
        for import in [
            "wasi:io/streams@0.2.12",
            "wasi:clocks/monotonic-clock@0.3.0",
            "wasi:random/insecure-seed@0.2.9",
            "wasi:http/client@0.3.0",
            "wasi:cli/stdout@0.2.9",
            "local:agent/types@0.1.0",
        ] {
            assert!(is_linked(import, &linked), "{import}");
        }
        for import in [
            "wasi:cli/environment@0.2.12",
            "wasi:sockets/tcp@0.2.12",
            "wasi:random/random@0.3.0",
            "wasi:filesystem/types@0.2.12",
            "wasi:io/streams@0.3.0",
            "wasi:clocks/wall-clock",
            "wasi:clocksx/wall-clock@0.2.0",
            "wasi:cli/stdoutx@0.2.12",
            "wasi:cli/stdout@0.3.0",
            "wasi:cli/exit@0.2.12",
        ] {
            assert!(!is_linked(import, &linked), "{import}");
        }
        let mut with_dir = linked.clone();
        with_dir.push(FILESYSTEM);
        assert!(is_linked("wasi:filesystem/types@0.2.12", &with_dir));
    }

    #[test]
    fn stubs_refuse_answer_nothing_or_trap_by_what_a_function_returns() {
        let engine = Engine::default();
        let component = Component::new(
            &engine,
            r#"(component
                (import "wasi:sockets/probe@0.2.0" (instance
                    (type $c (enum "other" "access-denied"))
                    (export "error-code" (type $code (eq $c)))
                    (type $f (enum "busy" "not-permitted" "access"))
                    (export "fs-code" (type $fs (eq $f)))
                    (type $o (enum "other"))
                    (export "odd-code" (type $odd (eq $o)))
                    (export "connect" (func (result (result (error $code)))))
                    (export "open" (func (result (result u32 (error $fs)))))
                    (export "plain" (func (result (result))))
                    (export "vars" (func (result (list string))))
                    (export "cwd" (func (result (option string))))
                    (export "count" (func (result u32)))
                    (export "exit" (func))
                    (export "odd" (func (result (result (error $odd)))))
                ))
            )"#,
        )
        .unwrap();
        let component_type = component.component_type();
        let (_, import) = component_type.imports(&engine).next().unwrap();
        let ComponentItem::ComponentInstance(instance) = import.ty else {
            panic!("an instance import");
        };
        let replies: Vec<_> = instance
            .exports(&engine)
            .filter_map(|(name, export)| match export.ty {
                ComponentItem::ComponentFunc(func) => Some((name, reply(&func))),
                _ => None,
            })
            .collect();
        let refused =
            |case: &str| Reply::Refuse(Val::Result(Err(Some(Box::new(Val::Enum(case.into()))))));
        assert_eq!(
            replies,
            [
                ("connect", refused("access-denied")),
                ("open", refused("not-permitted")),
                ("plain", Reply::Refuse(Val::Result(Err(None)))),
                ("vars", Reply::Empty(Val::List(Vec::new()))),
                ("cwd", Reply::Empty(Val::Option(None))),
                ("count", Reply::Trap),
                ("exit", Reply::Trap),
                ("odd", Reply::Trap),
            ]
        );
        // An empty answer isn't recorded, except to a guest asking for
        // directories with none mounted.
        let empty = |interface: &str, function: &str| Denial {
            interface: interface.into(),
            function: function.into(),
            reply: Reply::Empty(Val::List(Vec::new())),
        };
        assert!(empty("wasi:filesystem/preopens@0.2.12", "get-directories").recorded());
        assert!(!empty("wasi:cli/environment@0.2.12", "get-environment").recorded());
        assert!(!empty("wasi:filesystem/preopens@0.2.12", "other").recorded());
    }
}
