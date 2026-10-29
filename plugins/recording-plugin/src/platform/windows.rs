use std::os::windows::ffi::OsStrExt;

use anyhow::Result;
use bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry;
use tokio::signal::windows::{
    CtrlBreak, CtrlC, CtrlClose, CtrlLogoff, CtrlShutdown, ctrl_break, ctrl_c, ctrl_close,
    ctrl_logoff, ctrl_shutdown,
};

use super::StopEvent;

pub(crate) struct StopEvents {
    interrupt: CtrlC,
    terminate: CtrlBreak,
    close: CtrlClose,
    logoff: CtrlLogoff,
    shutdown: CtrlShutdown,
}

impl StopEvents {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            interrupt: ctrl_c()?,
            terminate: ctrl_break()?,
            close: ctrl_close()?,
            logoff: ctrl_logoff()?,
            shutdown: ctrl_shutdown()?,
        })
    }

    pub(crate) async fn recv(&mut self) -> anyhow::Error {
        let event = tokio::select! {
            _ = self.interrupt.recv() => StopEvent::Interrupted,
            _ = self.terminate.recv() => StopEvent::Terminated,
            _ = self.close.recv() => StopEvent::Disconnected,
            _ = self.logoff.recv() => StopEvent::Disconnected,
            _ = self.shutdown.recv() => StopEvent::Disconnected,
        };
        event.into_error()
    }
}

pub(crate) fn executable(path: &std::path::Path) -> bool {
    path.is_file()
}

fn environment_entry(key: &std::ffi::OsStr, value: &std::ffi::OsStr) -> EnvironmentEntry {
    EnvironmentEntry {
        key: key.encode_wide().flat_map(u16::to_le_bytes).collect(),
        value: value.encode_wide().flat_map(u16::to_le_bytes).collect(),
        encoding: Some("windows-utf16le".into()),
    }
}

pub(crate) fn caller_environment() -> Vec<EnvironmentEntry> {
    let mut entries: Vec<_> = std::env::vars_os()
        .map(|(key, value)| environment_entry(&key, &value))
        .collect();
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn environment_preserves_utf16_including_unpaired_surrogates() {
        let key = OsString::from_wide(&[0x004b, 0xd800]);
        let value = OsString::from_wide(&[0x00e9, 0xd83d, 0xde42, 0xdc00]);
        let entry = environment_entry(&key, &value);
        assert_eq!(entry.key, [0x4b, 0x00, 0x00, 0xd8]);
        assert_eq!(
            entry.value,
            [0xe9, 0x00, 0x3d, 0xd8, 0x42, 0xde, 0x00, 0xdc]
        );
        assert_eq!(entry.encoding.as_deref(), Some("windows-utf16le"));
    }
}
