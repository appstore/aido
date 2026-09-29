//! The JSON API: thin handlers over the library's existing surfaces.
//! Run-shaped responses reuse the `--json` envelope's field names
//! (`version`, `run_id`, `task`, `artifacts`, `failed_parts`, `error`),
//! so the UI and scripts read one schema; artifact bytes never ride a
//! JSON response — they are served by their own endpoint, by mime.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::middleware::from_fn_with_state;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use tokio_stream::StreamExt as _;

use super::assets;
use super::guard::{self, Guard};
use super::invoke::{self, RunRequest};
use super::runs::{self, Runs, SseEvent};
use crate::domain::{AppError, Artifact, GenerationStatus, RunRecord, JSON_ENVELOPE_VERSION};
use crate::history;
use crate::tasks::{Task, TaskParam, COUNT_RANGE, SIZE_CHOICES, SPEED_RANGE};

/// Everything the handlers share: the request gate and the live runs.
#[derive(Clone)]
pub struct UiState {
    pub guard: Arc<Guard>,
    pub runs: Arc<Runs>,
}

pub fn router(state: Arc<UiState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/tasks", get(tasks_list))
        .route("/api/tasks/{name}", get(task_show))
        .route("/api/runs", get(runs_list).post(runs_create))
        .route("/api/runs/preview", post(runs_preview))
        .route("/api/runs/{id}", get(run_show))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/cancel", post(run_cancel))
        .route("/api/runs/{id}/artifacts/{aid}", get(run_artifact))
        .fallback(assets_fallback)
        .layer(DefaultBodyLimit::max(
            // The plan's own input budgets (per file, per run) still
            // bind below this; the ceiling is only the HTTP headroom.
            160 * 1024 * 1024,
        ))
        .layer(from_fn_with_state(state.clone(), guard::check))
        .with_state(state)
}

async fn health(State(_state): State<Arc<UiState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "ok": true,
    }))
}

async fn assets_fallback(uri: Uri) -> Response {
    assets::serve(uri.path()).into_response()
}

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    let body = serde_json::json!({ "error": { "kind": "usage", "message": message.into() } });
    (status, Json(body)).into_response()
}

