//! Playbook system for headless scripted bmux execution.
//!
//! Supports two input formats:
//! - TOML playbook files (`.playbook.toml`)
//! - Line-oriented DSL (pipeable from stdin)
//!
//! Both formats parse into the same internal representation and are executed
//! by the same engine against an ephemeral sandbox server (or a live server).

pub use bmux_playbook::{diff, parse_dsl, parse_toml, subst, types};
pub mod display_track;
pub mod engine;
pub mod interactive;
pub mod sandbox;
pub mod screen;

use anyhow::Result;

use self::engine::run_playbook;
use self::types::{Playbook, PlaybookResult};

/// Execution options for playbook runs.
pub use bmux_playbook::RunOptions;

pub use bmux_playbook::parser::{parse_file, parse_stdin};

/// Run a playbook and return the result.
/// # Errors
///
/// Returns an error if the playbook execution fails.
pub async fn run(playbook: Playbook, target_server: bool) -> Result<PlaybookResult> {
    run_with_options(playbook, target_server, RunOptions::default()).await
}

/// Run a playbook with explicit execution options.
/// # Errors
///
/// Returns an error if the playbook execution fails.
pub async fn run_with_options(
    playbook: Playbook,
    target_server: bool,
    options: RunOptions,
) -> Result<PlaybookResult> {
    run_playbook(playbook, target_server, options).await
}

pub use bmux_playbook::validation::validate;

pub use bmux_playbook::format_result;
