//! The JSON API: thin handlers over the library's existing surfaces.
//! Run-shaped responses reuse the `--json` envelope's field names
//! (`version`, `run_id`, `task`, `artifacts`, `failed_parts`), so the
//! UI and scripts read one schema. Two deliberate shape notes: `error`
//! rides only the SSE done frame (a failed run's detail carries
//! `status`/warnings instead), and the run LIST is a summary shape —
//! `status` a flat word, `failed_parts` a count — where the detail
//! carries the full objects. Artifact bytes never ride a JSON response;
//! they are served by their own endpoint, by mime.

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
use super::watches::{self, Watches};
use crate::domain::{AppError, Artifact, GenerationStatus, RunRecord, JSON_ENVELOPE_VERSION};
use crate::history;
use crate::tasks::{Task, TaskParam, COUNT_RANGE, SIZE_CHOICES, SPEED_RANGE};

/// Everything the handlers share: the request gate, the live runs and
/// the watch daemons this server started.
#[derive(Clone)]
pub struct UiState {
    pub guard: Arc<Guard>,
    pub runs: Arc<Runs>,
    pub watches: Arc<Watches>,
}

pub fn router(state: Arc<UiState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/tasks", get(tasks_list).post(tasks_create))
        .route("/api/tasks/{name}", get(task_show).delete(task_delete))
        .route("/api/tasks/{name}/source", get(task_source))
        .route("/api/profiles", get(profiles_list))
        .route("/api/config", get(config_show).put(config_save))
        .route("/api/chain", post(chain_create))
        .route("/api/chain/preview", post(chain_preview))
        .route("/api/runs", get(runs_list).post(runs_create))
        .route("/api/runs/preview", post(runs_preview))
        .route("/api/runs/{id}", get(run_show))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/cancel", post(run_cancel))
        .route("/api/runs/{id}/artifacts/{aid}", get(run_artifact))
        .route("/api/runs/{id}/archive", get(run_archive))
        .route("/api/watches", get(watches_list).post(watches_create))
        .route("/api/watches/preview", post(watches_preview))
        .route("/api/watches/{id}/events", get(watch_events))
        .route("/api/watches/{id}/stop", post(watch_stop))
        .fallback(assets_fallback)
        .layer(DefaultBodyLimit::max(body_limit()))
        .layer(from_fn_with_state(state.clone(), guard::check))
        .with_state(state)
}

/// The HTTP ceiling sits above the run's own input budget (a
/// configurable `input_bytes` larger than the default must not have its
/// requests die at the transport instead of at aido's own rules).
fn body_limit() -> usize {
    const DEFAULT_INPUT_BYTES: u64 = 128 * 1024 * 1024;
    const HEADROOM: u64 = 32 * 1024 * 1024;
    let input = crate::config::load()
        .ok()
        .and_then(|c| c.settings.input_bytes)
        .unwrap_or(DEFAULT_INPUT_BYTES);
    (input + HEADROOM) as usize
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

/// A task file name the UI may write: one path component of plain stem
/// characters, so a request can never escape the tasks directory (the
/// name becomes `<tasks dir>/<name>.toml` verbatim). A name that shadows
/// a built-in is allowed — that override is `load_all`'s own rule.
pub(super) fn task_file_stem(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name the task (a file stem, e.g. `daily-report`)".into());
    }
    if name.len() > 64 {
        return Err("the task name is too long (64 bytes at most)".into());
    }
    let legal = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if !legal || name.starts_with(['-', '.']) {
        return Err(format!(
            "'{name}' is not a usable task file name: use letters, digits, '-', '_' and '.', \
             not starting with '-' or '.'"
        ));
    }
    Ok(name.to_string())
}

#[derive(Deserialize)]
struct TaskPut {
    name: String,
    toml: String,
    #[serde(default)]
    overwrite: bool,
}

