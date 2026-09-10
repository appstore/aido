//! aido: send material to an AI task, deliver the result.
//!
//! The crate layout follows one dependency direction — CLI parsing,
//! adapters and delivery depend on [`domain`] types; the domain depends on
//! nothing. One run flows through: [`cli`] normalization → [`plan`]
//! building (precheck, `--dry-run`) → [`runner`] execution (processors,
//! adapters) → [`output`] delivery → [`history`] recording.

pub mod api;
pub mod app;
pub mod cli;
pub mod clipboard;
pub mod config;
pub mod domain;
pub mod history;
pub mod input;
pub mod output;
pub mod plan;
pub mod processors;
pub mod runner;
pub mod spinner;
pub mod tasks;
