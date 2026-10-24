//! Ordered, owner-retaining shutdown of runtime participants.
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Instant,
};

/// Ordering boundaries; implementations, not the host, own their policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ShutdownPhase {
    /// Commit retained state before releasing resources.
    Persist,
    /// Release owned resources.
    Release,
    /// Clear projections after resource release.
    Clear,
}

/// An owned shutdown operation. Dropping an observer must not cancel its owner.
pub trait ShutdownParticipant: Send + Sync {
    fn id(&self) -> &str;
    fn phase(&self) -> ShutdownPhase;
    fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;
}

type Completion = Option<Result<(), String>>;
#[derive(Default)]
struct State {
    participants: Vec<Arc<dyn ShutdownParticipant>>,
    completion: Option<tokio::sync::watch::Receiver<Completion>>,
}

/// Instance-scoped lifecycle registry, retained in the host state registry.
#[derive(Clone, Default)]
pub struct ShutdownRegistry(Arc<Mutex<State>>);
impl ShutdownRegistry {
    /// Register an owner before shutdown starts.
    ///
    /// # Errors
    /// Rejects duplicate identities, poisoned state, or late registration.
    pub fn register(&self, participant: Arc<dyn ShutdownParticipant>) -> Result<(), String> {
        let mut state = self.0.lock().map_err(|_| "shutdown registry poisoned")?;
        if state.completion.is_some() {
            return Err("shutdown already started".into());
        }
        if state
            .participants
            .iter()
            .any(|p| p.id() == participant.id())
        {
            return Err(format!(
                "duplicate shutdown participant: {}",
                participant.id()
            ));
        }
        state.participants.push(participant);
        Ok(())
    }

    /// Run once, sharing the retained result with concurrent/repeated observers.
    ///
    /// # Errors
    /// Reports every participant failure without discarding other cleanup work.
    pub async fn run(&self) -> Result<(), String> {
        self.run_with_progress(crate::shutdown_progress::ShutdownProgress::default())
            .await
    }

    /// Run with the host's observational progress counter.
    ///
    /// # Errors
    /// Reports the retained participant result, just like run.
    pub async fn run_with_progress(
        &self,
        progress: crate::shutdown_progress::ShutdownProgress,
    ) -> Result<(), String> {
        let mut completion = {
            let mut state = self.0.lock().map_err(|_| "shutdown registry poisoned")?;
            if let Some(receiver) = &state.completion {
                receiver.clone()
            } else {
                let (sender, receiver) = tokio::sync::watch::channel(None);
                state.completion = Some(receiver.clone());
                let participants = state.participants.clone();
                tokio::spawn(async move {
                    let result = run_participants(participants, progress).await;
                    sender.send_replace(Some(result));
                });
                receiver
            }
        };
        loop {
            if let Some(result) = completion.borrow_and_update().clone() {
                return result;
            }
            completion
                .changed()
                .await
                .map_err(|_| "shutdown owner ended without completion")?;
        }
    }
}

async fn run_participants(
    participants: Vec<Arc<dyn ShutdownParticipant>>,
    progress: crate::shutdown_progress::ShutdownProgress,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for phase in [
        ShutdownPhase::Persist,
        ShutdownPhase::Release,
        ShutdownPhase::Clear,
    ] {
        let mut tasks = tokio::task::JoinSet::new();
        for participant in participants.iter().filter(|p| p.phase() == phase) {
            let participant = Arc::clone(participant);
            let progress = progress.clone();
            tasks.spawn(async move {
                let start = Instant::now();
                tracing::info!(participant = participant.id(), ?phase, "shutdown participant started");
                let mut shutdown = participant.shutdown();
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                interval.tick().await;
                let result = loop {
                    tokio::select! {
                        result = &mut shutdown => break result,
                        _ = interval.tick() => tracing::warn!(participant = participant.id(), ?phase, elapsed_ms = start.elapsed().as_millis(), "shutdown participant still pending; ownership retained"),
                    }
                };
                tracing::info!(participant = participant.id(), ?phase, elapsed_ms = start.elapsed().as_millis(), error = ?result.as_ref().err(), "shutdown participant completed");
                if let Err(error) = progress.advance(&format!("participant {} settled", participant.id())) { tracing::warn!(%error, "shutdown progress publication failed"); }
                result.map_err(|error| format!("{}: {error}", participant.id()))
            });
        }
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error),
                Err(error) => errors.push(format!("shutdown participant panicked: {error}")),
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Participant {
        id: &'static str,
        phase: ShutdownPhase,
        calls: Arc<Mutex<Vec<&'static str>>>,
        fail: bool,
    }
    impl ShutdownParticipant for Participant {
        fn id(&self) -> &str {
            self.id
        }
        fn phase(&self) -> ShutdownPhase {
            self.phase
        }
        fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>> {
            let (id, calls, fail) = (self.id, Arc::clone(&self.calls), self.fail);
            Box::pin(async move {
                calls.lock().unwrap().push(id);
                if fail { Err("injected".into()) } else { Ok(()) }
            })
        }
    }
    #[tokio::test]
    async fn observer_cancellation_retains_owner_and_same_phase_runs_concurrently() {
        struct BarrierParticipant(&'static str, Arc<tokio::sync::Barrier>);
        impl ShutdownParticipant for BarrierParticipant {
            fn id(&self) -> &str {
                self.0
            }
            fn phase(&self) -> ShutdownPhase {
                ShutdownPhase::Release
            }
            fn shutdown(
                &self,
            ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>> {
                let barrier = Arc::clone(&self.1);
                Box::pin(async move {
                    barrier.wait().await;
                    Ok(())
                })
            }
        }
        let registry = ShutdownRegistry::default();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        for id in ["first", "second"] {
            registry
                .register(Arc::new(BarrierParticipant(id, Arc::clone(&barrier))))
                .unwrap();
        }
        let observer_registry = registry.clone();
        let observer = tokio::spawn(async move { observer_registry.run().await });
        tokio::task::yield_now().await;
        observer.abort();
        let _ = observer.await;
        tokio::time::timeout(std::time::Duration::from_secs(1), barrier.wait())
            .await
            .unwrap();
        assert_eq!(registry.run().await, Ok(()));
    }

    #[tokio::test]
    async fn ordered_once_and_failure_does_not_skip_release() {
        let registry = ShutdownRegistry::default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        for (id, phase, fail) in [
            ("release", ShutdownPhase::Release, false),
            ("persist", ShutdownPhase::Persist, true),
            ("clear", ShutdownPhase::Clear, false),
        ] {
            registry
                .register(Arc::new(Participant {
                    id,
                    phase,
                    calls: Arc::clone(&calls),
                    fail,
                }))
                .unwrap();
        }
        let (a, b) = tokio::join!(registry.run(), registry.run());
        assert_eq!(a, b);
        assert!(a.unwrap_err().contains("persist: injected"));
        assert_eq!(*calls.lock().unwrap(), ["persist", "release", "clear"]);
        assert!(registry.run().await.is_err());
        assert_eq!(calls.lock().unwrap().len(), 3);
        assert!(
            registry
                .register(Arc::new(Participant {
                    id: "late",
                    phase: ShutdownPhase::Clear,
                    calls,
                    fail: false
                }))
                .is_err()
        );
    }
}