/// Create or replace a custom task: parse with the loader's own rules
/// (the file on disk must never become invalid), write atomically, then
/// drop the task cache so the new definition answers the very next
/// request — including the run form's task picker.
async fn tasks_create(Json(body): Json<TaskPut>) -> Response {
    let name = match task_file_stem(&body.name) {
        Ok(name) => name,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, message),
    };
    let task = match crate::tasks::parse_task(&name, &body.toml, false) {
        Ok(task) => task,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let Some(path) = crate::tasks::file_path(&name) else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot determine the tasks directory on this platform",
        );
    };
    if path.exists() && !body.overwrite {
        return api_error(
            StatusCode::CONFLICT,
            format!(
                "task '{name}' already has a file at {}; save with overwrite to replace it",
                path.display()
            ),
        );
    }
    if let Err(e) =
        std::fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new("/")))
    {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot create {}: {e}", path.display()),
        );
    }
    if let Err(e) = crate::output::write_file_atomic(
        body.toml.as_bytes(),
        &path,
        true,
        crate::output::FileMode::Default,
    ) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot write {}: {e:#}", path.display()),
        );
    }
    crate::tasks::invalidate();
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "task": task_dto(&task),
            "path": path.display().to_string(),
        })),
    )
        .into_response()
}

/// The TOML behind a task: the custom file when one exists (it is the
/// definition that actually runs), else the embedded built-in — read-only
/// truth for the wizard's editor view.
async fn task_source(Path(name): Path<String>) -> Response {
    let custom = crate::tasks::file_path(&name).filter(|p| p.exists());
    match custom {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(toml) => Json(serde_json::json!({
                "name": name,
                "builtin": false,
                "path": path.display().to_string(),
                "toml": toml,
            }))
            .into_response(),
            Err(e) => io_error(
                "cannot read the task file",
                &AppError::service(format!("{}: {e}", path.display())),
            ),
        },
        None => match crate::tasks::builtin_source(&name) {
            Some(toml) => Json(serde_json::json!({
                "name": name,
                "builtin": true,
                "path": serde_json::Value::Null,
                "toml": toml,
            }))
            .into_response(),
            None => api_error(StatusCode::NOT_FOUND, format!("unknown task '{name}'")),
        },
    }
}

/// Remove a custom task's file. A built-in without a custom file has
/// nothing on disk to remove — that is a refusal, not a 404 (the name
/// resolves, and will keep resolving after the call).
async fn task_delete(Path(name): Path<String>) -> Response {
    let Some(path) = crate::tasks::file_path(&name) else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot determine the tasks directory on this platform",
        );
    };
    if !path.exists() {
        return match crate::tasks::get(&name) {
            Ok(_) => api_error(
                StatusCode::BAD_REQUEST,
                format!("'{name}' is a built-in task with no file to delete"),
            ),
            Err(e) => api_error(StatusCode::NOT_FOUND, format!("{e:#}")),
        };
    }
    if let Err(e) = std::fs::remove_file(&path) {
        return io_error(
            "cannot delete the task file",
            &AppError::service(format!("{}: {e}", path.display())),
        );
    }
    crate::tasks::invalidate();
    StatusCode::NO_CONTENT.into_response()
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

