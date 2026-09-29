//! In-process HTTP contract tests: the router is exercised directly via
//! `tower::ServiceExt::oneshot` — no sockets, fast and hermetic. SSE streaming
//! is intentionally not asserted here (covered by live smoke runs).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

use swarm_api::spawner::{DemoSpawner, ExecutorFactory, SimulatedImplementer};
use swarm_api::state::AppState;
use swarm_core::mail::Assignment;
use swarm_supervisor::{Supervisor, SupervisorConfig};

fn setup() -> (axum::Router, Arc<DemoSpawner>) {
    setup_with_token(None)
}

fn setup_with_token(token: Option<&str>) -> (axum::Router, Arc<DemoSpawner>) {
    let factory: ExecutorFactory = Arc::new(|_a: &Assignment| {
        Box::new(SimulatedImplementer {
            work_for: Duration::from_secs(3600), // outlives the test; never finishes
        })
    });
    let spawner = Arc::new(DemoSpawner::new(factory));
    let config = SupervisorConfig::default();
    let supervisor = Supervisor::new(config, Box::new((*spawner).clone())).expect("valid config");
    spawner.set_handle(supervisor.handle());
    let state = AppState {
        supervisor,
        tasks: Arc::default(),
        spawner: spawner.clone(),
        submit_token: token.map(Arc::<str>::from),
    };
    (swarm_api::router(state), spawner)
}

async fn send_authorized(
    app: axum::Router,
    method: &'static str,
    uri: &str,
    json: Option<serde_json::Value>,
    token: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = match json {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let res = app.oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let text = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
        .unwrap_or_default();
    (status, text)
}

async fn send(
    app: axum::Router,
    method: &'static str,
    uri: &str,
    json: Option<serde_json::Value>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match json {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let res = app.oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let text = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
        .unwrap_or_default();
    (status, text)
}

#[tokio::test]
async fn landing_and_dashboard_are_served() {
    let (app, _spawner) = setup();
    // Landing at "/" (project overview).
    let (status, html) = send(app.clone(), "GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("PHOENIX CI"), "landing html expected");

    // Dashboard moved to "/app".
    let (status, dash) = send(app, "GET", "/app", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(dash.contains("Quick pick"), "dashboard html expected");
}


#[tokio::test]
async fn submit_then_snapshot_then_list() {
    let (app, _spawner) = setup();
    let (status, body) = send(
        app.clone(),
        "POST",
        "/tasks",
        Some(serde_json::json!({
            "pr_id": "PR-T1",
            "title": "t",
            "bug_description": "d"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let task_id = serde_json::from_slice::<serde_json::Value>(body.as_bytes())
        .unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Snapshot exists immediately after submit.
    let (status, snap) = send(app.clone(), "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(snap.contains("\"attempt\":1"));

    // Listing includes it.
    let (_, list) = send(app.clone(), "GET", "/tasks", None).await;
    assert!(list.contains("PR-T1"));
}

#[tokio::test]
async fn bad_and_unknown_ids_translate_to_plain_errors() {
    let (app, _spawner) = setup();
    let (status, _) = send(app.clone(), "GET", "/tasks/not-a-uuid", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) =
        send(app.clone(), "GET", "/tasks/00000000-0000-0000-0000-000000000000", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.to_lowercase().contains("no task"),
        "plain-language 404 expected, got: {body}"
    );

    let (status, _) = send(
        app.clone(),
        "POST",
        "/tasks/00000000-0000-0000-0000-000000000000/kill",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn real_mode_rejects_unauthorized_task_writes_before_creating_a_task() {
    let token = "a-very-long-random-development-token-with-adequate-length";
    let (app, _spawner) = setup_with_token(Some(token));
    let spec = serde_json::json!({ "pr_id": "PR-SECRET", "title": "t", "bug_description": "d" });
    for supplied in [None, Some("wrong"), Some("Bearer wrong")] {
        let (status, _) = send_authorized(app.clone(), "POST", "/tasks", Some(spec.clone()), supplied).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (_, list) = send(app.clone(), "GET", "/tasks", None).await;
    assert!(!list.contains("PR-SECRET"));

    let (status, body) = send_authorized(app.clone(), "POST", "/tasks", Some(spec), Some(token)).await;
    assert_eq!(status, StatusCode::OK);
    let task_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["task_id"].as_str().unwrap().to_owned();
    let (status, _) = send(app.clone(), "POST", &format!("/tasks/{task_id}/kill"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send_authorized(app, "POST", &format!("/tasks/{task_id}/kill"), None, Some(token)).await;
    assert_eq!(status, StatusCode::OK);
}
