use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    Stdout,
    Clipboard,
    Both,
}

impl std::fmt::Display for OutputMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OutputMode::Stdout => "stdout",
            OutputMode::Clipboard => "clipboard",
            OutputMode::Both => "both",
        })
    }
}

/// Send clipboard, piped stdin, or file content to an OpenAI-compatible model.
#[derive(Debug, Parser)]
#[command(
    name = "aido",
    version,
    about = "Send clipboard, piped stdin, or file content to an OpenAI-compatible model",
    after_help = "Input priority: files > piped stdin > clipboard (text, or images for vision models).\n\nExamples:\n  aido ocr --copy                # preset action: name first\n  aido ocr screenshot.png        # file input: text or image files\n  git diff | aido code-review\n  aido -p \"clean up this text\" --copy\n  aido list                      # show available actions\n  echo hi | aido --base-url http://localhost:30000 -m qwen3\n\nActions are presets: aido ocr ... == aido --preset ocr ..."
)]
pub struct Cli {
    /// System instructions for the model (the input text is sent as the user message)
    #[arg(short = 'p', long, conflicts_with = "preset")]
    pub prompt: Option<String>,

    /// Input file(s): text is read as-is; PNG/JPEG images are sent to vision models
    #[arg(value_name = "FILE")]
    pub files: Vec<std::path::PathBuf>,

    /// Use a prompt preset (see `aido list`); shorthand: aido NAME
    #[arg(long)]
    pub preset: Option<String>,

    /// Config profile to use (see --init for a sample config)
    #[arg(long)]
    pub profile: Option<String>,

    /// Model name
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// OpenAI-compatible base URL, e.g. https://api.openai.com/v1 or http://localhost:30000
    #[arg(long)]
    pub base_url: Option<String>,

    /// API key (prefer the AIDO_API_KEY / OPENAI_API_KEY env vars)
    #[arg(long)]
    pub api_key: Option<String>,

    /// Where to write the result
    #[arg(short = 'o', long, value_enum, conflicts_with = "copy")]
    pub output: Option<OutputMode>,

    /// Shorthand for --output clipboard
    #[arg(short = 'c', long)]
    pub copy: bool,

    /// Max completion tokens (default 4096; 0 omits the field entirely)
    #[arg(long)]
    pub max_tokens: Option<u64>,

    /// Sampling temperature
    #[arg(long)]
    pub temperature: Option<f32>,

    /// Request timeout in seconds (default 120)
    #[arg(long)]
    pub timeout: Option<u64>,

    /// Disable the progress spinner
    #[arg(long)]
    pub no_spinner: bool,

    /// List available presets and exit (same as `aido list`)
    #[arg(long)]
    pub list_presets: bool,

    /// Write a sample config file and exit
    #[arg(long)]
    pub init: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// List available presets
    List,

    /// Internal: hold clipboard contents in the background (Linux)
    #[command(name = "__hold", hide = true)]
    Hold {
        /// Seconds to keep the clipboard alive
        secs: u64,
    },
}
