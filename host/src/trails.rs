//! The `audit_trail` host tool: a finished run's or session's audit records,
//! for an agent reviewing what runs actually did.
//!
//! The records live in the service's database, so the tool goes through the
//! service-backed `Delegation` context, which decides what the caller may
//! read (never its own run) and how the records are rendered. This module
//! only checks the arguments.
use crate::delegation::Delegation;

/// The host tool agents with `read_audit_trails` get.
pub(crate) const TOOL: &str = "audit_trail";

/// Whose audit records to read.
#[derive(Clone, Debug, PartialEq)]
pub enum Trail {
    Run(String),
    Session(String),
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    run_id: Option<String>,
    session_id: Option<String>,
}

pub(crate) async fn call(context: Option<&dyn Delegation>, input: &str) -> Result<String, String> {
    let request: Request =
        serde_json::from_str(input).map_err(|_| "invalid audit_trail arguments")?;
    let trail = match (request.run_id, request.session_id) {
        (Some(id), None) => Trail::Run(id),
        (None, Some(id)) => Trail::Session(id),
        _ => return Err("give exactly one of run_id and session_id".into()),
    };
    let context = context.ok_or("reading audit trails requires the runner service")?;
    context.audit_trail(trail).await
}

pub(crate) fn definitions() -> Vec<crate::ToolDefinition> {
    use serde_json::json;
    vec![crate::ToolDefinition {
        name: TOOL.into(),
        description: "Read a run's or a session's security audit trail: the metadata records \
            the host made of what its provider and tools did (sandboxes created, tool and model \
            calls, outbound requests and whether they were allowed, limits enforced), with any \
            problems in the trail itself (missing records, unfinished turns, refusals because \
            the log was full). Records never include prompts, payloads or credentials. Not \
            available for this run, or any run in its delegation tree."
            .into(),
        input_schema_json: json!({"type":"object","properties":{
            "run_id":{"type":"string","minLength":1},
            "session_id":{"type":"string","minLength":1,
                "description":"Every run in the session, a run at a time."}},
            "minProperties":1,"maxProperties":1,"additionalProperties":false})
        .to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::Reply;

    struct Fake;
    impl Delegation for Fake {
        fn delegate(&self, _: String, _: String, _: Option<u64>) -> Reply<'_> {
            unreachable!()
        }
        fn wait(&self, _: String) -> Reply<'_> {
            unreachable!()
        }
        fn audit_trail(&self, trail: Trail) -> Reply<'_> {
            Box::pin(async move { Ok(format!("{trail:?}")) })
        }
    }

    #[tokio::test]
    async fn takes_exactly_one_target() {
        assert_eq!(
            call(Some(&Fake), r#"{"run_id":"r1"}"#).await.unwrap(),
            r#"Run("r1")"#
        );
        assert_eq!(
            call(Some(&Fake), r#"{"session_id":"s1"}"#).await.unwrap(),
            r#"Session("s1")"#
        );
        for input in [
            "{}",
            r#"{"run_id":"r1","session_id":"s1"}"#,
            r#"{"run_id":"r1","path":"/etc"}"#,
        ] {
            assert!(call(Some(&Fake), input).await.is_err(), "{input}");
        }
        assert!(
            call(None, r#"{"run_id":"r1"}"#)
                .await
                .unwrap_err()
                .contains("service")
        );
    }
}
