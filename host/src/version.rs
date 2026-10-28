//! Agent versions: what an agent definition allowed, kept so an audit trail
//! can be checked against it after `runner.toml` has changed.
//!
//! A version is a snapshot of the resolved definition: its provider and
//! model, every tool's capabilities and component digest, its host tools,
//! delegates and limits, and a hash of its system prompt. Its id is the
//! snapshot's SHA-256, so the same definition always has the same id and any
//! change, to the config or a component's bytes, makes a new one.
//!
//! Like audit records, a snapshot never holds a credential: headers appear
//! by name and the hosts they go to (`inspect::HeaderGrant`), and the system
//! prompt only as a hash.
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::AgentInfo;
use crate::inspect::Capabilities;

/// Bumped whenever a snapshot field changes meaning or goes away. Part of
/// the snapshot, so a bump makes every version new.
pub const SCHEMA_VERSION: u32 = 1;

/// An agent definition's version: its id and the snapshot it hashes.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentVersion {
    /// `sha256:<hex>` of `definition`'s JSON.
    pub id: String,
    pub definition: Value,
}

/// A provider or tool as configured, and the component file it loaded.
#[derive(Serialize)]
pub(crate) struct ComponentSnapshot<'a> {
    pub digest: Option<&'a str>,
    #[serde(flatten)]
    pub capabilities: &'a Capabilities,
}

#[derive(Serialize)]
pub(crate) struct Snapshot<'a> {
    pub schema: u32,
    pub agent: &'a str,
    pub model: &'a str,
    pub provider: ComponentSnapshot<'a>,
    /// By name.
    pub tools: Vec<ComponentSnapshot<'a>>,
    /// Tools the host provides itself, e.g. `delegate`, sorted.
    pub host_tools: Vec<&'a str>,
    /// Agents it may delegate to, by name. Each child run records its own
    /// version.
    pub delegates: &'a [String],
    pub max_tokens: u32,
    pub max_model_calls: Option<u32>,
    /// SHA-256 of the system prompt it runs with, the default if it sets none.
    pub system_prompt_sha256: String,
}

impl Snapshot<'_> {
    pub(crate) fn version(&self) -> AgentVersion {
        let definition = serde_json::to_value(self).expect("snapshot is serializable");
        AgentVersion {
            id: crate::audit::digest(definition.to_string().as_bytes()),
            definition,
        }
    }
}

/// What a schedule's runs were approved to use: the version of its agent
/// and of every agent it can reach by delegating, transitively, by agent
/// name. A snapshot lists delegates only by name, so pinning the agent's
/// own version alone would let an edit to a delegate widen what the
/// schedule can do.
pub type Pin = BTreeMap<String, String>;

/// `agent`'s pin under the definitions in `agents`, or `None` if it, or an
/// agent it delegates to, isn't one of them.
pub fn pin(agents: &[AgentInfo], agent: &str) -> Option<Pin> {
    let mut pin = Pin::new();
    let mut pending = vec![agent];
    while let Some(name) = pending.pop() {
        if pin.contains_key(name) {
            continue;
        }
        let info = agents.iter().find(|info| info.name == name)?;
        pin.insert(name.to_string(), info.version.clone());
        pending.extend(info.delegates.iter().map(String::as_str));
    }
    Some(pin)
}

/// The agents whose version in `current` isn't the one in `pinned`,
/// sorted. All of `pinned`'s if `current` is `None`, i.e. the agent or a
/// delegate is gone from the config.
pub fn changed(pinned: &Pin, current: Option<&Pin>) -> Vec<String> {
    let empty = Pin::new();
    let current = current.unwrap_or(&empty);
    let mut changed: Vec<String> = pinned
        .keys()
        .chain(current.keys())
        .filter(|agent| pinned.get(*agent) != current.get(*agent))
        .cloned()
        .collect();
    changed.sort();
    changed.dedup();
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, version: &str, delegates: &[&str]) -> AgentInfo {
        AgentInfo {
            name: name.into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            tools: vec![],
            delegates: delegates.iter().map(|d| d.to_string()).collect(),
            max_tokens: 1,
            max_model_calls: None,
            version: version.into(),
            tool_access: Default::default(),
        }
    }

    #[test]
    fn a_pin_covers_every_agent_reachable_by_delegating() {
        let agents = [
            agent("lead", "v1", &["researcher", "writer"]),
            agent("researcher", "v2", &["fetcher", "lead"]),
            agent("writer", "v3", &[]),
            agent("fetcher", "v4", &[]),
            agent("unrelated", "v5", &[]),
        ];
        let pin = pin(&agents, "lead").unwrap();
        assert_eq!(
            pin.into_iter().collect::<Vec<_>>(),
            [
                ("fetcher", "v4"),
                ("lead", "v1"),
                ("researcher", "v2"),
                ("writer", "v3")
            ]
            .map(|(a, v)| (a.to_string(), v.to_string()))
        );
        assert_eq!(super::pin(&agents, "missing"), None);
        assert_eq!(super::pin(&[agent("a", "v1", &["gone"])], "a"), None);
    }

    #[test]
    fn changes_name_every_agent_whose_version_differs() {
        let pinned: Pin = [("a", "v1"), ("b", "v2")]
            .map(|(a, v)| (a.to_string(), v.to_string()))
            .into();
        assert!(changed(&pinned, Some(&pinned)).is_empty());
        let mut current = pinned.clone();
        current.insert("b".into(), "v3".into());
        current.insert("c".into(), "v4".into());
        assert_eq!(changed(&pinned, Some(&current)), ["b", "c"]);
        assert_eq!(changed(&pinned, None), ["a", "b"]);
    }
}
