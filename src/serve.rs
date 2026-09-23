//! The `aido serve` command: long-running servers over aido's engines.
//!
//! `serve` is a verb like `run`/`ask`/`watch` — it starts a process, it
//! does not manage server objects. The subcommand names what is served
//! (`aido serve asr`); each server lives in its own module behind its own
//! feature.

use crate::cli::{AsrServeArgs, ServeCmd};
#[cfg(not(feature = "local-asr"))]
use crate::domain::AppError;
use crate::domain::AppResult;

#[cfg(feature = "asr-server")]
pub mod asr;

/// Dispatch `aido serve <sub>`. The subcommands always parse (so --help
/// works in every build); a binary without the backing feature refuses
/// here, worded like the other not-compiled refusals.
pub async fn run(cmd: &ServeCmd) -> AppResult<()> {
    match cmd {
        ServeCmd::Asr(args) => serve_asr(args).await,
    }
}

async fn serve_asr(args: &AsrServeArgs) -> AppResult<()> {
    #[cfg(feature = "local-asr")]
    {
        return asr::run(args).await;
    }
    #[cfg(not(feature = "local-asr"))]
    {
        let _ = args;
        #[cfg(feature = "asr-server")]
        return Err(AppError::usage(
            "the local ASR engine is not compiled into this binary (rebuild \
             with --features local-asr)",
        ));
        #[cfg(not(feature = "asr-server"))]
        return Err(AppError::usage(
            "the ASR server is not compiled into this binary (rebuild with \
             --features asr-server, or --features local-asr for the engine)",
        ));
    }
}
