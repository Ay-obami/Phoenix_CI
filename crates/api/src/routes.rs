//! Route handlers. Every response is derived from supervisor state — the API
//! never invents status on its own.

use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tower_http::cors::{Any, CorsLayer};

use swarm_core::events::SwarmEvent;
use swarm_core::ids::TaskId;
use swarm_core::task::{TaskRecord, TaskSpec};

use crate::state::AppState;

/// Live catalog of demo PRs published by the target repo (prs.json on its
/// default branch). Transformed into dashboard-ready quick-pick entries.
const DEFAULT_PRESETS_URL: &str =
    "https://raw.githubusercontent.com/Ay-obami/swarm-demo-target/main/prs.json";

async fn presets(
    State(_st): State<AppState>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, axum::Json<serde_json::Value>)> {
    let url = std::env::var("SWARM_PRESETS_URL").unwrap_or_else(|_| DEFAULT_PRESETS_URL.into());
    let http = reqwest::Client::new();
    let response = http
        .get(url)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({"error": format!("preset catalog unavailable: {e}")})),
        ))?;
    let body = response.text().await.map_err(|e| (
        StatusCode::BAD_GATEWAY,
        axum::Json(serde_json::json!({"error": format!("preset catalog read failed: {e}")})),
    ))?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({"error": format!("bad preset catalog: {e}")})),
        )
    })?;

    let repo_git = v["repo_git"].as_str().unwrap_or_default().to_string();
    let mut out = Vec::new();
    for p in v["prs"].as_array().cloned().unwrap_or_default() {
        let (Some(branch), Some(title)) = (
            p["branch"].as_str().map(str::to_owned),
            p["title"].as_str().map(str::to_owned),
        ) else {
            continue;
        };
        out.push(serde_json::json!({
            "label": p["label"],
            "title": title,
            "bug": p["description"],
            "repo_url": repo_git,
            "git_ref": branch,
        }));
    }
    Ok(axum::Json(serde_json::Value::Array(out)))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(landing))
        .route("/app", get(index))
        .route("/presets", get(presets))
        .route("/tasks", post(submit_task).get(list_tasks))
        .route("/tasks/:id", get(get_task))

        .route("/tasks/:id/kill", post(kill_task))
        .route("/events", get(events))
        .layer(
            // Demo/dev CORS: the dashboard may be served from another origin.
            // Real-agent writes require a bearer token even when CORS permits a request.
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state)
}

/// Project landing page — what Phoenix CI is, how it works, links.
async fn landing() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../assets/landing.html"))
}

/// Self-hosted demo dashboard (Phase 4): renders the REAL `/events` stream in
/// plain language. No build step, no CDN — one embedded HTML file.
async fn index() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../assets/index.html"))
}


fn authorize_write(st: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let Some(expected) = st.submit_token.as_deref() else { return Ok(()); };
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let provided = supplied.as_bytes();
    let required = expected.as_bytes();
    let valid = provided.len() == required.len()
        && required.iter().zip(provided).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0;
    if valid { Ok(()) } else {
        Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "valid API token required" }))))
    }
}

async fn submit_task(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(spec): Json<TaskSpec>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authorize_write(&st, &headers)?;
    let id = st.supervisor.submit_pr(spec.clone());
    st.tasks.lock().unwrap().insert(id, spec);
    tracing::info!(task_id=%id, "task submitted via API");
    Ok(Json(serde_json::json!({ "task_id": id.to_string() })))
}

async fn list_tasks(State(st): State<AppState>) -> Json<serde_json::Value> {
    let ids: Vec<_> = st.tasks.lock().unwrap().keys().copied().collect();
    let tasks = ids
        .into_iter()
        .filter_map(|id| {
            let rec = st.supervisor.snapshot(id)?;
            Some(serde_json::json!({
                "task_id": id.to_string(),
                "pr_id": rec.spec.pr_id,
                "title": rec.spec.title,
                "status": rec.status,
                "attempt": rec.attempt,
            }))
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "tasks": tasks }))
}

fn parse_id(id: &str) -> Result<TaskId, (StatusCode, Json<serde_json::Value>)> {
    TaskId::parse(id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "That task id is not a valid identifier.",
            })),
        )
    })
}

async fn get_task(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskRecord>, (StatusCode, Json<serde_json::Value>)> {
    let task_id = parse_id(&id)?;
    match st.supervisor.snapshot(task_id) {
        Some(record) => Ok(Json(record)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "No task with that id has been submitted.",
            })),
        )),
    }
}

/// Demo fault injection: kill the current worker; the supervisor's lease
/// expiry does the visible recovery.
async fn kill_task(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authorize_write(&st, &headers)?;
    let task_id = parse_id(&id)?;
    if st.supervisor.snapshot(task_id).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "No task with that id has been submitted." })),
        ));
    }
    match st.spawner.kill_current(task_id) {
        Some(attempt) => Ok(Json(serde_json::json!({
            "killed": true,
            "attempt": attempt,
            "note": "worker aborted; expect worker.failed + task.reassigned within the lease timeout",
        }))),
        None => Ok(Json(serde_json::json!({
            "killed": false,
            "note": "no live worker generation found for this task",
        }))),
    }
}

/// Spec-style dotted event names for SSE `event:` lines.
fn event_name(ev: &SwarmEvent) -> &'static str {
    match ev {
        SwarmEvent::TaskCreated { .. } => "task.created",
        SwarmEvent::WorkerStarted { .. } => "worker.started",
        SwarmEvent::WorkerHeartbeat { .. } => "worker.heartbeat",
        SwarmEvent::WorkerFailed { .. } => "worker.failed",
        SwarmEvent::TaskReassigned { .. } => "task.reassigned",
        SwarmEvent::StaleResultRejected { .. } => "stale_result.rejected",
        SwarmEvent::TestsPassed { .. } => "tests.passed",
        SwarmEvent::TestsFailed { .. } => "tests.failed",
        SwarmEvent::MergeGated { .. } => "merge.gated",
        SwarmEvent::MergeOpened { .. } => "merge.opened",
        SwarmEvent::GithubPushOk { .. } => "github.push_ok",
        SwarmEvent::GithubPrMerged { .. } => "github.pr_merged",
        SwarmEvent::GithubActionFailed { .. } => "github.action_failed",
    }
}

async fn events(State(st): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = st.supervisor.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| {
        match item {
            Ok(ev) => {
                let name = event_name(&ev);
                match serde_json::to_string(&ev) {
                    Ok(data) => Some(Ok::<_, Infallible>(Event::default().event(name).data(data))),
                    Err(e) => {
                        tracing::error!(error = ?e, "failed to serialize event for SSE");
                        None
                    }
                }
            }
            Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "sse subscriber lagged; skipping missed events");
                None
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// Keep IntoResponse import used even if handlers evolve (compile-time canary).
#[allow(unused)]
fn _assert_into_response<T: IntoResponse>(_: T) {}