/// One run as a single download: every artifact under its delivered
/// name plus the history manifest when there is one — the same set
/// `--out-dir` would have written, zipped for the browser that has no
/// directory to receive into.
async fn run_archive(Path(id): Path<String>) -> Response {
    let record = match crate::app::resolve_run(&id) {
        Ok(record) => record,
        Err(e) => return api_error(StatusCode::NOT_FOUND, format!("{e:#}")),
    };
    let manifest = crate::history::history_dir()
        .filter(|dir| dir.join(&record.run_id).is_dir())
        .and_then(|dir| std::fs::read(dir.join(&record.run_id).join("manifest.json")).ok());
    if record.artifacts.is_empty() && manifest.is_none() {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("run {id} has nothing to archive"),
        );
    }
    let name = format!("aido-{}.zip", crate::output::sanitize_stem(&record.run_id));
    let built =
        tokio::task::spawn_blocking(move || build_archive(&record, manifest.as_deref())).await;
    let bytes = match built {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => return io_error("cannot build the archive", &e),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    };
    (
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// manifest.json first, then the artifacts under the names a delivery
/// would use (`artifact_file_name`: stems sanitized, kinds extended) —
/// a reader of the zip and a reader of an `--out-dir` see the same
/// tree. Deflated like every aido zip.
fn build_archive(
    record: &RunRecord,
    manifest: Option<&[u8]>,
) -> Result<Vec<u8>, crate::domain::AppError> {
    use std::io::Write as _;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        if let Some(bytes) = manifest {
            zip.start_file("manifest.json", options)
                .map_err(|e| crate::domain::AppError::service(format!("zip: {e}")))?;
            zip.write_all(bytes)
                .map_err(|e| crate::domain::AppError::service(format!("zip: {e}")))?;
        }
        for artifact in &record.artifacts {
            let name = crate::output::artifact_file_name(artifact);
            zip.start_file(&name, options)
                .map_err(|e| crate::domain::AppError::service(format!("zip: {e}")))?;
            zip.write_all(&artifact.bytes)
                .map_err(|e| crate::domain::AppError::service(format!("zip: {e}")))?;
        }
        zip.finish()
            .map_err(|e| crate::domain::AppError::service(format!("zip: {e}")))?;
    }
    Ok(buf.into_inner())
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

/// Parse the multipart body: one `request` field (the JSON body of a
/// run or a chain) and any number of `file` fields, in order.
async fn parse_multipart<T: serde::de::DeserializeOwned>(
    mut multipart: Multipart,
) -> Result<(T, Vec<(String, Vec<u8>)>), String> {
    let mut request: Option<T> = None;
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
/// would have printed for the same mistake, plus whether the request
/// asked for server-side delivery (the whitelist's `out_dir`/`out_file`
/// — the plan alone cannot say, its per-part placeholder is also a
/// directory destination).
fn prepare(
    request: RunRequest,
    files: Vec<(String, Vec<u8>)>,
) -> Result<(crate::plan::ExecutionPlan, bool), AppError> {
    let deliver = request.out_dir.is_some() || request.out_file.is_some();
    let (landed, paths) = invoke::land_uploads(files)?;
    let invocation = invoke::parse(&request, &paths)?;
    let plan = invoke::build_plan(&invocation)?;
    // The plan holds the gathered inputs; the landed copies can go.
    drop(landed);
    Ok((plan, deliver))
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
    let (plan, deliver) = match built {
        Ok(Ok(built)) => built,
        Ok(Err(e)) => return api_error(StatusCode::BAD_REQUEST, e.chain_inline()),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    };
    let run_id = if plan.record_history {
        history::new_run_id()
    } else {
        history::stamp_now()
    };
    runs::spawn(plan, state.runs.clone(), run_id.clone(), deliver);
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

// --- configuration ---------------------------------------------------------

/// The profile names a run can pick — the datalist behind the run form's
/// `--profile` and the chain builder's per-stage picker. The built-in
/// `default` profile exists only when the user defined none (the config's
/// own rule); the caller shows it accordingly.
async fn profiles_list() -> Response {
    let cfg = match crate::config::load() {
        Ok(cfg) => cfg,
        Err(e) => return io_error("cannot load the config", &AppError::usage(format!("{e:#}"))),
    };
    Json(serde_json::json!({
        "default_profile": cfg.default_profile,
        "profiles": cfg.profiles.iter().map(|(name, p)| serde_json::json!({
            "name": name,
            "provider": p.provider,
            "model": p.model,
            "is_default": cfg.default_profile.as_deref() == Some(name.as_str()),
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// The config as the editor sees it: the file's own bytes (the textarea's
/// truth), the parsed view of what runs actually resolve against, and
/// `check`'s issues. A config that cannot load (an AIDO_CONFIG typo, a
/// broken file) still answers 200 — the editor is exactly where that gets
/// fixed — with `load_error` carrying the message.
async fn config_show() -> Response {
    let path = crate::config::config_path();
    let exists = path.as_ref().is_some_and(|p| p.exists());
    let raw = match &path {
        Some(path) if exists => std::fs::read_to_string(path).unwrap_or_default(),
        _ => String::new(),
    };
    let (effective, load_error, issues) = match crate::config::load() {
        Ok(cfg) => (Some(config_dto(&cfg)), None, crate::config::check(&cfg)),
        Err(e) => (None, Some(format!("{e:#}")), Vec::new()),
    };
    Json(serde_json::json!({
        "path": path.as_ref().map(|p| p.display().to_string()),
        "exists": exists,
        "raw": raw,
        "effective": effective,
        "load_error": load_error,
        "issues": issues,
    }))
    .into_response()
}

/// The parsed view: names and shapes only — an `api_key_env` names an
/// environment variable, it never carries a value, so nothing here is a
/// secret.
fn config_dto(cfg: &crate::config::Config) -> serde_json::Value {
    serde_json::json!({
        "default_profile": cfg.default_profile,
        "profiles": cfg.profiles.iter().map(|(name, p)| serde_json::json!({
            "name": name,
            "provider": p.provider,
            "model": p.model,
            "operations": p
                .operations
                .as_ref()
                .map(|ops| ops.iter().map(|o| o.to_string()).collect::<Vec<_>>()),
            "input_types": p.input_types,
            "output_types": p.output_types,
            "is_default": cfg.default_profile.as_deref() == Some(name.as_str()),
        })).collect::<Vec<_>>(),
        "providers": cfg.providers.iter().map(|(name, p)| serde_json::json!({
            "name": name,
            "base_url": p.base_url,
            "api_key_env": p.api_key_env,
            "routes": p.routes.iter().map(|(op, adapter)| (
                op.clone(), adapter.to_string()
            )).collect::<std::collections::BTreeMap<String, String>>(),
        })).collect::<Vec<_>>(),
        "settings": {
            "stream": cfg.settings.stream,
            "timeout_secs": cfg.settings.timeout_secs,
            "total_timeout_secs": cfg.settings.total_timeout_secs,
            "hold_secs": cfg.settings.hold_secs,
            "history_keep": cfg.settings.history_keep,
            "history_bytes": cfg.settings.history_bytes,
            "input_bytes": cfg.settings.input_bytes,
            "watch_interval_ms": cfg.settings.watch_interval_ms,
            "watch_stable_ms": cfg.settings.watch_stable_ms,
        },
    })
}

#[derive(Deserialize)]
struct ConfigPut {
    toml: String,
}

/// Save the config file: parse first (the file on disk must never become
/// invalid), write atomically, then answer `check`'s verdict on exactly
/// what was saved. Issues do not block the save — a config may be
/// honestly incomplete — they are shown next to the editor's save button.
async fn config_save(Json(body): Json<ConfigPut>) -> Response {
    let parsed: crate::config::Config = match toml::from_str(&body.toml) {
        Ok(parsed) => parsed,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, format!("invalid TOML: {e}")),
    };
    let Some(path) = crate::config::config_path() else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot determine the config path on this platform",
        );
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot create {}: {e}", parent.display()),
            );
        }
    }
    // Atomic (tmp + rename, umask-respecting): a failed write leaves the
    // previous file exactly as it was.
    if let Err(e) = crate::output::write_file_atomic(
        body.toml.as_bytes(),
        &path,
        true,
        crate::output::FileMode::Default,
    ) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot write {}: {e:#}", path.display()),
        );
    }
    Json(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
        "issues": crate::config::check(&parsed),
    }))
    .into_response()
}

// --- chains ----------------------------------------------------------------

/// Start a chain run. The whole plan-time contract — stage parsing,
/// route resolution, the junction type checks — runs here, so a chain
/// that cannot work answers immediately with the CLI's own message and
/// zero requests.
async fn chain_create(State(state): State<Arc<UiState>>, multipart: Multipart) -> Response {
    let (request, files) = match parse_multipart::<super::invoke::ChainRequest>(multipart).await {
        Ok(parsed) => parsed,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, message),
    };
    let built = tokio::task::spawn_blocking(move || {
        let (landed, paths) = invoke::land_uploads(files)?;
        let prepared = invoke::parse_chain(&request, &paths)?;
        // The plan holds stage 1's gathered inputs; the landed copies go.
        drop(landed);
        Ok::<_, AppError>((prepared.chain, prepared.record_history))
    })
    .await;
    let (chain, record_history) = match built {
        Ok(Ok(built)) => built,
        Ok(Err(e)) => return api_error(StatusCode::BAD_REQUEST, e.chain_inline()),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    };
    let run_id = if record_history {
        history::new_run_id()
    } else {
        history::stamp_now()
    };
    runs::spawn_chain(chain, state.runs.clone(), run_id.clone(), record_history);
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "run_id": run_id })),
    )
        .into_response()
}

