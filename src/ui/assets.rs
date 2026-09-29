//! Embedded SPA assets: `ui/dist` inlined at compile time in release
//! builds, read from disk in debug ones (a cargo-only checkout carries a
//! placeholder `index.html` and still builds — CI builds the real assets
//! before release builds).

use axum::http::header;
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui/dist/"]
struct Dist;

/// Serve one asset. Unknown non-API paths fall back to the SPA entry:
/// `/history` and friends are client-side routes, not files. A checkout
/// that never built the frontend (cargo-only; the embedded folder holds
/// only the placeholder) still answers 200 with a page that says how to
/// build it — the SPA entry exists, it just is this explanation.
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
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            UNBUILT_PAGE,
        )
            .into_response(),
    }
}

/// The page a cargo-only checkout serves: no embedded assets, one clear
/// instruction.
const UNBUILT_PAGE: &str = r#"<!doctype html>
<html lang="zh-CN">
  <head>
    <meta charset="utf-8" />
    <title>aido</title>
  </head>
  <body style="margin:0; min-height:100vh; display:grid; place-items:center; background:#15171c; color:#c9d1e0; font-family:system-ui, sans-serif;">
    <main style="max-width:34rem; padding:2rem;">
      <h1 style="margin:0 0 .5rem;">aido</h1>
      <p style="margin:.5rem 0;">此构建没有内嵌前端资源。</p>
      <p style="margin:.5rem 0;">开发调试（debug 构建直接读取 ui/dist，改前端不用重编 Rust）：</p>
      <pre style="background:#0c0e12; padding:.75rem 1rem; border-radius:8px; overflow-x:auto;">cd ui &amp;&amp; npm install &amp;&amp; npm run build</pre>
      <p style="margin:.5rem 0;">发布构建需要先跑上面的构建再 <code>cargo build --release</code>（CI 已自动完成）；CLI 用法不受影响。</p>
    </main>
  </body>
</html>
"#;

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
