use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;

use anyhow::Result;
use bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry;
use tokio::signal::unix::{Signal, SignalKind, signal};

use super::StopEvent;

pub(crate) struct StopEvents {
    interrupt: Signal,
    terminate: Signal,
    suspend: Signal,
    hangup: Signal,
}

impl StopEvents {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            suspend: signal(SignalKind::from_raw(libc::SIGTSTP))?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    pub(crate) async fn recv(&mut self) -> anyhow::Error {
        tokio::select! {
            _ = self.interrupt.recv() => StopEvent::Interrupted.into_error(),
            _ = self.terminate.recv() => StopEvent::Terminated.into_error(),
            _ = self.suspend.recv() => suspension_error(),
            _ = self.hangup.recv() => StopEvent::Disconnected.into_error(),
        }
    }
}

fn suspension_error() -> anyhow::Error {
    anyhow::anyhow!("outer terminal suspension unsupported; execution retained; input suspended, no automatic resume")
        .context(crate::interactive_terminal::InteractiveFailureKind::Unsupported)
}

pub(crate) fn executable(path: &std::path::Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn environment_entry(key: std::ffi::OsString, value: std::ffi::OsString) -> EnvironmentEntry {
    EnvironmentEntry {
        key: key.into_vec(),
        value: value.into_vec(),
        encoding: Some("unix-bytes".into()),
    }
}

pub(crate) fn caller_environment() -> Vec<EnvironmentEntry> {
    let mut entries: Vec<_> = std::env::vars_os()
        .map(|(key, value)| environment_entry(key, value))
        .collect();
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn suspension_retains_execution_without_automatic_resume() {
        let error = suspension_error();
        assert!(format!("{error:#}").contains("execution retained"));
        assert_eq!(
            crate::interactive_terminal::failure_kind(&error),
            crate::interactive_terminal::InteractiveFailureKind::Unsupported,
        );
    }

    #[test]
    fn environment_preserves_non_utf8_native_bytes() {
        let entry = environment_entry(
            OsString::from_vec(b"KEY\xff".to_vec()),
            OsString::from_vec(b"VALUE\xfe".to_vec()),
        );
        assert_eq!(entry.key, b"KEY\xff");
        assert_eq!(entry.value, b"VALUE\xfe");
        assert_eq!(entry.encoding.as_deref(), Some("unix-bytes"));
    }

    #[test]
    fn caller_environment_is_sorted_by_wire_key() {
        let entries = caller_environment();
        assert!(entries.windows(2).all(|pair| pair[0].key <= pair[1].key));
    }
}
