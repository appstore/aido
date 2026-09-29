//! The request gate: only this process's own origin may talk to the
//! server.
//!
//! A localhost server is reachable from two directions that must both
//! close. Other pages in the user's browser — their requests carry an
//! `Origin` header ours never has. And DNS rebinding — a foreign name
//! resolving to 127.0.0.1; the `Host` header names the site the browser
//! believes it is visiting. The token covers everything else: a local
//! process that cannot read the URL (printed once, to the user's
//! terminal) does not get in.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::api::UiState;

/// What every request is checked against: the session token and the
/// port the server actually bound (the URL was built from it, so the
/// Host check uses the same truth).
pub struct Guard {
    pub token: String,
    pub port: u16,
}

pub async fn check(State(state): State<Arc<UiState>>, req: Request, next: Next) -> Response {
    let guard = &state.guard;
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.to_ascii_lowercase());
    if !host.is_some_and(|h| {
        h == format!("127.0.0.1:{}", guard.port) || h == format!("localhost:{}", guard.port)
    }) {
        return refuse(StatusCode::FORBIDDEN, "unexpected Host");
    }
    if let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .map(|o| o.to_ascii_lowercase())
    {
        let own = [
            format!("http://127.0.0.1:{}", guard.port),
            format!("http://localhost:{}", guard.port),
        ];
        if !own.contains(&origin) {
            return refuse(StatusCode::FORBIDDEN, "cross-site Origin");
        }
    }
    if req.uri().path().starts_with("/api/") {
        let query = req.uri().query().and_then(query_token);
        let sent = req
            .headers()
            .get("x-aido-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or(query);
        if sent.as_deref() != Some(guard.token.as_str()) {
            return refuse(StatusCode::UNAUTHORIZED, "missing or wrong token");
        }
    }
    next.run(req).await
}

/// The `t` query parameter, percent-decoded: the SPA sends the token
/// URL-encoded, and a fixed `AIDO_UI_TOKEN` may contain reserved
/// characters. `+` decodes as a space, the form-encoding convention.
pub(super) fn query_token(query: &str) -> Option<String> {
    let raw = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "t").then_some(value)
    })?;
    let hex = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let high = hex(*bytes.get(i + 1)?)?;
                let low = hex(*bytes.get(i + 2)?)?;
                out.push((high << 4) | low);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn refuse(status: StatusCode, reason: &'static str) -> Response {
    (status, reason).into_response()
}