fn io_error(context: &str, error: &crate::domain::AppError) -> Response {
    let body = serde_json::json!({ "error": { "kind": "service", "message": format!("{context}: {error}") } });
    (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
}

// --- tasks -----------------------------------------------------------------

async fn tasks_list() -> Response {
    let all = match crate::tasks::load_all() {
        Ok(all) => all,
        Err(e) => return io_error("cannot load tasks", &AppError::usage(format!("{e:#}"))),
    };
    let tasks: Vec<_> = all.values().map(task_dto).collect();
    Json(serde_json::json!({ "tasks": tasks })).into_response()
}

async fn task_show(Path(name): Path<String>) -> Response {
    match crate::tasks::get(&name) {
        Ok(task) => Json(task_dto(&task)).into_response(),
        Err(e) => api_error(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
}

/// One task as the UI sees it: its contract (types, processor, per-part)
/// plus a descriptor per declared parameter, so the run form is built
/// from what the plan will actually accept — the ranges come from the
/// same constants `plan::validate_task_params` enforces.
fn task_dto(task: &Task) -> serde_json::Value {
    serde_json::json!({
        "name": task.name,
        "operation": task.operation.to_string(),
        "summary": crate::domain::first_line(&task.instruction, 96),
        "instruction": task.instruction,
        "profile": task.profile,
        "input_types": task.input_types,
        "required_types": task.required_types,
        "output_types": task.output_types,
        "requires_material": task.requires_material,
        "processor": task.processor.as_str(),
        "per_part": task.per_part,
        "max_inputs": task.max_inputs,
        "builtin": task.builtin,
        "params": task.params.iter().map(|p| param_dto(task, *p)).collect::<Vec<_>>(),
    })
}

fn param_dto(task: &Task, param: TaskParam) -> serde_json::Value {
    let mut spec = serde_json::json!({
        "name": param.name(),
        "default": task.default_param(param.name()),
    });
    let kind = |spec: &mut serde_json::Value, s: &str| spec["kind"] = s.into();
    match param {
        TaskParam::To => kind(&mut spec, "language"),
        TaskParam::Voice => kind(&mut spec, "string"),
        TaskParam::Speed => {
            kind(&mut spec, "number");
            spec["min"] = SPEED_RANGE.0.into();
            spec["max"] = SPEED_RANGE.1.into();
        }
        TaskParam::Count => {
            kind(&mut spec, "integer");
            spec["min"] = COUNT_RANGE.0.into();
            spec["max"] = COUNT_RANGE.1.into();
        }
        TaskParam::Size => {
            kind(&mut spec, "enum");
            spec["choices"] = serde_json::Value::from(SIZE_CHOICES);
        }
    }
    spec
}

// --- history ---------------------------------------------------------------

#[derive(Deserialize)]
struct RunsQuery {
    task: Option<String>,
    status: Option<String>,
}

/// The run list, newest first; `seq` is the CLI's own numbering
/// (`history list` prints 1 = newest), so a UI row and a terminal row
/// address the same run with the same number.
async fn runs_list(Query(query): Query<RunsQuery>) -> Response {
    let ids = match history::list_ids() {
        Ok(ids) => ids,
        Err(e) => return io_error("cannot list history", &AppError::service(format!("{e:#}"))),
    };
    let mut rows = Vec::new();
    for (i, id) in ids.iter().rev().enumerate() {
        let seq = i + 1;
        let status = match history::load_meta(id) {
            Ok(Some(meta)) => {
                let status = status_label(&meta.generation, meta.failed_parts);
                if let Some(want) = &query.task {
                    if meta.task.as_deref() != Some(want.as_str()) {
                        continue;
                    }
                }
                if let Some(want) = &query.status {
                    if &status != want {
                        continue;
                    }
                }
                serde_json::json!({
                    "seq": seq,
                    "run_id": id,
                    "task": meta.task,
                    "status": status,
                    "failed_parts": meta.failed_parts,
                    "parts_total": meta.parts_total,
                    "artifacts": meta.artifacts,
                    "warnings": meta.warnings,
                    "created_at": meta.created_at,
                })
            }
            // The CLI prints "(error: …)" for a damaged entry; the list
            // keeps it so the numbering never lies.
            _ => serde_json::json!({
                "seq": seq,
                "run_id": id,
                "task": serde_json::Value::Null,
                "status": "unreadable",
                "failed_parts": 0,
                "parts_total": 0,
                "artifacts": 0,
                "warnings": 0,
                "created_at": "",
            }),
        };
        rows.push(status);
    }
    Json(serde_json::json!({ "runs": rows })).into_response()
}

/// The list-level status word: `complete`, `partial` (delivered with
/// failed parts — the CLI's exit 6), or the generation's own serde tag
/// (`incomplete`, `failed`, `cancelled`, `running`).
fn status_label(generation: &GenerationStatus, failed_parts: usize) -> String {
    if generation.is_complete() {
        return if failed_parts > 0 {
            "partial".into()
        } else {
            "complete".into()
        };
    }
    match serde_json::to_value(generation) {
        Ok(value) => value["status"].as_str().unwrap_or("unknown").to_string(),
        Err(_) => "unknown".into(),
    }
}

/// One run's record — the envelope's fields plus what only history
/// keeps (warnings, summary, deliveries). Artifact entries carry no
/// bytes; [`run_artifact`] serves those.
async fn run_show(Path(id): Path<String>) -> Response {
    let record = match crate::app::resolve_run(&id) {
        Ok(record) => record,
        Err(e) => return api_error(StatusCode::NOT_FOUND, format!("{e:#}")),
    };
    Json(run_dto(&record)).into_response()
}

/// The record as the done frame and the detail endpoint both serve it
/// (runs.rs flattens this into its SSE `done`).
pub(super) fn run_dto(record: &RunRecord) -> serde_json::Value {
    serde_json::json!({
        "version": JSON_ENVELOPE_VERSION,
        "run_id": record.run_id,
        "task": record.task,
        "created_at": record.created_at,
        "status": record.generation,
        "summary": record.summary,
        "artifacts": record.artifacts.iter().map(artifact_dto).collect::<Vec<_>>(),
        "warnings": record.warnings,
        "failed_parts": record.failed_parts.iter()
            .map(|(part, error)| serde_json::json!({ "part": part, "error": error }))
            .collect::<Vec<_>>(),
        "parts_total": record.parts_total,
        "deliveries": record.deliveries,
        "stages": record.stages,
        "last_stage_len": record.last_stage_len,
    })
}

fn artifact_dto(artifact: &Artifact) -> serde_json::Value {
    serde_json::json!({
        "id": artifact.id,
        "kind": artifact.kind,
        "mime": artifact.mime,
        "format": artifact.format,
        "size": artifact.bytes.len(),
        "provenance": artifact.provenance,
    })
}

/// One artifact's bytes, as the mime says — a text file downloads as
/// text, an image renders, an audio artifact plays.
async fn run_artifact(Path((id, aid)): Path<(String, String)>) -> Response {
    let record = match crate::app::resolve_run(&id) {
        Ok(record) => record,
        Err(e) => return api_error(StatusCode::NOT_FOUND, format!("{e:#}")),
    };
    let Some(artifact) = record.artifacts.iter().find(|a| a.id == aid) else {
        return api_error(
            StatusCode::NOT_FOUND,
            format!("run {id} has no artifact '{aid}'"),
        );
    };
    let name = crate::output::artifact_file_name(artifact);
    (
        [
            (header::CONTENT_TYPE, artifact.mime.clone()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{name}\""),
            ),
        ],
        artifact.bytes.clone(),
    )
        .into_response()
}

// --- starting runs ---------------------------------------------------------

/// Parse the multipart body: one `request` field (the JSON RunRequest)
/// and any number of `file` fields, in order.
async fn parse_multipart(
    mut multipart: Multipart,
) -> Result<(RunRequest, Vec<(String, Vec<u8>)>), String> {
    let mut request: Option<RunRequest> = None;
    let mut files = Vec::new();
    while let Some(field) = multipart.next_field().await.map_err(|e| e.to_string())? {
        match field.name().unwrap_or_default() {
            "request" => {
                let text = field.text().await.map_err(|e| e.to_string())?;
                request = Some(
                    serde_json::from_str(&text)
                        .map_err(|e| format!("the request field is not valid: {e}"))?,
                );
            }
            "file" => {
                let name = field.file_name().unwrap_or("upload").to_string();
                let bytes = field.bytes().await.map_err(|e| e.to_string())?;
                files.push((name, bytes.to_vec()));
            }
            other => return Err(format!("unexpected multipart field '{other}'")),
        }
    }
    let request = request.ok_or("the body must carry one 'request' field")?;
    Ok((request, files))
}

/// Land the uploads and build the plan — the blocking half of starting
/// a run, off the async workers. Returns Err with the message the CLI
/// would have printed for the same mistake.
fn prepare(
    request: RunRequest,
    files: Vec<(String, Vec<u8>)>,
) -> Result<crate::plan::ExecutionPlan, AppError> {
    let (landed, paths) = invoke::land_uploads(files)?;
    let invocation = invoke::parse(&request, &paths)?;
    let plan = invoke::build_plan(&invocation)?;
    // The plan holds the gathered inputs; the landed copies can go.
    drop(landed);
    Ok(plan)
}

/// Start a run. The plan (validation included) is built here, so every
/// usage mistake answers immediately with the CLI's own message; the
/// run id exists before the first request, exactly as run_task reserves
/// it, so a cancel mid-flight still records.
async fn runs_create(State(state): State<Arc<UiState>>, multipart: Multipart) -> Response {
    let (request, files) = match parse_multipart(multipart).await {
        Ok(parsed) => parsed,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, message),
    };
    let built = tokio::task::spawn_blocking(move || prepare(request, files)).await;
    let plan = match built {
        Ok(Ok(plan)) => plan,
        Ok(Err(e)) => return api_error(StatusCode::BAD_REQUEST, e.chain_inline()),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    };
    let run_id = if plan.record_history {
        history::new_run_id()
    } else {
        history::stamp_now()
    };
    runs::spawn(plan, state.runs.clone(), run_id.clone());
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "run_id": run_id })),
    )
        .into_response()
}

