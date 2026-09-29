//! The JSON API: thin handlers over the library's existing surfaces.
//! Run-shaped responses reuse the `--json` envelope's field names, so
//! the UI and scripts read one schema.

use std::sync::Arc;

use axum::extract::State;
use axum::http::Uri;
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;

use super::assets;
use super::guard::{self, Guard};

pub fn router(guard: Arc<Guard>) -> Router {
    Router::new()
        .route("/api/health", get(health))
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
