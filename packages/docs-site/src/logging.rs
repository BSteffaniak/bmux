//! Process-global, synchronous stderr logging for the documentation server.

use std::io::IsTerminal;

use tracing_subscriber::{EnvFilter, filter::LevelFilter, util::SubscriberInitExt};

pub(crate) fn init() -> Result<(), tracing_subscriber::util::TryInitError> {
    // The cached moosicbox_log_runtime initializer always ensures runtime
    // directories, even without a file sink. This server needs no filesystem
    // logging state, so use the existing tracing infrastructure directly.
    let filter = EnvFilter::builder()
        .with_env_var("RUST_LOG")
        .with_default_directive(LevelFilter::ERROR.into())
        .from_env_lossy();

    // The global subscriber owns the synchronous writer for process lifetime;
    // there is no background worker or guard that must be retained. The
    // tracing-log feature makes try_init install the log-record bridge too.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .finish()
        .try_init()
}