/// The chain's dry-run: `describe_chain`'s per-stage blocks, rendered
/// against the same non-tty, clipboard-less environment a UI run would
/// execute in — zero requests.
async fn chain_preview(State(_state): State<Arc<UiState>>, multipart: Multipart) -> Response {
    let (request, files) = match parse_multipart::<super::invoke::ChainRequest>(multipart).await {
        Ok(parsed) => parsed,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, message),
    };
    let built = tokio::task::spawn_blocking(move || {
        let (landed, paths) = invoke::land_uploads(files)?;
        let prepared = invoke::parse_chain(&request, &paths)?;
        let cfg = crate::config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
        let terminal = crate::plan::TerminalInfo {
            stdin: false,
            stdout: false,
            stderr: false,
        };
        let mut empty = std::io::empty();
        let mut probe = || false;
        let mut no_clipboard = || {
            Err(anyhow::anyhow!(
                "the clipboard is not available to the web UI; paste it as a file"
            ))
        };
        let mut env = crate::input::InputEnv::custom(&mut empty, &mut probe, &mut no_clipboard);
        let text = crate::chain::describe_chain(&prepared.chain, &cfg, terminal, &mut env)?;
        drop(landed);
        Ok::<_, AppError>((prepared.chain, text))
    })
    .await;
    let (chain, text) = match built {
        Ok(Ok(built)) => built,
        Ok(Err(e)) => return api_error(StatusCode::BAD_REQUEST, e.chain_inline()),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    };
    Json(serde_json::json!({
        "text": text,
        "label": chain.task_label(),
        "stages": chain.stages.iter().map(|stage| serde_json::json!({
            "name": stage.task.name,
            "profile": stage.resolved.profile_name,
            "model": stage.resolved.model,
            "produce": stage.resolved.produce,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

// --- watch daemons ---------------------------------------------------------

/// The dashboard's list: every watch this server started, running and
/// stopped (stopping is between files; the current file finishes).
async fn watches_list(State(state): State<Arc<UiState>>) -> Response {
    Json(serde_json::json!({ "watches": state.watches.list() })).into_response()
}

/// Start a watch daemon. The full CLI precheck — task resolution, the
/// probe plan, delivery sanity, credentials — runs here in the
/// response, so a watch that cannot work never starts.
async fn watches_create(
    State(state): State<Arc<UiState>>,
    Json(request): Json<watches::WatchRequest>,
) -> Response {
    let built = tokio::task::spawn_blocking(move || Watches::prepare(&request)).await;
    let prepared = match built {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(e)) => return api_error(StatusCode::BAD_REQUEST, e.chain_inline()),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    };
    let id = state.watches.start(prepared);
    (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response()
}

/// The watch's dry-run: the probe plan every arriving file would run,
/// zero requests.
async fn watches_preview(Json(request): Json<watches::WatchRequest>) -> Response {
    let built = tokio::task::spawn_blocking(move || Watches::prepare(&request)).await;
    let prepared = match built {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(e)) => return api_error(StatusCode::BAD_REQUEST, e.chain_inline()),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    };
    Json(Watches::preview(&prepared)).into_response()
}

/// One daemon's live activity, as SSE; the stream ends with its
/// `stopped` frame. A late subscriber sees nothing before it joined —
/// the list's counters are the catch-up.
async fn watch_events(State(state): State<Arc<UiState>>, Path(id): Path<String>) -> Response {
    let Some(receiver) = state.watches.subscribe(&id) else {
        return api_error(StatusCode::NOT_FOUND, format!("no live watch '{id}'"));
    };
    let stream = tokio_stream::wrappers::BroadcastStream::new(receiver).map(|item| {
        let event = match item {
            Ok(event) => event,
            Err(_) => watches::lagged_frame(),
        };
        let data = serde_json::to_string(&event)
            .unwrap_or_else(|_| r#"{"type":"dir_unreadable","dir":"…"}"#.into());
        Ok::<_, std::convert::Infallible>(Event::default().data(data))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// Ask a daemon to stop (between files; the current one finishes).
async fn watch_stop(State(state): State<Arc<UiState>>, Path(id): Path<String>) -> Response {
    if state.watches.stop(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_error(StatusCode::NOT_FOUND, format!("no watch '{id}' to stop"))
    }
}
