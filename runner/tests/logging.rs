mod support;

use axum::{body::Body, http::Request};
use runner::{db::Database, service::Service};
use tower::ServiceExt;

#[tokio::test]
async fn api_logs_include_auth_rejections_and_templates_without_secrets() {
    let capture = support::Capture::default();
    let _subscriber = tracing::subscriber::set_default(capture.subscriber());
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(
        Database::open(&dir.path().join("db")).unwrap(),
        "TOKEN_SECRET".into(),
        vec![host::AgentInfo {
            name: "default".into(),
            provider: "fixture".into(),
            model: "fixture".into(),
            tools: vec![],
            capabilities: Default::default(),
            delegates: vec![],
            max_tokens: host::DEFAULT_MAX_TOKENS,
            max_model_calls: None,
        }],
    );
    for (path, authorized, expected) in [
        ("/v1/agents?QUERY_SECRET", true, 200),
        ("/v1/agents?QUERY_SECRET", false, 401),
        ("/v1/runs/PATH_SECRET?QUERY_SECRET", true, 404),
        ("/UNKNOWN_SECRET?QUERY_SECRET", true, 404),
    ] {
        let mut request = Request::get(path);
        if authorized {
            request = request.header("authorization", "Bearer TOKEN_SECRET");
        }
        let response = service
            .router()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
    let logs = capture.text();
    for expected in [
        "status=200",
        "status=401",
        "status=404",
        "/v1/runs/{id}",
        "unmatched",
    ] {
        assert!(logs.contains(expected), "{expected}: {logs}");
    }
    for secret in [
        "TOKEN_SECRET",
        "PATH_SECRET",
        "QUERY_SECRET",
        "UNKNOWN_SECRET",
    ] {
        assert!(!logs.contains(secret), "{logs}");
    }
}
