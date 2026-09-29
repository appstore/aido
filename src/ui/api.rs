//! The JSON API: thin handlers over the library's existing surfaces.
//! Run-shaped responses reuse the `--json` envelope's field names
//! (`version`, `run_id`, `task`, `artifacts`, `failed_parts`, `error`),
//! so the UI and scripts read one schema; artifact bytes never ride a
//! JSON response — they are served by their own endpoint, by mime.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use super::assets;
use super::guard::{self, Guard};
use crate::domain::{AppError, Artifact, GenerationStatus, RunRecord, JSON_ENVELOPE_VERSION};
use crate::history;
use crate::tasks::{Task, TaskParam, COUNT_RANGE, SIZE_CHOICES, SPEED_RANGE};

pub fn router(guard: Arc<Guard>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/tasks", get(tasks_list))
        .route("/api/tasks/{name}", get(task_show))
        .route("/api/runs", get(runs_list))
        .route("/api/runs/{id}", get(run_show))
        .route("/api/runs/{id}/artifacts/{aid}", get(run_artifact))
        .fallback(assets_fallback)
        .layer(from_fn_with_state(guard.clone(), guard::check))
        .with_state(guard)
}

async fn health(State(_guard): State<Arc<Guard>>) -> Json<serde_json::Value> {
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

fn run_dto(record: &RunRecord) -> serde_json::Value {
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
