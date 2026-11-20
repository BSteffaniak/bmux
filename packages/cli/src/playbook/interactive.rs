//! Interactive execution uses the selected CLI host adapter.
use anyhow::Result;
use std::time::Duration;

/// Run the socket-controlled playbook sandbox.
/// # Errors
/// Returns sandbox startup, protocol or cleanup errors.
pub async fn run_interactive(
    socket_override: Option<&str>,
    record: bool,
    viewport_cols: u16,
    viewport_rows: u16,
    shell: Option<&str>,
    session_timeout: Option<Duration>,
) -> Result<u8> {
    bmux_playbook::interactive::run_interactive(
        super::engine::host(),
        socket_override,
        record,
        viewport_cols,
        viewport_rows,
        shell,
        session_timeout,
    )
    .await
}
