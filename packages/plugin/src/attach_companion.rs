//! Generic process-local lifecycle registry for attach-client plugin companions.
//!
//! Companions register bounded start/stop callbacks during client-adapter
//! installation. The attach runtime starts the current registry snapshot on
//! entry and stops it on exit without knowing plugin IDs or product domains.

use std::sync::{Arc, OnceLock, RwLock};

pub type AttachCompanionCallback = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Registry selection supplied by the invoking presentation runtime.
pub struct AttachPresentationResources {
    pub layouts: Arc<crate::layout::PluginLayoutRegistry>,
    pub allocations: Arc<crate::layout::AllocationRegistry>,
    pub surfaces: Arc<crate::surface::PluginSurfaceRegistry>,
    pub input: Arc<crate::AttachPresentationInputRegistry>,
    pub events: Arc<crate::EventBus>,
}

pub type AttachCompanionFactory =
    Arc<dyn Fn(&AttachPresentationResources) -> Result<Box<dyn Send + Sync>, String> + Send + Sync>;

#[derive(Clone)]
pub struct AttachCompanion {
    id: String,
    start: AttachCompanionCallback,
    stop: AttachCompanionCallback,
    factory: Option<AttachCompanionFactory>,
}

/// A successfully started companion owned by one runtime invocation.
/// Dropping it stops the captured registration, even if the registry changed.
pub struct StartedAttachCompanion {
    companion: AttachCompanion,
    _installation: Option<Box<dyn Send + Sync>>,
}

impl Drop for StartedAttachCompanion {
    fn drop(&mut self) {
        if let Err(error) = self.companion.stop() {
            tracing::warn!(companion_id = self.companion.id(), %error, "failed stopping attach companion");
        }
    }
}

impl AttachCompanion {
    /// Register a factory whose returned installation is owned by the runtime.
    #[must_use]
    pub fn from_factory(id: impl Into<String>, factory: AttachCompanionFactory) -> Self {
        Self {
            id: id.into(),
            start: Arc::new(|| Ok(())),
            stop: Arc::new(|| Ok(())),
            factory: Some(factory),
        }
    }

    /// Install using the invoking runtime's selected resources.
    ///
    /// # Errors
    /// Returns installation or startup failure.
    pub fn start_with_resources(
        self,
        resources: &AttachPresentationResources,
    ) -> Result<StartedAttachCompanion, String> {
        if let Some(factory) = &self.factory {
            let installation = factory(resources)?;
            return Ok(StartedAttachCompanion {
                companion: self,
                _installation: Some(installation),
            });
        }
        self.start_owned()
    }
    /// Start and retain this exact registration until runtime teardown.
    ///
    /// # Errors
    /// Returns startup failure after attempting to clean up partial startup.
    pub fn start_owned(self) -> Result<StartedAttachCompanion, String> {
        if self.factory.is_some() {
            return Err("companion factory requires presentation resources".to_string());
        }
        let started = StartedAttachCompanion {
            companion: self,
            _installation: None,
        };
        started.companion.start()?;
        Ok(started)
    }
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        start: AttachCompanionCallback,
        stop: AttachCompanionCallback,
    ) -> Self {
        Self {
            id: id.into(),
            start,
            stop,
            factory: None,
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// # Errors
    /// Returns the companion callback's startup failure.
    pub fn start(&self) -> Result<(), String> {
        (self.start)()
    }

    /// # Errors
    /// Returns the companion callback's shutdown failure.
    pub fn stop(&self) -> Result<(), String> {
        (self.stop)()
    }
}

#[derive(Default)]
pub struct AttachCompanionRegistry {
    companions: RwLock<Vec<AttachCompanion>>,
}

impl AttachCompanionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, companion: AttachCompanion) {
        let Ok(mut companions) = self.companions.write() else {
            return;
        };
        if let Some(existing) = companions
            .iter_mut()
            .find(|existing| existing.id == companion.id)
        {
            *existing = companion;
        } else {
            companions.push(companion);
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<AttachCompanion> {
        self.companions
            .read()
            .map_or_else(|_| Vec::new(), |companions| companions.clone())
    }

    pub fn clear(&self) {
        if let Ok(mut companions) = self.companions.write() {
            companions.clear();
        }
    }
}

static GLOBAL_ATTACH_COMPANION_REGISTRY: OnceLock<AttachCompanionRegistry> = OnceLock::new();

#[must_use]
pub fn global_attach_companion_registry() -> &'static AttachCompanionRegistry {
    GLOBAL_ATTACH_COMPANION_REGISTRY.get_or_init(AttachCompanionRegistry::new)
}

pub fn register_attach_companion(companion: AttachCompanion) {
    global_attach_companion_registry().register(companion);
}

#[must_use]
pub fn registered_attach_companions() -> Vec<AttachCompanion> {
    global_attach_companion_registry().snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn owned_lifecycle_stops_original_registration_after_replacement() {
        let registry = AttachCompanionRegistry::new();
        let stopped = Arc::new(AtomicUsize::new(0));
        let counter = stopped.clone();
        registry.register(AttachCompanion::new(
            "example",
            Arc::new(|| Ok(())),
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }),
        ));
        let started = registry.snapshot().pop().unwrap().start_owned().unwrap();
        registry.register(AttachCompanion::new(
            "example",
            Arc::new(|| Ok(())),
            Arc::new(|| panic!("replacement must not be stopped")),
        ));
        drop(started);
        assert_eq!(stopped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_owned_start_cleans_up_partial_startup() {
        let stopped = Arc::new(AtomicUsize::new(0));
        let counter = stopped.clone();
        let companion = AttachCompanion::new(
            "example",
            Arc::new(|| Err("startup failed".into())),
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }),
        );
        assert!(companion.start_owned().is_err());
        assert_eq!(stopped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn duplicate_registration_replaces_callbacks() {
        let registry = AttachCompanionRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let initial = Arc::clone(&calls);
        registry.register(AttachCompanion::new(
            "example",
            Arc::new(move || {
                initial.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }),
            Arc::new(|| Ok(())),
        ));
        let replacement = Arc::clone(&calls);
        registry.register(AttachCompanion::new(
            "example",
            Arc::new(move || {
                replacement.fetch_add(10, Ordering::Relaxed);
                Ok(())
            }),
            Arc::new(|| Ok(())),
        ));
        let companions = registry.snapshot();
        assert_eq!(companions.len(), 1);
        companions[0].start().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 10);
    }
}
