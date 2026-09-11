//! Domain types shared by CLI parsing, execution and delivery.
//!
//! Nothing here may depend on clap, HTTP, the clipboard or the terminal:
//! adapters and output depend on these types, never the other way around.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The three content kinds aido understands on either side of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Text,
    Image,
    Audio,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Audio => "audio",
        }
    }
}

impl std::fmt::Display for MediaKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// Where one piece of material came from — kept so runs and `--dry-run`
/// reports can explain the input without guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputSource {
    File(PathBuf),
    Stdin,
    Clipboard,
    /// A literal `--text` argument.
    Literal,
}

impl std::fmt::Display for InputSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(p) => write!(f, "file {}", p.display()),
            Self::Stdin => f.write_str("stdin"),
            Self::Clipboard => f.write_str("clipboard"),
            Self::Literal => f.write_str("--text"),
        }
    }
}

/// One piece of ordered material. Original bytes and MIME survive intact;
/// image re-encoding happens only where an adapter or processor needs it.
#[derive(Debug, Clone)]
pub struct InputPart {
    pub id: usize,
    pub source: InputSource,
    /// File name when the source has one (files keep theirs, stdin and
    /// clipboard get a synthesized one), used for labels and history.
    pub name: String,
    pub kind: MediaKind,
    /// Set only by the `--dry-run` clipboard placeholder: `kind` above is
    /// a stand-in (text), not the real kind, which the actual run learns
    /// when it reads the clipboard. Validation treats such a part as
    /// acceptable to any task instead of judging the stand-in kind.
    pub unknown_kind: bool,
    pub mime: String,
    pub content: InputContent,
}

impl InputPart {
    pub fn text(&self) -> Option<&str> {
        match &self.content {
            InputContent::Text(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputContent {
    Text(String),
    /// Image or audio bytes, exactly as they were read.
    Media(Vec<u8>),
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

/// Which request produced an artifact, so `--dry-run` plans and run records
/// can explain provenance. One run may issue several requests (OCR slices,
/// text chunks): a merged artifact names every request whose reply it joins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Provenance {
    /// Produced by request `index` (0-based) of the run.
    Request { index: usize },
    /// Joined from the replies of several requests of one run, named in
    /// reply order (an ocr run's slices, a chunk-join run's chunks).
    Merged { requests: Vec<usize> },
    /// Restored from history (`aido last`, `history show`); not produced
    /// in this process. Set only when artifacts are loaded back — history
    /// manifests store no provenance, so this is never written to disk.
    Restored,
}

/// A delivered result of a run. Text is an artifact like any other kind —
/// there is no separate "text field plus binary artifacts" path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub kind: MediaKind,
    pub mime: String,
    /// Codec/container for media ("png", "mp3"), "text" for text.
    pub format: String,
    #[serde(with = "encoded_bytes")]
    pub bytes: Vec<u8>,
    pub provenance: Provenance,
}

impl Artifact {
    pub fn text(&self) -> Option<&str> {
        if self.kind == MediaKind::Text {
            std::str::from_utf8(&self.bytes).ok()
        } else {
            None
        }
    }
}

mod encoded_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Run state and events
// ---------------------------------------------------------------------------

/// Outcome of the generation half of a run, independent of whether the
/// results could be delivered anywhere.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GenerationStatus {
    #[default]
    Running,
    Complete,
    Incomplete {
        reason: String,
    },
    Failed,
    Cancelled,
}

impl GenerationStatus {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// One delivery target of a run and how it ended. A generation can succeed
/// while a delivery fails — the user must be able to recover the result
/// without asking the model again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending,
    Succeeded,
    Failed { error: String },
}

impl DeliveryStatus {
    pub fn is_succeeded(&self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// What a run was asked to write and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Destination {
    Stdout,
    File { path: PathBuf },
    Directory { path: PathBuf },
    Clipboard,
}

impl std::fmt::Display for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdout => f.write_str("stdout"),
            Self::File { path } => write!(f, "file {}", path.display()),
            Self::Directory { path } => write!(f, "directory {}", path.display()),
            Self::Clipboard => f.write_str("clipboard"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryState {
    pub destination: Destination,
    pub status: DeliveryStatus,
}

/// Everything known about one run, in the shape history stores it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub task: Option<String>,
    pub created_at: String,
    /// Sanitized description of the generation setup; never credentials.
    pub summary: RunSummary,
    pub generation: GenerationStatus,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub deliveries: Vec<DeliveryState>,
}

/// The non-sensitive parts of the execution plan a run records.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunSummary {
    pub task: Option<String>,
    pub profile: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub adapter: Option<String>,
    #[serde(default)]
    pub inputs: Vec<InputSummary>,
    pub processor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputSummary {
    pub name: String,
    pub kind: MediaKind,
    pub source: String,
    pub bytes: u64,
}

/// Events emitted while a generation runs. The terminal event fires once;
/// consumers (spinner, live stdout, history) subscribe instead of polling.
#[derive(Debug, Clone)]
pub enum GenerationEvent {
    /// A chunk of reply text arrived.
    TextDelta {
        text: String,
    },
    /// A media artifact finished downloading/validating.
    ArtifactComplete {
        artifact: Artifact,
    },
    /// Token usage, when the protocol reports it; unknown otherwise.
    Usage {
        input: Option<u64>,
        output: Option<u64>,
    },
    Warning {
        message: String,
    },
    /// Terminal state for the whole run. Emitted at most once.
    Finished {
        status: GenerationStatus,
    },
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The failure classes a run can end in; each maps to a distinct exit
/// code so scripts can tell "you made a mistake" from "the service broke"
/// from "the result was bad" from "the result is fine but could not be
/// delivered".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// CLI, config, input, capability or output-target preflight error. Exit 2.
    Usage,
    /// Service, network, timeout or response-protocol error. Exit 3.
    Service,
    /// Generation incomplete or artifacts don't satisfy the request. Exit 4.
    Generation,
    /// An explicit output target failed after a successful generation. Exit 5.
    Delivery,
    /// A per-part batch finished with every successful part delivered but
    /// at least one part failed; the failures are listed in the message
    /// and the run's warnings. Exit 6.
    Partial,
}

impl ErrorKind {
    /// The name this kind carries in the JSON run report's `error.kind`,
    /// shared by the success report and the error report.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Service => "service",
            Self::Generation => "generation",
            Self::Delivery => "delivery",
            Self::Partial => "partial",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Self::Usage => 2,
            Self::Service => 3,
            Self::Generation => 4,
            Self::Delivery => 5,
            Self::Partial => 6,
        }
    }
}

/// An error with its classification attached, carrying the underlying cause
/// chain for display.
#[derive(Debug)]
pub struct AppError {
    pub kind: ErrorKind,
    pub message: String,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl AppError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    pub fn service(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Service, message)
    }

