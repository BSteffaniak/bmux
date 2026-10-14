//! Read-only subscriptions to accepted publications; no source or control authority.
use std::{io, sync::Arc};

use crate::{Snapshot, SnapshotFeed, SnapshotPublisher};

/// A latest-only reader. Cloning retains no history and producers never wait for readers.
#[derive(Clone)]
pub struct CommittedSubscription {
    receiver: tokio::sync::watch::Receiver<Option<Arc<Snapshot>>>,
}

impl SnapshotPublisher {
    /// Subscribe only to successfully admitted, complete publications.
    /// # Errors
    /// Returns an error if the publication lock is poisoned.
    pub fn subscribe_committed(&self) -> io::Result<CommittedSubscription> {
        let sender = self
            .sender
            .lock()
            .map_err(|_| io::Error::other("publication lock poisoned"))?;
        Ok(CommittedSubscription {
            receiver: sender.subscribe(),
        })
    }
}

impl SnapshotFeed {
    #[must_use]
    pub fn subscribe_committed(&self) -> CommittedSubscription {
        CommittedSubscription {
            receiver: self.receiver.clone(),
        }
    }
}

impl CommittedSubscription {
    /// Read the current committed repair without consuming a change notification.
    #[must_use]
    pub fn latest(&self) -> Option<Arc<Snapshot>> {
        if self.is_closed() {
            None
        } else {
            self.receiver.borrow().clone()
        }
    }

    /// A closed producer invalidates retained output for live delivery.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.receiver.has_changed().is_err()
    }

    /// Wait for a replacement, coalescing all intermediate publications.
    /// # Errors
    /// Returns `BrokenPipe` when the producer has closed.
    pub async fn changed(&mut self) -> io::Result<Option<Arc<Snapshot>>> {
        self.receiver
            .changed()
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "publication closed"))?;
        Ok(self.receiver.borrow_and_update().clone())
    }
}

impl Snapshot {
    /// Complete encoded repair, not an exact-base delta. Resources remain immutable.
    #[must_use]
    pub fn binary(&self) -> Option<&crate::BinaryPresentation> {
        self.binary.as_ref()
    }

    #[must_use]
    pub fn resources(&self) -> &std::collections::BTreeMap<String, crate::Resource> {
        &self.resources
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn admission_and_slow_readers_preserve_latest_complete_commit() {
        let (publisher, _feed) = crate::snapshot_feed(32).unwrap();
        let mut reader = publisher.subscribe_committed().unwrap();
        assert!(reader.latest().is_none());
        for revision in 1..=128 {
            publisher.publish(revision, Arc::from([1_u8])).unwrap();
        }
        assert!(publisher.publish(128, Arc::from([2_u8])).is_err());
        assert!(publisher.publish(129, Arc::from([0_u8; 33])).is_err());
        let latest = reader.changed().await.unwrap().unwrap();
        assert_eq!(latest.revision, 128);
        assert_eq!(&*latest.bytes, &[1]);
        drop(publisher);
        assert!(reader.is_closed());
        assert!(reader.latest().is_none());
        assert!(reader.changed().await.is_err());
    }
}
