//! Security audit records: what guest components did and what the host
//! allowed or denied, written for someone checking a run afterwards.
//!
//! Threat model: the guests (providers and tools) may be malicious; the host
//! is trusted. So every record is made here, at a boundary the host controls
//! (a tool or provider call, an outbound request, a store's resource limits),
//! from values the host derived itself. Guests have no handle to this module
//! and can't add, change or suppress a record.
//!
//! Records are metadata only: never prompts, tool input or output, request or
//! response bodies, header values, or error text, any of which may carry
//! secrets. The few guest-influenced strings that are kept (a tool name the
//! model asked for, a request's host and method) are bounded, and escaped by
//! JSON encoding.
//!
//! Records go out through the turn's event sink as `{"kind":"audit", ...}`;
//! the service stores them in their own table, not the run's event stream.
//!
//! Flooding: a turn records at most `MAX_EVENTS_PER_TURN` actions. Past that,
//! the host denies further outbound requests and tool and model calls rather
//! than let them go unrecorded, so a guest that floods the log only stops its
//! own run.
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::EventSink;
use crate::activity::bounded_name;

/// Bumped whenever a field changes meaning or goes away.
pub const SCHEMA_VERSION: u32 = 1;

/// The most actions one turn records. A turn's own bookkeeping records
/// (`turn.started`, `audit.capacity_exhausted`, `turn.finished`) don't count.
pub const MAX_EVENTS_PER_TURN: u64 = 4096;

/// A component file's identity for audit records: its SHA-256.
pub(crate) fn digest(bytes: &[u8]) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(bytes);
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Which component an action was by, as the config names it, and the
/// SHA-256 of the file it was loaded from.
#[derive(Debug)]
pub(crate) struct ComponentId {
    pub kind: &'static str,
    pub name: String,
    pub digest: Option<String>,
}

impl ComponentId {
    fn to_json(&self) -> Value {
        json!({"kind": self.kind, "name": bounded_name(&self.name), "digest": self.digest})
    }
}

/// The audit trail of whichever turn is running in a session. Every store of
/// the session's components shares one; each turn starts its own trail in it.
#[derive(Clone, Default)]
pub(crate) struct AuditCell(Arc<Mutex<Option<TurnAudit>>>);

struct TurnAudit {
    agent: String,
    session: String,
    sink: EventSink,
    /// Sequence number of the last record written. Contiguous within a turn,
    /// so a gap means records were lost.
    seq: u64,
    /// Actions given a slot so far, recorded or still in progress.
    reserved: u64,
    /// Actions refused because the turn had no slots left.
    refused: u64,
}

impl TurnAudit {
    /// Writes one record. Holding the cell's lock while calling the sink keeps
    /// records in sequence order; the service's sink only queues them.
    fn write(
        &mut self,
        action: &str,
        component: Option<&ComponentId>,
        mut record: Map<String, Value>,
    ) {
        self.seq += 1;
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        record.insert("kind".into(), "audit".into());
        record.insert("v".into(), SCHEMA_VERSION.into());
        record.insert("seq".into(), self.seq.into());
        record.insert("ts_ms".into(), ts_ms.into());
        record.insert("agent".into(), bounded_name(&self.agent).into());
        record.insert("session".into(), bounded_name(&self.session).into());
        record.insert("action".into(), action.into());
        record.insert(
            "component".into(),
            component.map_or(Value::Null, ComponentId::to_json),
        );
        (self.sink)(Value::Object(record));
    }
}

impl AuditCell {
    /// Starts this turn's trail, replacing any earlier turn's.
    pub(crate) fn begin(&self, agent: &str, session: &str, sink: EventSink) -> TurnGuard {
        let mut turn = TurnAudit {
            agent: agent.to_string(),
            session: session.to_string(),
            sink,
            seq: 0,
            reserved: 0,
            refused: 0,
        };
        turn.write("turn.started", None, Map::new());
        *self.0.lock().unwrap() = Some(turn);
        TurnGuard {
            cell: self.clone(),
            finished: false,
        }
    }

