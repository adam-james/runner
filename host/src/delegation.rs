//! Service-backed host capabilities. Guests never receive scheduler or database handles.
use std::{future::Future, pin::Pin};

pub type Reply<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

pub trait Delegation: Send + Sync {
    fn delegate(&self, agent: String, prompt: String, deadline_secs: Option<u64>) -> Reply<'_>;
    fn wait(&self, run_id: String) -> Reply<'_>;
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Delegate {
    agent: String,
    prompt: String,
    deadline_secs: Option<u64>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    run_id: String,
}

pub(crate) async fn call(
    context: Option<&dyn Delegation>,
    targets: &[String],
    name: &str,
    input: &str,
) -> Result<String, String> {
    if targets.is_empty() {
        return Err("delegation is not permitted for this agent".into());
    }
    let context = context.ok_or("delegation requires the runner service")?;
    match name {
        "delegate" => {
            let request: Delegate =
                serde_json::from_str(input).map_err(|_| "invalid delegate arguments")?;
            if !targets.contains(&request.agent) {
                return Err("delegation target is not permitted".into());
            }
            context
                .delegate(request.agent, request.prompt, request.deadline_secs)
                .await
        }
        "wait" => {
            let request: Wait =
                serde_json::from_str(input).map_err(|_| "invalid wait arguments")?;
            context.wait(request.run_id).await
        }
        _ => Err("unknown delegation tool".into()),
    }
}

pub(crate) fn definitions(targets: &[String]) -> Vec<crate::ToolDefinition> {
    use serde_json::json;
    if targets.is_empty() {
        return vec![];
    }
    vec![
        crate::ToolDefinition {
            name: "delegate".into(),
            description: "Start a child task in a fresh session with a configured specialist. Pass all needed context in the prompt. Start independent tasks before waiting. Unfinished children are cancelled when this turn ends.".into(),
            input_schema_json: json!({"type":"object","properties":{
                "agent":{"type":"string","enum":targets},
                "prompt":{"type":"string","minLength":1},
                "deadline_secs":{"type":"integer","minimum":1,"maximum":86400}},
                "required":["agent","prompt"],"additionalProperties":false}).to_string(),
        },
        crate::ToolDefinition {
            name: "wait".into(),
            description: "Wait for one direct child run and collect its final saved answer, status, and safe error. Repeated waits return the same result.".into(),
            input_schema_json: json!({"type":"object","properties":{"run_id":{"type":"string"}},
                "required":["run_id"],"additionalProperties":false}).to_string(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake;
    impl Delegation for Fake {
        fn delegate(&self, agent: String, prompt: String, _: Option<u64>) -> Reply<'_> {
            Box::pin(async move { Ok(format!("{agent}:{prompt}")) })
        }
        fn wait(&self, id: String) -> Reply<'_> {
            Box::pin(async move { Ok(id) })
        }
    }

    #[tokio::test]
    async fn host_enforces_targets_and_static_arguments() {
        let targets = vec!["researcher".into()];
        let input = r#"{"agent":"researcher","prompt":"task"}"#;
        assert_eq!(
            call(Some(&Fake), &targets, "delegate", input)
                .await
                .unwrap(),
            "researcher:task"
        );
        for input in [
            r#"{"agent":"reader","prompt":"task"}"#,
            r#"{"agent":"researcher","prompt":"task","model":"override"}"#,
            r#"{"agent":"researcher","prompt":"task","tools":["files"]}"#,
        ] {
            assert!(
                call(Some(&Fake), &targets, "delegate", input)
                    .await
                    .is_err()
            );
        }
        assert!(call(Some(&Fake), &[], "delegate", input).await.is_err());
        assert!(
            call(Some(&Fake), &[], "wait", r#"{"run_id":"x"}"#)
                .await
                .is_err()
        );
        assert!(
            call(None, &targets, "delegate", input)
                .await
                .unwrap_err()
                .contains("service")
        );
        let schema: serde_json::Value =
            serde_json::from_str(&definitions(&targets)[0].input_schema_json).unwrap();
        assert_eq!(
            schema["properties"]["agent"]["enum"],
            serde_json::json!(targets)
        );
    }
}
