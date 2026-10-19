//! Retained, durable lifecycle authority; notifications are only hints to reread it.
use crate::protocol::{self, LifecycleSnapshot, Response};
use std::io;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

pub(super) struct Lifecycle {
    path: PathBuf,
    snapshot: Mutex<LifecycleSnapshot>,
    changed: Condvar,
}
impl Lifecycle {
    pub(super) fn new(path: PathBuf, status: Response) -> io::Result<Self> {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(io::Error::other(
                    "retained lifecycle exists; restart/relaunch requires explicit recovery",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let snapshot = LifecycleSnapshot {
            capability: protocol::LIFECYCLE_CAPABILITY,
            revision: 1,
            status,
        };
        crate::atomic_state::commit(&path, &snapshot)?;
        Ok(Self {
            path,
            snapshot: Mutex::new(snapshot),
            changed: Condvar::new(),
        })
    }
    pub(super) fn snapshot(&self) -> LifecycleSnapshot {
        self.snapshot.lock().expect("lifecycle lock").clone()
    }
    pub(super) fn update(&self, change: impl FnOnce(&mut Response)) -> io::Result<()> {
        let mut retained = self.snapshot.lock().expect("lifecycle lock");
        let mut next = retained.clone();
        change(&mut next.status);
        if serde_json::to_vec(&next.status).map_err(io::Error::other)?
            == serde_json::to_vec(&retained.status).map_err(io::Error::other)?
        {
            return Ok(());
        }
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("lifecycle revision exhausted"))?;
        crate::atomic_state::commit(&self.path, &next)?;
        *retained = next;
        self.changed.notify_all();
        Ok(())
    }
    pub(super) fn cancel(&self, cancelled: &std::sync::atomic::AtomicBool) {
        let _retained = self.snapshot.lock().expect("lifecycle lock");
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        self.changed.notify_all();
    }
    pub(super) fn next(
        &self,
        revision: u64,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Option<LifecycleSnapshot> {
        let retained = self.snapshot.lock().expect("lifecycle lock");
        let retained = self
            .changed
            .wait_while(retained, |s| {
                s.revision <= revision && !cancelled.load(std::sync::atomic::Ordering::SeqCst)
            })
            .expect("lifecycle lock");
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            None
        } else {
            Some(retained.clone())
        }
    }
    pub(super) fn wait_until(
        &self,
        deadline: Instant,
        ready: impl Fn(&Response) -> bool,
    ) -> io::Result<Response> {
        let retained = self.snapshot.lock().expect("lifecycle lock");
        let (retained, _) = self
            .changed
            .wait_timeout_while(
                retained,
                deadline.saturating_duration_since(Instant::now()),
                |s| !ready(&s.status),
            )
            .expect("lifecycle lock");
        if !ready(&retained.status) {
            return Err(io::Error::other(
                "terminal owner lifecycle deadline exceeded; outcome uncertain",
            ));
        }
        Ok(retained.status.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::Duration;
    fn status() -> Response {
        Response {
            version: protocol::VERSION,
            execution_token: "test".into(),
            started: false,
            target_pid: None,
            exit_code: None,
            exit_signal: None,
            group_consumed: false,
            target_terminated: false,
            terminal_session_ended: Some(false),
            cleanup_error: None,
            capture_error: None,
            error: None,
        }
    }
    #[test]
    fn initial_and_racing_change_are_retained_and_durable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lifecycle.json");
        let lifecycle = Lifecycle::new(path.clone(), status()).unwrap();
        let initial = lifecycle.snapshot();
        lifecycle
            .update(|s| {
                s.exit_code = Some(7);
                s.terminal_session_ended = Some(true);
            })
            .unwrap();
        let next = lifecycle
            .next(initial.revision, &AtomicBool::new(false))
            .unwrap();
        assert!(next.revision > initial.revision);
        assert_eq!(next.status.exit_code, Some(7));
        let durable: LifecycleSnapshot =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(durable.revision, next.revision);
        assert_eq!(durable.status.exit_code, Some(7));
    }
    #[test]
    fn cancellation_unblocks_without_manufacturing_completion() {
        let root = tempfile::tempdir().unwrap();
        let lifecycle =
            Arc::new(Lifecycle::new(root.path().join("lifecycle.json"), status()).unwrap());
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker = Arc::clone(&lifecycle);
        let stop = Arc::clone(&cancelled);
        let waiter = std::thread::spawn(move || worker.next(1, &stop));
        lifecycle.cancel(&cancelled);
        assert!(waiter.join().unwrap().is_none());
        assert_eq!(
            lifecycle.snapshot().status.terminal_session_ended,
            Some(false)
        );
    }
    #[test]
    fn no_op_does_not_advance_revision_and_deadline_is_uncertain() {
        let root = tempfile::tempdir().unwrap();
        let lifecycle = Lifecycle::new(root.path().join("lifecycle.json"), status()).unwrap();
        lifecycle.update(|_| {}).unwrap();
        assert_eq!(lifecycle.snapshot().revision, 1);
        assert!(
            lifecycle
                .wait_until(Instant::now() + Duration::from_millis(1), |s| s.started)
                .is_err()
        );
        assert!(!lifecycle.snapshot().status.started);
        assert!(Lifecycle::new(root.path().join("lifecycle.json"), status()).is_err());
    }
}
