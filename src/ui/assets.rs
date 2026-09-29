//! Embedded SPA assets: `ui/dist` inlined at compile time in release
//! builds, read from disk in debug ones (a cargo-only checkout carries a
//! placeholder `index.html` and still builds — CI builds the real assets
//! before release builds).

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui/dist/"]
struct Dist;

/// Serve one asset. Unknown non-API paths fall back to the SPA entry:
/// `/history` and friends are client-side routes, not files.
pub fn serve(path: &str) -> Response {
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(file) = Dist::get(path) {
        return ([(header::CONTENT_TYPE, mime_for(path))], file.data).into_response();
    }
    match Dist::get("index.html") {
        Some(index) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            index.data,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "aido ui: no embedded frontend — build it with `cd ui && npm install && npm run build`",
        )
            .into_response(),
    }
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}