    /// A slot for one action by `component`, or `None` if this turn has used
    /// them all, in which case the caller must not go ahead with it. Outside
    /// a turn (e.g. a host test driving a component directly) nothing is
    /// recorded and the action is allowed.
    pub(crate) fn reserve(
        &self,
        component: &Arc<ComponentId>,
        action: &'static str,
    ) -> Option<Ticket> {
        let mut cell = self.0.lock().unwrap();
        let recorded = match cell.as_mut() {
            None => false,
            Some(turn) if turn.reserved < MAX_EVENTS_PER_TURN => {
                turn.reserved += 1;
                true
            }
            Some(turn) => {
                if turn.refused == 0 {
                    turn.write(
                        "audit.capacity_exhausted",
                        Some(component),
                        Map::from_iter([("limit".into(), MAX_EVENTS_PER_TURN.into())]),
                    );
                }
                turn.refused += 1;
                return None;
            }
        };
        Some(Ticket {
            cell: recorded.then(|| self.clone()),
            component: component.clone(),
            action,
            fields: Map::new(),
        })
    }

    /// Records something the host did on its own account, e.g. enforcing a
    /// limit, which needs no slot reserved beforehand. Dropped once the turn
    /// is out of slots, since the action it records was refused anyway.
    pub(crate) fn note(
        &self,
        component: &Arc<ComponentId>,
        action: &'static str,
        decision: Decision,
        outcome: &'static str,
        fields: Map<String, Value>,
    ) {
        if let Some(mut ticket) = self.reserve(component, action) {
            ticket.fields = fields;
            ticket.finish(decision, outcome);
        }
    }

    fn finish_turn(&self, outcome: &'static str) {
        let Some(mut turn) = self.0.lock().unwrap().take() else {
            return;
        };
        let recorded = turn.seq + 1;
        let refused = turn.refused;
        turn.write(
            "turn.finished",
            None,
            Map::from_iter([
                ("outcome".into(), outcome.into()),
                // Including this one, so a reader can tell none went missing.
                ("records".into(), recorded.into()),
                ("refused".into(), refused.into()),
            ]),
        );
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Decision {
    Allowed,
    Denied,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::Allowed => "allowed",
            Decision::Denied => "denied",
        }
    }
}

/// Ends a turn's trail with a `turn.finished` record; dropped unfinished (the
/// turn was cancelled or hit its deadline), it ends as `interrupted`.
pub(crate) struct TurnGuard {
    cell: AuditCell,
    finished: bool,
}

impl TurnGuard {
    pub(crate) fn finish(mut self, outcome: &'static str) {
        self.finished = true;
        self.cell.finish_turn(outcome);
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.cell.finish_turn("interrupted");
        }
    }
}

/// One action's reserved record, written when it finishes; dropped
/// unfinished, it's written as `interrupted`.
pub(crate) struct Ticket {
    /// `None` when there's no turn to record into.
    cell: Option<AuditCell>,
    component: Arc<ComponentId>,
    action: &'static str,
    fields: Map<String, Value>,
}

impl Ticket {
    /// Adds a metadata field. Strings are bounded; pass only values the host
    /// derived, never payloads, header values or error text.
    pub(crate) fn field(&mut self, key: &'static str, value: impl Into<Value>) {
        let value = match value.into() {
            Value::String(text) => Value::String(bounded_name(&text)),
            value => value,
        };
        self.fields.insert(key.into(), value);
    }

    pub(crate) fn finish(mut self, decision: Decision, outcome: &'static str) {
        self.write(Some(decision), outcome);
    }