    pub fn generation(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Generation, message)
    }

    pub fn delivery(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Delivery, message)
    }

    pub fn partial(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Partial, message)
    }

    /// The full message plus every underlying cause, one per line.
    pub fn chain(&self) -> String {
        let mut out = self.message.clone();
        if let Some(err) = self.source.as_deref() {
            out.push_str(": ");
            out.push_str(&err.to_string());
        }
        out
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.chain())
    }
}

impl std::error::Error for AppError {}

impl From<anyhow::Error> for AppError {
    /// Un-classified errors default to Service: they come from adapters and
    /// transport, the bulk of the dynamic failure surface.
    fn from(err: anyhow::Error) -> Self {
        Self {
            kind: ErrorKind::Service,
            message: format!("{err:#}"),
            source: None,
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        Self {
            kind: ErrorKind::Service,
            message: err.to_string(),
            source: Some(Box::new(err)),
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn text_part(id: usize, s: &str) -> InputPart {
        InputPart {
            id,
            source: InputSource::Literal,
            name: format!("part-{id}"),
            kind: MediaKind::Text,
            unknown_kind: false,
            mime: "text/plain".into(),
            content: InputContent::Text(s.into()),
        }
    }

    fn image_part(id: usize) -> InputPart {
        InputPart {
            id,
            source: InputSource::File("a.png".into()),
            name: "a.png".into(),
            kind: MediaKind::Image,
            unknown_kind: false,
            mime: "image/png".into(),
            content: InputContent::Media(vec![1, 2, 3]),
        }
    }

    #[test]
    fn ordered_mixed_input_keeps_position() {
        // text before image before text: order must survive, not be grouped.
        let parts = [text_part(0, "first"), image_part(1), text_part(2, "third")];
        assert_eq!(parts[0].kind, MediaKind::Text);
        assert_eq!(parts[1].kind, MediaKind::Image);
        assert_eq!(parts[2].text(), Some("third"));
    }

    #[test]
    fn run_record_represents_generated_but_undelivered() {
        // generation complete, clipboard delivery failed: recoverable.
        let record = RunRecord {
            run_id: "r1".into(),
            task: Some("ocr".into()),
            created_at: "2026-09-10T00:00:00Z".into(),
            summary: RunSummary {
                task: Some("ocr".into()),
                profile: None,
                provider: None,
                model: None,
                adapter: None,
                inputs: Vec::new(),
                processor: None,
            },
            generation: GenerationStatus::Complete,
            artifacts: vec![Artifact {
                id: "a0".into(),
                kind: MediaKind::Text,
                mime: "text/plain".into(),
                format: "text".into(),
                bytes: b"hello".to_vec(),
                provenance: Provenance::Request { index: 0 },
            }],
            warnings: Vec::new(),
            deliveries: vec![DeliveryState {
                destination: Destination::Clipboard,
                status: DeliveryStatus::Failed {
                    error: "no display".into(),
                },
            }],
        };
        assert!(record.generation.is_complete());
        assert!(!record.deliveries[0].status.is_succeeded());
        assert_eq!(record.artifacts[0].text(), Some("hello"));
    }

    #[test]
    fn app_error_chain_prints_causes() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err = AppError::from(io_err);
        assert!(err.chain().contains("gone"));
        assert_eq!(err.kind.exit_code(), 3);
    }

    #[test]
    fn provenance_keeps_the_tagged_form_old_records_carry() {
        // The shape the --out-dir delivery manifests carry. Old records
        // hold "request" and "restored"; "merged" only joins them, so
        // every historical form still parses.
        assert_eq!(
            serde_json::to_value(Provenance::Request { index: 0 }).unwrap(),
            serde_json::json!({"type": "request", "index": 0})
        );
        for (raw, parsed) in [
            (
                r#"{"type":"request","index":3}"#,
                Provenance::Request { index: 3 },
            ),
            (r#"{"type":"restored"}"#, Provenance::Restored),
            (
                r#"{"type":"merged","requests":[0,1,2]}"#,
                Provenance::Merged {
                    requests: vec![0, 1, 2],
                },
            ),
        ] {
            assert_eq!(serde_json::from_str::<Provenance>(raw).unwrap(), parsed);
        }
    }

    #[test]
    fn exit_codes_are_distinct() {
        let codes = [
            ErrorKind::Usage.exit_code(),
            ErrorKind::Service.exit_code(),
            ErrorKind::Generation.exit_code(),
            ErrorKind::Delivery.exit_code(),
            ErrorKind::Partial.exit_code(),
        ];
        assert_eq!(codes, [2, 3, 4, 5, 6]);
    }
}
