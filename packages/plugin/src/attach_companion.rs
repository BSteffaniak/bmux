//! Generic process-local lifecycle registry for attach-client plugin companions.
//!
//! Companions register bounded start/stop callbacks during client-adapter
//! installation. The attach runtime starts the current registry snapshot on
//! entry and stops it on exit without knowing plugin IDs or product domains.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

pub type AttachCompanionCallback = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Registry selection supplied by the invoking presentation runtime.
pub struct AttachPresentationResources {
    pub layouts: Arc<crate::layout::PluginLayoutRegistry>,
    pub allocations: Arc<crate::layout::AllocationRegistry>,
    pub surfaces: Arc<crate::surface::PluginSurfaceRegistry>,
    pub input: Arc<crate::AttachPresentationInputRegistry>,
    pub events: Arc<crate::EventBus>,
    /// This runtime's shared typed host state. Companions that cooperate
    /// within one runtime meet here instead of in process-wide statics.
    pub states: Arc<crate::PluginStateRegistry>,
    /// Per-plugin settings from the invoking runtime's effective config,
    /// keyed by plugin id. Companions read their settings here at start so
    /// a runtime with its own config (for example a sandbox) is honored
    /// instead of whatever config the process registered companions with.
    pub plugin_settings: Arc<BTreeMap<String, toml::Value>>,
    /// Root for state the invoking runtime keeps on its own machine across
    /// attaches and restarts. `None` (an isolated or embedded runtime) means
    /// companions keep nothing between installations.
    pub local_state_dir: Option<std::path::PathBuf>,
}

impl AttachPresentationResources {
    /// The directory `plugin_id`'s companion may keep local state in, when
    /// this runtime offers persisted local state at all.
    #[must_use]
    pub fn plugin_local_state_dir(&self, plugin_id: &str) -> Option<std::path::PathBuf> {
        self.local_state_dir
            .as_ref()
            .map(|root| root.join("plugins").join(plugin_id))
    }

    /// Allocate an independent frontend scope. No process-global registry is
    /// selected implicitly; companion factories receive these exact handles.
    #[must_use]
    pub fn isolated(maximum_owner_contributions: usize) -> Self {
        Self {
            layouts: Arc::new(crate::layout::PluginLayoutRegistry::new(
                maximum_owner_contributions,
            )),
            allocations: Arc::new(crate::layout::AllocationRegistry::default()),
            surfaces: Arc::new(crate::surface::PluginSurfaceRegistry::new(
                maximum_owner_contributions,
            )),
            input: Arc::new(crate::AttachPresentationInputRegistry::new()),
            events: Arc::new(crate::EventBus::new()),
            states: Arc::new(crate::PluginStateRegistry::new()),
            plugin_settings: Arc::new(BTreeMap::new()),
            local_state_dir: None,
        }
    }

    /// Settings the invoking runtime's config supplies for `plugin_id`.
    #[must_use]
    pub fn settings_for(&self, plugin_id: &str) -> Option<&toml::Value> {
        self.plugin_settings.get(plugin_id)
    }
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
    /// Factories must return an installation guard owning all registrations and
    /// resources they create. A failure must unwind partial installation locally;
    /// factories must not select process-global presentation resources.
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
    fn isolated_frontend_scopes_do_not_share_registry_handles() {
        let first = AttachPresentationResources::isolated(32);
        let second = AttachPresentationResources::isolated(32);
        assert!(!Arc::ptr_eq(&first.layouts, &second.layouts));
        assert!(!Arc::ptr_eq(&first.allocations, &second.allocations));
        assert!(!Arc::ptr_eq(&first.surfaces, &second.surfaces));
        assert!(!Arc::ptr_eq(&first.input, &second.input));
        assert!(!Arc::ptr_eq(&first.events, &second.events));
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