/// The dry-run of the UI: the same plan a run would build, described,
/// with zero requests (the provider never hears about it).
async fn runs_preview(State(_state): State<Arc<UiState>>, multipart: Multipart) -> Response {
    let (request, files) = match parse_multipart(multipart).await {
        Ok(parsed) => parsed,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, message),
    };
    let built = tokio::task::spawn_blocking(move || {
        let (landed, paths) = invoke::land_uploads(files)?;
        let mut invocation = invoke::parse(&request, &paths)?;
        invocation.cli.dry_run = true;
        invoke::build_plan(&invocation).map(|plan| (plan, landed))
    })
    .await;
    let (plan, _landed) = match built {
        Ok(Ok(built)) => built,
        Ok(Err(e)) => return api_error(StatusCode::BAD_REQUEST, e.chain_inline()),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    };
    Json(serde_json::json!({
        "text": crate::plan::describe(&plan),
        "task": plan.task.name,
        "profile": plan.resolved.profile_name,
        "model": plan.resolved.model,
        "adapter": plan.resolved.adapter.to_string(),
        "steps": plan.steps.iter().map(|s| serde_json::json!({
            "label": s.label,
            "part": s.part,
            "role": format!("{:?}", s.role).to_lowercase(),
        })).collect::<Vec<_>>(),
        "destinations": plan.destinations.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
        "credentials_available": plan.credentials_available,
    }))
    .into_response()
}

/// One live run's progress, as SSE. The stream ends when the run does
/// (its closing frame is `done`, `error` or `cancelled`); a run that is
/// not live answers 404 and the detail endpoint tells the rest.
async fn run_events(State(state): State<Arc<UiState>>, Path(id): Path<String>) -> Response {
    let Some(receiver) = state.runs.subscribe(&id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            format!("no live run '{id}'; GET /api/runs/{id} tells what it became"),
        );
    };
    let stream = tokio_stream::wrappers::BroadcastStream::new(receiver).map(|item| {
        let event = match item {
            Ok(event) => event,
            // The subscriber fell behind a fast stream; history keeps
            // the whole truth, and the closing frame still arrives.
            Err(_) => SseEvent::Warning {
                text: "events were skipped to keep up; the final frame is authoritative".into(),
            },
        };
        let data = serde_json::to_string(&event)
            .unwrap_or_else(|_| r#"{"type":"warning","text":"an event failed to encode"}"#.into());
        Ok::<_, std::convert::Infallible>(Event::default().data(data))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

async fn run_cancel(State(state): State<Arc<UiState>>, Path(id): Path<String>) -> Response {
    if state.runs.cancel(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_error(
            StatusCode::NOT_FOUND,
            format!("no live run '{id}' to cancel"),
        )
    }
}
