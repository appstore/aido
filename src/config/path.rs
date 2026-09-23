//! Config-path handling: the one place that turns configured path strings
//! into filesystem paths. Servers and adapters receive resolved paths and
//! never interpret config syntax themselves — the next local-model
//! surface (tts, embedding, ocr) reuses this instead of growing its own
//! expansion.

#[cfg(feature = "local-asr")]
use std::path::PathBuf;

/// Expand a leading `~/` to the home directory. Other forms (absolute,
/// relative, bare `~`) pass through untouched: the tilde form is the only
/// home-relative spelling this config uses. The ASR server's model-field
/// resolution is the only consumer today; later local-model surfaces
/// (tts, embedding, ocr) reuse this instead of growing their own
/// expansion.
#[cfg(feature = "local-asr")]
pub(crate) fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(all(test, feature = "local-asr"))]
mod tests;
