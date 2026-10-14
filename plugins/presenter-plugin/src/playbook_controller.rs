//! Bounded asynchronous lifecycle admission shared by local and managed owners.
//! No calls from paint/poll; callers publish the latest snapshot on their update turn.
use crate::playbook::{Approval, DemoConfig};
use bmux_playbook_plugin_api::{
    playbook_commands as commands, playbook_state as state, playbook_types::Attempt,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};

const COMMAND_CAPACITY: usize = 8;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Run,
    Pause,
    Step,
    Resume,
    Stop,
    Restart,
}

#[derive(Clone, Debug)]
pub struct LifecycleSnapshot {
    /// Controller identity, separate from service execution generation.
    pub owner_epoch: uuid::Uuid,
    pub revision: u64,
    pub attempt: Option<Attempt>,
    pub acknowledgment: Option<String>,
    pub surface: Option<Arc<crate::playbook_capture::CaptureSnapshot>>,
    pub cancellation_revision: u64,
    pub unresolved: bool,
    pub error: Option<String>,
}
struct Admitted {
    cancellation_revision: u64,
    expected_generation: Option<u64>,
    control: Control,
}

/// Dropping the caller requests orderly stop; the owned task remains responsible
/// for observing cleanup. It does not block synchronous owner destruction.
pub struct Controller {
    owner_epoch: uuid::Uuid,
    commands: mpsc::Sender<Admitted>,
    snapshots: watch::Receiver<LifecycleSnapshot>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    task: Option<tokio::task::JoinHandle<()>>,
    cancellation: watch::Sender<u64>,
    shutdown: watch::Sender<bool>,
}
impl Controller {
    /// An inert snapshot endpoint for speculative owner projection. No task or
    /// live sender is shared with the original controller.
    pub(crate) fn detached(snapshot: LifecycleSnapshot) -> Self {
        let (commands, _) = mpsc::channel(COMMAND_CAPACITY);
        let (_, snapshots) = watch::channel(snapshot.clone());
        let (shutdown, _) = watch::channel(false);
        let (cancellation, _) = watch::channel(snapshot.cancellation_revision);
        Self {
            owner_epoch: snapshot.owner_epoch,
            commands,
            snapshots,
            shutdown,
            cancellation,
            pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            task: None,
        }
    }
    #[cfg(test)]
    pub(crate) fn test_endpoint(
        snapshot: LifecycleSnapshot,
    ) -> (
        Self,
        watch::Sender<LifecycleSnapshot>,
        watch::Receiver<u64>,
        watch::Receiver<bool>,
    ) {
        let mut controller = Self::detached(snapshot.clone());
        let (updates, snapshots) = watch::channel(snapshot);
        let (cancellation, cancelled) = watch::channel(0);
        let (shutdown, stopped) = watch::channel(false);
        controller.snapshots = snapshots;
        controller.cancellation = cancellation;
        controller.shutdown = shutdown;
        (controller, updates, cancelled, stopped)
    }
    /// Requires an explicit async runtime owner. Does not prepare/start until Run.
    pub fn spawn(
        runtime: &tokio::runtime::Handle,
        paths: bmux_config::ConfigPaths,
        approval: Approval,
        config: DemoConfig,
        executable: PathBuf,
        attempts_root: PathBuf,
    ) -> Self {
        let owner_epoch = uuid::Uuid::new_v4();
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (snapshot_tx, snapshots) = watch::channel(LifecycleSnapshot {
            owner_epoch,
            revision: 0,
            attempt: None,
            acknowledgment: None,
            surface: None,
            cancellation_revision: 0,
            unresolved: false,
            error: None,
        });
        let (cancellation, cancellation_rx) = watch::channel(0);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (capture_tx, capture_rx) = watch::channel(None);
        let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task = runtime.spawn(
            Worker {
                cancellation: cancellation_rx,
                cancelled_through: 0,
                preparation_request_id: uuid::Uuid::new_v4().to_string(),
                pending: Arc::clone(&pending),
                paths,
                approval,
                config,
                executable,
                attempts_root,
                commands: command_rx,
                snapshots: snapshot_tx,
                shutdown: shutdown_rx,
                capture_tx,
                capture_rx,
                observer: None,
                observer_failed: None,
                client: None,
                attempt: None,
                stop_requested: false,
                uncertain: false,
            }
            .run(),
        );
        Self {
            owner_epoch,
            commands: command_tx,
            snapshots,
            shutdown,
            cancellation,
            pending,
            task: Some(task),
        }
    }
    pub fn cancel_navigation(&self) {
        self.cancellation
            .send_modify(|revision| *revision = revision.saturating_add(1));
    }
    pub fn owner_epoch(&self) -> uuid::Uuid {
        self.owner_epoch
    }
    /// Immediate admission acknowledgment, never a claim of execution completion.
    pub fn admit(&self, expected_generation: Option<u64>, control: Control) -> Result<(), String> {
        if *self.shutdown.borrow() {
            return Err("playbook owner is shutting down".into());
        }
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Err(error) = self.commands.try_send(Admitted {
            expected_generation,
            control,
            cancellation_revision: *self.cancellation.borrow(),
        }) {
            self.pending
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(format!("playbook command not admitted: {error}"));
        }
        Ok(())
    }
    /// Async consumers wait on coalesced complete updates rather than clone state
    /// on every timer tick. Closed task ownership is reported explicitly.
    pub async fn changed(&mut self) -> Result<LifecycleSnapshot, String> {
        self.snapshots
            .changed()
            .await
            .map_err(|error| error.to_string())?;
        Ok(self.snapshots.borrow_and_update().clone())
    }
    pub fn settled(&self) -> bool {
        self.pending.load(std::sync::atomic::Ordering::SeqCst) == 0
            && self.snapshots.borrow().cancellation_revision == *self.cancellation.borrow()
            && !self.snapshots.borrow().unresolved
            && self
                .snapshots
                .borrow()
                .attempt
                .as_ref()
                .is_none_or(|attempt| attempt.cleanup_acknowledged && attempt.phase != "prepared")
    }
    pub fn latest(&self) -> LifecycleSnapshot {
        self.snapshots.borrow().clone()
    }
    /// Latest complete lifecycle snapshot; slow consumers repair rather than lose
    /// an incremental status stream. Display snapshots have separate F ownership.
    pub fn take_changed(&mut self) -> Option<LifecycleSnapshot> {
        if self.snapshots.has_changed().unwrap_or(false) {
            Some(self.snapshots.borrow_and_update().clone())
        } else {
            None
        }
    }
    pub async fn shutdown_settled(&mut self) -> Result<(), String> {
        self.request_shutdown();
        let snapshot = self.latest();
        if snapshot.unresolved {
            return Err("playbook outcome unresolved; cleanup cannot be acknowledged".into());
        }
        if snapshot.attempt.as_ref().is_some_and(|attempt| {
            attempt.phase == "cleanup-failed" && !attempt.cleanup_acknowledged
        }) {
            return Err(
                "playbook cleanup failed; settlement NOT acknowledged; runner ownership retained"
                    .into(),
            );
        }
        if let Some(task) = self.task.as_mut() {
            // Borrow the handle while waiting: cancellation of a local UI deadline
            // must not silently remove explicit task ownership from the controller.
            match tokio::time::timeout(Duration::from_secs(20), task).await {
                Ok(result) => {
                    self.task.take();
                    result.map_err(|error| error.to_string())?;
                    if !self.settled() {
                        return Err("task ended without confirmed cleanup settlement".into());
                    }
                    Ok(())
                }
                Err(_) => Err("owned playbook cleanup remains unresolved".into()),
            }
        } else if self.settled() {
            Ok(())
        } else {
            Err("cleanup settlement not confirmed".into())
        }
    }
    pub fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

struct ObserverTask {
    generation: u64,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

struct Worker {
    cancellation: watch::Receiver<u64>,
    cancelled_through: u64,
    preparation_request_id: String,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    paths: bmux_config::ConfigPaths,
    approval: Approval,
    config: DemoConfig,
    executable: PathBuf,
    attempts_root: PathBuf,
    commands: mpsc::Receiver<Admitted>,
    snapshots: watch::Sender<LifecycleSnapshot>,
    shutdown: watch::Receiver<bool>,
    capture_tx: watch::Sender<Option<Arc<crate::playbook_capture::CaptureSnapshot>>>,
    capture_rx: watch::Receiver<Option<Arc<crate::playbook_capture::CaptureSnapshot>>>,
    observer_failed: Option<u64>,
    observer: Option<ObserverTask>,
    client: Option<bmux_client::BmuxClient>,
    attempt: Option<Attempt>,
    stop_requested: bool,
    uncertain: bool,
}
impl Worker {
    async fn run(mut self) {
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let cancellation_revision = *self.cancellation.borrow_and_update();
            if cancellation_revision != self.cancelled_through {
                self.cancelled_through = cancellation_revision;
                if self.attempt.is_some() {
                    let result = self.execute(Control::Stop).await;
                    self.publish_result(result);
                } else {
                    self.publish_result(Ok(
                        "navigation cancellation retained; no confirmed attempt".into(),
                    ));
                }
            }
            let shutdown = *self.shutdown.borrow();
            if shutdown {
                if !self.uncertain
                    && self.attempt.as_ref().is_none_or(|attempt| {
                        attempt.cleanup_acknowledged && attempt.phase != "prepared"
                    })
                {
                    break;
                }
                if !self.stop_requested {
                    self.stop_requested = true;
                    let result = match tokio::time::timeout(
                        Duration::from_secs(15),
                        self.execute(Control::Stop),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            self.uncertain = true;
                            self.client = None;
                            Err("shutdown stop outcome uncertain; cleanup unresolved".into())
                        }
                    };
                    self.publish_result(result);
                }
            }
            self.reconcile_observer().await;
            tokio::select! {
                _ = self.cancellation.changed() => {}
                changed = self.capture_rx.changed() => {
                    if changed.is_ok() {
                        let capture = self.capture_rx.borrow_and_update().clone();
                        if capture.as_ref().is_some_and(|frame| self.attempt.as_ref().is_some_and(|attempt| attempt.generation == frame.attempt_generation)) {
                            self.snapshots.send_modify(|snapshot| {
                                snapshot.surface = capture.clone();
                                snapshot.revision = snapshot.revision.saturating_add(1);
                            });
                        }
                    }
                }
                changed = self.shutdown.changed(), if !shutdown => {
                    if changed.is_err() { self.shutdown = watch::channel(true).1; }
                }
                admitted = self.commands.recv(), if !shutdown => {
                    let Some(admitted) = admitted else { self.shutdown = watch::channel(true).1; continue; };
                    let generation = self.attempt.as_ref().map(|attempt| attempt.generation);
                    let result = if admitted.cancellation_revision != *self.cancellation.borrow() {
                        Err("command cancelled by navigation before execution".into())
                    } else if admitted.expected_generation != generation {
                        Err("stale playbook generation; reconcile before submitting".into())
                    } else {
                        match tokio::time::timeout(Duration::from_secs(15), self.execute(admitted.control)).await {
                            Ok(result) => result,
                            Err(_) => { self.uncertain = true; Err("command timed out; outcome uncertain; do not replay".into()) }
                        }
                    };
                    self.publish_result(result);
                    self.pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
                _ = tick.tick() => {
                    if self.uncertain && self.attempt.is_none() {
                        let result = self.lookup_preparation().await;
                        if let Err(error) = result { self.publish_result(Err(error)); }
                    }
                    if self.attempt.is_some() {
                        let result = match tokio::time::timeout(Duration::from_secs(5), self.refresh()).await {
                            Ok(result) => result,
                            Err(_) => { self.client = None; Err("status timed out; retaining last confirmed state".into()) },
                        };
                        if let Err(error) = result { self.publish_result(Err(error)); }
                    }
                }
            }
        }
        self.close_observer().await;
        self.pending.store(0, std::sync::atomic::Ordering::SeqCst);
    }
    async fn close_observer(&mut self) {
        if let Some(ObserverTask { stop, mut task, .. }) = self.observer.take() {
            stop.send_replace(true);
            if tokio::time::timeout(Duration::from_secs(8), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }
    async fn reconcile_observer(&mut self) {
        let generation = self.attempt.as_ref().map(|attempt| attempt.generation);
        if self
            .observer
            .as_ref()
            .is_some_and(|observer| Some(observer.generation) != generation)
        {
            self.close_observer().await;
            self.capture_rx.borrow_and_update();
            self.capture_tx.send_replace(None);
            self.snapshots.send_modify(|snapshot| {
                snapshot.surface = None;
                snapshot.revision = snapshot.revision.saturating_add(1);
            });
        }
        if self
            .observer
            .as_ref()
            .is_some_and(|observer| observer.task.is_finished())
        {
            if let Some(ObserverTask { task, .. }) = self.observer.take()
                && let Ok(Err(error)) = task.await
            {
                self.publish_result(Err(error));
            }
            self.observer_failed = generation;
        }
        if self.observer.is_none()
            && self.observer_failed != generation
            && let Some(attempt) = self
                .attempt
                .as_ref()
                .filter(|attempt| attempt.session_id.is_some() && !attempt.cleanup_acknowledged)
        {
            let attempt = attempt.clone();
            let executable = self.executable.clone();
            let owner = self.snapshots.borrow().owner_epoch.to_string();
            let sender = self.capture_tx.clone();
            let (stop, receiver) = watch::channel(false);
            let task = tokio::spawn(async move {
                crate::playbook_observer::observe(&executable, owner, &attempt, sender, receiver)
                    .await
            });
            self.observer = Some(ObserverTask {
                generation: self
                    .attempt
                    .as_ref()
                    .map_or(0, |attempt| attempt.generation),
                stop,
                task,
            });
        }
    }
    fn publish_result(&self, result: Result<String, String>) {
        self.snapshots.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.cancellation_revision = self.cancelled_through;
            snapshot.unresolved = self.uncertain;
            if snapshot.surface.as_ref().is_some_and(|surface| {
                self.attempt
                    .as_ref()
                    .is_none_or(|attempt| attempt.generation != surface.attempt_generation)
            }) {
                snapshot.surface = None;
            }
            snapshot.attempt.clone_from(&self.attempt);
            match result {
                Ok(ref acknowledgment) => {
                    snapshot.acknowledgment = Some(acknowledgment.clone());
                    snapshot.error = None;
                }
                Err(ref error) => {
                    snapshot.acknowledgment = None;
                    snapshot.error = Some(error.clone());
                }
            }
        });
    }
    async fn connect(&mut self) -> Result<(), String> {
        if self.client.is_none() {
            self.client = Some(
                bmux_client::BmuxClient::connect_with_paths(
                    &self.paths,
                    "presenter playbook owner",
                )
                .await
                .map_err(|error| error.to_string())?,
            );
        }
        Ok(())
    }
    async fn execute(&mut self, control: Control) -> Result<String, String> {
        match tokio::time::timeout(Duration::from_secs(10), self.execute_inner(control)).await {
            Ok(result) => result,
            Err(_) => {
                self.uncertain = true;
                self.client = None;
                Err("command deadline exceeded; outcome unresolved; never replay".into())
            }
        }
    }
    async fn execute_inner(&mut self, control: Control) -> Result<String, String> {
        if self.uncertain && control != Control::Stop {
            return Err(
                "previous command outcome uncertain; reconcile before further control".into(),
            );
        }
        self.connect().await?;
        if matches!(control, Control::Run | Control::Restart)
            && (*self.cancellation.borrow() != self.cancelled_through || *self.shutdown.borrow())
        {
            return Err("execution cancelled before preparation".into());
        }
        if control == Control::Run && self.attempt.is_none() {
            let mut preparation = self
                .approval
                .preparation(&self.config, &self.executable, &self.attempts_root)
                .map_err(|error| error.to_string())?;
            preparation
                .request_id
                .clone_from(&self.preparation_request_id);
            let result = commands::client::prepare(
                self.client.as_mut().ok_or("service connection missing")?,
                preparation,
            )
            .await;
            let attempt = match result {
                Ok(result) => result.map_err(|error| format!("{error:?}"))?,
                Err(error) => {
                    self.uncertain = true;
                    self.client = None;
                    return Err(format!(
                        "preparation outcome uncertain; do not replay: {error}"
                    ));
                }
            };
            self.attempt = Some(attempt);
            if *self.cancellation.borrow() != self.cancelled_through {
                return Err("navigation cancelled prepared run; stopping before start".into());
            }
        }
        let attempt = self
            .attempt
            .as_ref()
            .ok_or("Run must prepare an attempt first")?;
        let generation = attempt.generation;
        if matches!(control, Control::Run | Control::Restart) {
            // Source change never inherits original approval, including restart.
            self.approval
                .preparation(&self.config, &self.executable, &self.attempts_root)
                .map_err(|error| error.to_string())?;
        }
        if control == Control::Run && attempt.phase != "prepared" {
            return Err("attempt already started; stop and settle before Restart".into());
        }
        if control == Control::Restart {
            let settled = state::client::settled(
                self.client.as_mut().ok_or("service connection missing")?,
                generation,
            )
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("{error:?}"))?;
            if settled.generation != generation || !settled.cleanup_acknowledged {
                return Err(
                    "restart refused until previous attempt cleanup is acknowledged".into(),
                );
            }
        }
        if matches!(control, Control::Run | Control::Restart)
            && (*self.cancellation.borrow() != self.cancelled_through || *self.shutdown.borrow())
        {
            return Err("execution cancelled before mutation admission".into());
        }
        let client = self.client.as_mut().ok_or("service connection missing")?;
        let response = match control {
            Control::Run => commands::client::start(client, generation).await,
            Control::Pause => commands::client::pause_script(client, generation).await,
            Control::Step => commands::client::next_step(client, generation).await,
            Control::Resume => commands::client::resume_script(client, generation).await,
            Control::Stop => commands::client::stop(client, generation).await,
            Control::Restart => commands::client::restart(client, generation).await,
        };
        let next = match response {
            Ok(result) => result.map_err(|error| format!("{error:?}"))?,
            Err(error) => {
                self.uncertain = true;
                self.client = None;
                return Err(format!(
                    "command outcome uncertain; reconcile, do not replay: {error}"
                ));
            }
        };
        if control == Control::Restart {
            if next.generation == generation {
                return Err("restart reused an execution generation".into());
            }
        } else if next.generation != generation {
            return Err("command returned another execution generation".into());
        }
        self.attempt = Some(next);
        Ok(format!("{control:?} acknowledged by runner"))
    }
    async fn lookup_preparation(&mut self) -> Result<(), String> {
        match tokio::time::timeout(Duration::from_secs(5), self.lookup_preparation_inner()).await {
            Ok(result) => {
                if result.is_err() {
                    self.client = None;
                }
                result
            }
            Err(_) => {
                self.client = None;
                Err("preparation lookup timed out; ownership remains unresolved".into())
            }
        }
    }
    async fn lookup_preparation_inner(&mut self) -> Result<(), String> {
        self.connect().await?;
        let found = state::client::lookup_preparation(
            self.client.as_mut().ok_or("service connection missing")?,
            self.preparation_request_id.clone(),
        )
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("preparation remains unresolved: {error:?}"))?;
        self.attempt = Some(found);
        self.uncertain = false;
        self.publish_result(Ok(
            "Preparation reconciled without replay; Run was not retried".into(),
        ));
        if *self.shutdown.borrow() || *self.cancellation.borrow() != 0 {
            let result = self.execute(Control::Stop).await;
            self.publish_result(result);
        }
        Ok(())
    }
    async fn refresh(&mut self) -> Result<(), String> {
        self.connect().await?;
        let old = self.attempt.as_ref().ok_or("attempt missing")?;
        let generation = old.generation;
        let revision = old.revision;
        let next = state::client::watch(
            self.client.as_mut().ok_or("service connection missing")?,
            generation,
            revision,
        )
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("{error:?}"))?;
        if next.generation != generation || next.revision < revision {
            return Ok(());
        }
        if next.cleanup_acknowledged
            && !matches!(
                next.phase.as_str(),
                "prepared" | "probing" | "starting" | "running" | "stopping" | "cleanup-failed"
            )
        {
            self.uncertain = false;
        }
        if next.revision != revision || self.snapshots.borrow().unresolved != self.uncertain {
            self.attempt = Some(next);
            self.snapshots.send_modify(|snapshot| {
                snapshot.revision = snapshot.revision.saturating_add(1);
                snapshot.unresolved = self.uncertain;
                snapshot.attempt.clone_from(&self.attempt);
                snapshot.error = None;
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_bypasses_full_queue_and_blocks_settlement() {
        let owner_epoch = uuid::Uuid::new_v4();
        let (commands, mut queued) = mpsc::channel(1);
        let (updates, snapshots) = watch::channel(LifecycleSnapshot {
            owner_epoch,
            revision: 0,
            attempt: None,
            acknowledgment: None,
            surface: None,
            cancellation_revision: 0,
            unresolved: false,
            error: None,
        });
        let (shutdown, _shutdown_rx) = watch::channel(false);
        let (cancellation, _cancellation_rx) = watch::channel(0);
        let controller = Controller {
            owner_epoch,
            commands,
            snapshots,
            pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            task: None,
            shutdown,
            cancellation,
        };
        controller.admit(None, Control::Run).unwrap();
        assert!(controller.admit(None, Control::Run).is_err());
        controller.cancel_navigation();
        let admitted = queued.try_recv().unwrap();
        assert_ne!(
            admitted.cancellation_revision,
            *controller.cancellation.borrow()
        );
        controller
            .pending
            .store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(!controller.settled());
        updates.send_modify(|snapshot| {
            snapshot.cancellation_revision = *controller.cancellation.borrow();
            snapshot.unresolved = true;
        });
        assert!(
            !controller.settled(),
            "uncertain Prepare is not an absent settled run"
        );
        updates.send_modify(|snapshot| snapshot.unresolved = false);
        assert!(controller.settled());
    }
}
