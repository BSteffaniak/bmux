//! Read-only repair reader for the execution owner's actual scripted attach.
//! Never launches a second attach or admits audience input.
use crate::playbook_capture::{CaptureSnapshot, ManagedCaptureCursor};
use bmux_playbook::managed_display::DisplayPacket;
use bmux_playbook_plugin_api::playbook_types::Attempt;
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::watch;

#[derive(Debug)]
pub struct ObserverFailure {
    pub closed: bool,
    pub message: String,
}
impl From<String> for ObserverFailure {
    fn from(message: String) -> Self {
        Self {
            closed: false,
            message,
        }
    }
}
impl From<&str> for ObserverFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl std::fmt::Display for ObserverFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub async fn observe(
    _executable: &Path,
    owner: String,
    attempt: &Attempt,
    sender: watch::Sender<Option<Arc<CaptureSnapshot>>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ObserverFailure> {
    let root = std::path::PathBuf::from(&attempt.root);
    let generation = attempt.generation;
    let mut cursor = ManagedCaptureCursor::default();
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = interval.tick() => {}
        }
        let root = root.clone();
        let packet = tokio::task::spawn_blocking(move || DisplayPacket::read(&root, generation))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let Some(packet) = packet else {
            continue;
        };
        if let Some(snapshot) = cursor.admit(packet, &owner)? {
            sender.send_replace(Some(Arc::new(snapshot)));
        }
    }
}
