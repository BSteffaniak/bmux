//! Recording-owned native frontend and storage boundary. Platform selection is
//! centralized here. Stop events never grant execution-termination authority;
//! storage backends own private creation and durable namespace publication.
#[cfg(unix)]
#[path = "platform/unix.rs"]
mod native;
#[cfg(windows)]
#[path = "platform/windows.rs"]
mod native;
#[cfg(not(any(unix, windows)))]
compile_error!("interactive recording supports Unix and Windows hosts only");

#[cfg(windows)]
#[path = "platform/windows_storage.rs"]
mod storage;
#[cfg(unix)]
#[path = "platform/unix_storage.rs"]
mod storage;
#[cfg(windows)]
#[path = "platform/windows_security.rs"]
mod windows_security;
pub(crate) use native::{StopEvents, caller_environment, executable};
pub(crate) use storage::{
    create_new, create_private_directory, open_append, private_directory, publish,
    publish_noreplace, sync_directory, validate_runtime_socket,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StopEvent {
    Interrupted,
    Terminated,
    Disconnected,
}

impl StopEvent {
    pub(crate) fn into_error(self) -> anyhow::Error {
        use crate::interactive_terminal::InteractiveFailureKind;
        let (message, kind) = match self {
            Self::Interrupted => (
                "interactive frontend interrupted; execution retained; outstanding input is uncertain, no retry",
                InteractiveFailureKind::Interrupted,
            ),
            Self::Terminated => (
                "interactive frontend terminated; execution retained; outstanding input is uncertain, no retry",
                InteractiveFailureKind::Interrupted,
            ),
            Self::Disconnected => (
                "interactive terminal disconnected; execution retained; no automatic input resume",
                InteractiveFailureKind::Disconnect,
            ),
        };
        anyhow::anyhow!(message).context(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stop_retains_execution_and_failure_classification() {
        use crate::interactive_terminal::InteractiveFailureKind;
        for event in [
            StopEvent::Interrupted,
            StopEvent::Terminated,
            StopEvent::Disconnected,
        ] {
            let error = event.into_error();
            assert!(format!("{error:#}").contains("execution retained"));
            assert!(error.downcast_ref::<InteractiveFailureKind>().is_some());
        }
    }
}
