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
use serde::Serialize;
use serde_json::Value;

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
