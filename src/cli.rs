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
    after_help = "Input priority: files > piped stdin > clipboard (text, or images for vision models).\n\nExamples:\n  aido ocr --copy                # preset action: name first\n  aido ocr screenshot.png        # file input: text or image files\n  git diff | aido code-review\n  aido -p \"clean up this text\" --copy\n  aido -p \"write a haiku\" --stream  # stream the reply as it generates\n  aido list                      # show available actions\n  aido last                      # re-print the most recent result\n  echo hi | aido --base-url http://localhost:30000 -m qwen3\n\nActions are presets: aido ocr ... == aido --preset ocr ..."
)]
pub struct Cli {
    /// System instructions for the model (the input text is sent as the user message)
    #[arg(short = 'p', long, conflicts_with = "preset")]
    pub prompt: Option<String>,

    /// Input file(s), paired with an action or -p/--prompt; text is sent
    /// as-is, PNG/JPEG images go to vision models
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

    /// Also write the result to this file (composes with any --output mode)
    #[arg(long, value_name = "FILE")]
    pub save: Option<std::path::PathBuf>,

    /// Max completion tokens (default 8192; 0 omits the field entirely)
    #[arg(long)]
    pub max_tokens: Option<u64>,

    /// Sampling temperature
    #[arg(long)]
    pub temperature: Option<f32>,

    /// Request timeout in seconds (default 120); with --stream this bounds
    /// the wait for the response headers and each gap between bytes
    #[arg(long)]
    pub timeout: Option<u64>,

    /// Disable the progress spinner
    #[arg(long)]
    pub no_spinner: bool,

    /// Stream the reply to stdout as it is generated (SSE); has no effect
    /// when the result goes only to the clipboard
    #[arg(long, overrides_with = "no_stream")]
    pub stream: bool,

    /// Send one buffered request instead of streaming (overrides
    /// --stream / settings.stream)
    #[arg(long, overrides_with = "stream")]
    pub no_stream: bool,

    /// Send tall images whole instead of slicing them into
    /// legibility-sized requests
    #[arg(long)]
    pub no_split: bool,

    /// Strip markdown decoration (bold, headings, code fences, list
    /// markers, links, tables) so popup-style surfaces get clean text;
    /// code content is kept verbatim
    #[arg(long, overrides_with = "no_plain")]
    pub plain: bool,

    /// Keep the reply as-is (overrides --plain / preset plain /
    /// settings.plain)
    #[arg(long, overrides_with = "plain")]
    pub no_plain: bool,

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

    /// Print the most recent saved result (see the history section)
    Last {
        /// Copy the result back to the clipboard instead of printing it
        #[arg(short = 'c', long)]
        copy: bool,
    },

    /// Internal: hold clipboard contents in the background (Linux)
    #[command(name = "__hold", hide = true)]
    Hold {
        /// Seconds to keep the clipboard alive
        secs: u64,
    },
}