    fn write(&mut self, decision: Option<Decision>, outcome: &'static str) {
        let Some(cell) = self.cell.take() else {
            return;
        };
        let mut record = std::mem::take(&mut self.fields);
        record.insert(
            "decision".into(),
            decision.map_or(Value::Null, |d| d.as_str().into()),
        );
        record.insert("outcome".into(), outcome.into());
        // The turn may have ended (and another begun) while this action ran;
        // its record then has nowhere to go and is dropped with the old turn.
        if let Some(turn) = cell.0.lock().unwrap().as_mut() {
            turn.write(self.action, Some(&self.component), record);
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        // Allowed and started, but never finished.
        self.write(Some(Decision::Allowed), "interrupted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture() -> (EventSink, Arc<Mutex<Vec<Value>>>) {
        let records = Arc::new(Mutex::new(Vec::new()));
        let sink_records = records.clone();
        let sink: EventSink = Arc::new(move |value| sink_records.lock().unwrap().push(value));
        (sink, records)
    }

    fn tool() -> Arc<ComponentId> {
        Arc::new(ComponentId {
            kind: "tool",
            name: "weather".into(),
            digest: Some("sha256:00".into()),
        })
    }

    #[test]
    fn records_are_sequenced_and_bracketed_by_the_turn() {
        let (sink, records) = capture();
        let cell = AuditCell::default();
        let turn = cell.begin("default", "s1", sink);
        let mut ticket = cell.reserve(&tool(), "tool.call").unwrap();
        ticket.field("tool", "weather");
        ticket.finish(Decision::Allowed, "ok");
        drop(cell.reserve(&tool(), "http.request").unwrap());
        turn.finish("completed");

        let records = records.lock().unwrap();
        let actions: Vec<_> = records.iter().map(|r| r["action"].clone()).collect();
        assert_eq!(
            actions,
            ["turn.started", "tool.call", "http.request", "turn.finished"]
        );
        let seqs: Vec<_> = records.iter().map(|r| r["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, [1, 2, 3, 4]);
        assert!(
            records
                .iter()
                .all(|r| r["v"] == SCHEMA_VERSION && r["kind"] == "audit")
        );
        assert_eq!(records[1]["component"]["name"], "weather");
        assert_eq!(records[1]["decision"], "allowed");
        assert_eq!(records[2]["outcome"], "interrupted");
        assert_eq!(records[3]["outcome"], "completed");
        assert_eq!(records[3]["records"], 4);
    }

    #[test]
    fn a_dropped_turn_finishes_as_interrupted() {
        let (sink, records) = capture();
        let cell = AuditCell::default();
        drop(cell.begin("default", "s1", sink));
        assert_eq!(records.lock().unwrap()[1]["outcome"], "interrupted");
        // Outside a turn, actions go ahead unrecorded.
        cell.reserve(&tool(), "tool.call")
            .unwrap()
            .finish(Decision::Allowed, "ok");
        assert_eq!(records.lock().unwrap().len(), 2);
    }

    #[test]
    fn exhausted_turns_refuse_actions_and_say_so_once() {
        let (sink, records) = capture();
        let cell = AuditCell::default();
        let turn = cell.begin("default", "s1", sink);
        for _ in 0..MAX_EVENTS_PER_TURN {
            cell.reserve(&tool(), "http.request")
                .unwrap()
                .finish(Decision::Denied, "denied");
        }
        assert!(cell.reserve(&tool(), "http.request").is_none());
        assert!(cell.reserve(&tool(), "tool.call").is_none());
        turn.finish("failed");

        let records = records.lock().unwrap();
        let exhausted: Vec<_> = records
            .iter()
            .filter(|r| r["action"] == "audit.capacity_exhausted")
            .collect();
        assert_eq!(exhausted.len(), 1);
        let last = records.last().unwrap();
        assert_eq!(last["refused"], 2);
        assert_eq!(last["records"].as_u64().unwrap() as usize, records.len());
    }

    #[test]
    fn guest_influenced_strings_are_bounded_and_escaped() {
        let (sink, records) = capture();
        let cell = AuditCell::default();
        let _turn = cell.begin("default", "s1", sink);
        let mut ticket = cell.reserve(&tool(), "tool.call").unwrap();
        ticket.field("tool", format!("x\n{{\"forged\":1}}{}", "y".repeat(500)));
        ticket.finish(Decision::Denied, "unknown_tool");
        let records = records.lock().unwrap();
        let tool = records[1]["tool"].as_str().unwrap();
        assert_eq!(tool.chars().count(), 128);
        let encoded = serde_json::to_string(&records[1]).unwrap();
        assert!(encoded.contains("x\\n{\\\"forged\\\""));
        assert!(records[1].get("forged").is_none());
    }
}
