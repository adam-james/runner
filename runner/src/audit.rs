//! `runner runs audit`: a run's security audit records as a table, and
//! checks that none of them went missing.
use serde_json::Value;

use crate::db::Event;

/// Fields every record has, shown in their own columns.
const COMMON: &[&str] = &[
    "kind",
    "v",
    "seq",
    "ts_ms",
    "agent",
    "session",
    "action",
    "component",
    "decision",
    "outcome",
];

/// One line per record: its sequence number, time since the first record,
/// action, component, decision and outcome, then its other fields.
pub fn render(records: &[Event]) -> String {
    let start = records
        .first()
        .and_then(|record| record.data["ts_ms"].as_u64())
        .unwrap_or(0);
    let mut out = String::new();
    for record in records {
        let data = &record.data;
        let elapsed = data["ts_ms"]
            .as_u64()
            .unwrap_or(start)
            .saturating_sub(start);
        let component = match &data["component"] {
            Value::Object(component) => format!(
                "{} {}",
                component["kind"].as_str().unwrap_or("?"),
                component["name"].as_str().unwrap_or("?"),
            ),
            _ => String::new(),
        };
        let mut line = format!(
            "{:>4} {:>9} {:<24} {:<22} {:<8} {}",
            data["seq"],
            format!("+{}.{:03}s", elapsed / 1000, elapsed % 1000),
            data["action"].as_str().unwrap_or("?"),
            component,
            data["decision"].as_str().unwrap_or(""),
            data["outcome"].as_str().unwrap_or(""),
        );
        if let Value::Object(fields) = data {
            for (key, value) in fields {
                if COMMON.contains(&key.as_str()) || value.is_null() {
                    continue;
                }
                match value {
                    Value::String(text) => line.push_str(&format!(" {key}={text:?}")),
                    value => line.push_str(&format!(" {key}={value}")),
                }
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Why `records` might not be the whole story: a missing or unfinished
/// turn, a gap in the sequence, or actions refused because the turn ran out
/// of audit slots. Empty when the trail is complete.
pub fn check(records: &[Event]) -> Vec<String> {
    let mut problems = Vec::new();
    if records.is_empty() {
        problems.push("no audit records (the run never started, or predates auditing)".into());
        return problems;
    }
    let mut expected = 0;
    let mut in_turn = false;
    for record in records {
        let data = &record.data;
        let seq = data["seq"].as_u64().unwrap_or(0);
        if data["action"] == "turn.started" {
            if in_turn {
                problems.push(format!(
                    "a turn ended without a turn.finished record before #{seq}"
                ));
            }
            in_turn = true;
            expected = 1;
        } else if !in_turn {
            problems.push(format!("record #{seq} is outside any turn"));
            continue;
        }
        if seq != expected {
            problems.push(format!(
                "records #{expected}..#{} are missing",
                seq.saturating_sub(1)
            ));
        }
        expected = seq + 1;
        if data["action"] == "turn.finished" {
            in_turn = false;
            if data["records"].as_u64() != Some(seq) {
                problems.push(format!(
                    "the turn reports {} records but #{seq} is its last",
                    data["records"]
                ));
            }
            if let Some(refused) = data["refused"].as_u64().filter(|&n| n > 0) {
                problems.push(format!(
                    "{refused} actions were refused because the turn's audit log was full"
                ));
            }
        }
    }
    if in_turn {
        problems.push(
            "the last turn has no turn.finished record: it may still be running, or the \
             daemon stopped before its records were saved"
                .into(),
        );
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn trail(records: &[Value]) -> Vec<Event> {
        records
            .iter()
            .enumerate()
            .map(|(i, data)| Event {
                id: i as i64 + 1,
                run_id: "r".into(),
                data: data.clone(),
            })
            .collect()
    }

    fn complete() -> Vec<Value> {
        vec![
            json!({"seq":1, "ts_ms":1000, "action":"turn.started", "component":null}),
            json!({"seq":2, "ts_ms":1012, "action":"http.request",
                "component":{"kind":"tool","name":"web_search","digest":"sha256:ab"},
                "decision":"denied", "outcome":"not_allowed_host", "host":"evil.example",
                "port":null}),
            json!({"seq":3, "ts_ms":1500, "action":"turn.finished", "component":null,
                "outcome":"completed", "records":3, "refused":0}),
        ]
    }

    #[test]
    fn renders_one_line_per_record_with_extra_fields() {
        let text = render(&trail(&complete()));
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains("+0.012s"));
        assert!(lines[1].contains("tool web_search"));
        assert!(lines[1].contains("denied"));
        assert!(lines[1].contains("host=\"evil.example\""));
        assert!(!lines[1].contains("port="));
        assert!(lines[2].contains("records=3"));
    }

    #[test]
    fn a_complete_trail_has_no_problems() {
        assert!(check(&trail(&complete())).is_empty());
    }

    #[test]
    fn gaps_unfinished_turns_and_refusals_are_reported() {
        let mut records = complete();
        records.remove(1);
        assert!(check(&trail(&records))[0].contains("#2..#2 are missing"));

        let unfinished = &complete()[..2];
        assert!(check(&trail(unfinished))[0].contains("no turn.finished"));

        let mut refused = complete();
        refused[2]["refused"] = json!(5);
        assert!(check(&trail(&refused))[0].contains("5 actions were refused"));

        assert!(!check(&[]).is_empty());
    }
}
