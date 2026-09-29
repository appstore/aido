//! The local web UI: `aido ui` serves an embedded single-page app on
//! 127.0.0.1 and exposes the run engine over a small JSON API.
//!
//! The server reuses the library's own path — normalize → plan → runner
//! → history — with a non-tty [`crate::plan::TerminalInfo`] injected, so
//! a UI run and a CLI run obey exactly the same rules and land in the
//! same history. Only loopback is served; [`guard`] is the request gate.

mod api;
mod assets;
mod guard;
mod invoke;
mod runs;

#[cfg(test)]
mod tests;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use crate::domain::{AppError, AppResult};

/// Bind loopback, print the URL (token included) and serve until the
/// caller stops asking — Ctrl+C walks the same signal select every
/// other aido command does, dropping this future and exiting 130.
pub async fn serve(port: Option<u16>, no_open: bool) -> AppResult<()> {
    let token = token();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port.unwrap_or(0)));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| AppError::usage(format!("cannot bind {addr}: {e}")))?;
    // Port 0 asked for a free one; the OS answer is what the URL and the
    // Host checks must both use.
    let port = listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| AppError::usage(format!("cannot tell the bound port: {e}")))?;
    let url = format!("http://127.0.0.1:{port}/?t={}", encode_token(&token));
    let state = Arc::new(api::UiState {
        guard: Arc::new(guard::Guard { token, port }),
        runs: runs::Runs::shared(),
    });
    let app = api::router(state);
    println!("aido ui listening on {url}");
    if !no_open {
        open_browser(&url);
    }
    axum::serve(listener, app)
        .await
        .map_err(|e| AppError::service(format!("the web UI server failed: {e}")))
}

/// The session token every `/api` call must carry. `AIDO_UI_TOKEN` pins
/// a fixed one (tests and frontend development); otherwise two fresh
/// `RandomState` seeds hash a run stamp and the pid into a secret an
/// other local process cannot guess — sixteen random hex digits without
/// a new dependency.
fn token() -> String {
    if let Ok(fixed) = std::env::var("AIDO_UI_TOKEN") {
        let fixed = fixed.trim().to_string();
        if !fixed.is_empty() {
            return fixed;
        }
    }
    use std::hash::{BuildHasher, Hasher};
    let mut first = std::collections::hash_map::RandomState::new().build_hasher();
    first.write(crate::history::stamp_now().as_bytes());
    first.write_u64(u64::from(std::process::id()));
    let mut second = std::collections::hash_map::RandomState::new().build_hasher();
    second.write_u64(first.finish());
    format!("{:016x}", second.finish())
}

/// Percent-encode the token for the URL: a pinned AIDO_UI_TOKEN may
/// carry `+`/`&`/`#`/`%`, which would silently mangle the printed link
/// (the guard and the SPA both decode `+` back to a space).
fn encode_token(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    for byte in token.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Open the default browser with the platform opener — no dependency
/// for one spawn. A failed open is a non-event: the URL is printed and
/// stays valid.
fn open_browser(url: &str) {
    let (program, args): (&str, Vec<String>) = if cfg!(target_os = "macos") {
        ("open", vec![url.to_string()])
    } else if cfg!(target_os = "windows") {
        // `start` reads a quoted first argument as a window title; an
        // empty one takes that role so the URL stays the target.
        (
            "cmd",
            vec!["/c".into(), "start".into(), String::new(), url.to_string()],
        )
    } else {
        ("xdg-open", vec![url.to_string()])
    };
    let _ = std::process::Command::new(program).args(&args).spawn();
}
