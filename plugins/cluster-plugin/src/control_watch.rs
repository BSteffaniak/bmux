//! Control state pushed to watching connections (`cluster-control-watch`).
//!
//! A connection that watches is woken when consensus applies a new control revision; when the
//! host can send it a message, it is sent the revision (`CONTROL_APPLIED_KIND`) and then reads
//! what it needs. Like frames, nothing is queued: a connection that falls behind is sent the
//! latest revision once.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use bmux_plugin_sdk::WireEventSink;

/// Who watches, and the revision each was last sent.
#[derive(Default)]
pub struct ControlWatch {
    subscribers: Mutex<BTreeMap<uuid::Uuid, u64>>,
    /// The latest applied control revision the watcher saw.
    revision: AtomicU64,
    /// The task following consensus, once started.
    watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Whether the pull source is registered with the host (false when it cannot push).
    registered: OnceLock<bool>,
}

impl Drop for ControlWatch {
    fn drop(&mut self) {
        if let Some(watcher) = self
            .watcher
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            watcher.abort();
        }
    }
}

impl ControlWatch {
    /// Watch (or, with `enabled` false, stop watching) for `client`; returns the revision
    /// applied now. The client is sent each later one.
    ///
    /// # Errors
    ///
    /// The host cannot push to clients, the control state cannot be read, or there is no host
    /// runtime to follow consensus on.
    pub fn watch(
        self: &Arc<Self>,
        client: uuid::Uuid,
        enabled: bool,
        node: &crate::consensus_runtime::ConsensusNode,
        sink: Option<Arc<dyn WireEventSink>>,
    ) -> Result<u64, String> {
        let revision = node.control_revision().map_err(|error| error.to_string())?;
        self.revision.fetch_max(revision, Ordering::SeqCst);
        if !enabled {
            self.subscribers().remove(&client);
            return Ok(revision);
        }
        let sink = sink.ok_or_else(|| "the host cannot push to clients".to_string())?;
        if !self.register(&sink) {
            return Err("the host refused the control watch source".to_string());
        }
        self.subscribers().insert(client, revision);
        self.follow(node, sink)?;
        Ok(revision)
    }

    fn subscribers(&self) -> std::sync::MutexGuard<'_, BTreeMap<uuid::Uuid, u64>> {
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn register(self: &Arc<Self>, sink: &Arc<dyn WireEventSink>) -> bool {
        *self.registered.get_or_init(|| {
            let watch = Arc::downgrade(self);
            sink.register_pull_source(
                bmux_cluster_plugin_api::CONTROL_APPLIED_KIND.to_string(),
                Arc::new(move |client| watch.upgrade().and_then(|watch| watch.pull(client))),
            )
            .is_ok()
        })
    }

    /// Start following consensus, once: when the applied log advances, read the control
    /// revision, and wake the watchers if it moved.
    fn follow(
        self: &Arc<Self>,
        node: &crate::consensus_runtime::ConsensusNode,
        sink: Arc<dyn WireEventSink>,
    ) -> Result<(), String> {
        let mut watcher = self.watcher.lock().unwrap_or_else(PoisonError::into_inner);
        if watcher.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|error| format!("following consensus needs the host runtime: {error}"))?;
        let watch = Arc::downgrade(self);
        let node = node.clone();
        let task = runtime.spawn(async move {
            let mut metrics = node.metrics_watch();
            let mut applied = metrics.borrow().last_applied;
            while metrics.changed().await.is_ok() {
                let now = metrics.borrow().last_applied;
                if now == applied {
                    continue;
                }
                applied = now;
                let Some(watch) = watch.upgrade() else {
                    return;
                };
                let Ok(revision) = node.control_revision() else {
                    continue;
                };
                if watch.revision.fetch_max(revision, Ordering::SeqCst) < revision {
                    watch.wake_all(sink.as_ref());
                }
            }
        });
        *watcher = Some(task);
        drop(watcher);
        Ok(())
    }

    /// Wake every watcher; one that cannot be woken (disconnected) stops watching.
    fn wake_all(&self, sink: &dyn WireEventSink) {
        let clients: Vec<uuid::Uuid> = self.subscribers().keys().copied().collect();
        for client in clients {
            if sink.wake(client).is_err() {
                self.subscribers().remove(&client);
            }
        }
    }

    /// The host can send to `client`: the revision, if newer than the one it was last sent.
    fn pull(&self, client: uuid::Uuid) -> Option<Vec<u8>> {
        let revision = self.revision.load(Ordering::SeqCst);
        let mut subscribers = self.subscribers();
        let sent = subscribers.get_mut(&client)?;
        if *sent >= revision {
            return None;
        }
        *sent = revision;
        drop(subscribers);
        bmux_ipc::encode(&revision).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A watcher is sent a revision once, only when newer than what it was sent, and one that
    /// stopped watching is sent nothing.
    #[test]
    fn watchers_are_sent_each_newer_revision_once() {
        let watch = ControlWatch::default();
        let (client, other) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        watch.subscribers().insert(client, 3);
        watch.revision.store(3, Ordering::SeqCst);
        assert_eq!(watch.pull(client), None, "nothing newer yet");
        watch.revision.store(5, Ordering::SeqCst);
        assert_eq!(watch.pull(client), Some(bmux_ipc::encode(&5_u64).unwrap()));
        assert_eq!(watch.pull(client), None, "sent once");
        assert_eq!(watch.pull(other), None, "not watching");
    }
}
