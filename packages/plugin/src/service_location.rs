//! Per-process map of plugin service locality.
//!
//! Typed service calls between plugins need to resolve to the
//! *activated* provider instance, which lives in whichever process ran
//! that plugin's `activate` callback. In the canonical deployment the
//! server process is the one that activates plugins. Other processes
//! — an attach client, a standalone `bmux <command>` invocation — load
//! the same plugin crates in order to dispatch commands locally, but
//! they never call `activate`. If one of those processes invokes a
//! plugin's typed service handler in-process, the handler finds no
//! registered state and either returns an error or (in the worst case)
//! panics.
//!
//! The [`ServiceLocationMap`] records, for each plugin id, whether the
//! current process holds the activated provider (`Local`) or must
//! forward typed service calls elsewhere (`Remote`). Providers that
//! are not registered at all are treated as unreachable; typed
//! dispatch returns a structured error rather than synthesizing a
//! local no-op provider.
//!
//! # Lifecycle
//!
//! 1. Server bootstrap calls [`ServiceLocationMap::mark_local`] for
//!    every plugin it successfully activates.
//! 2. Client-side bootstraps (attach / standalone CLI) call
//!    [`ServiceLocationMap::mark_remote`] for every plugin that will
//!    be reachable over IPC (typically every enabled plugin, since
//!    the server activates all of them).
//! 3. `loader::call_service_raw` consults the map before dispatching
//!    a typed plugin-to-plugin call: `Local` stays in-process,
//!    `Remote` forwards via the host kernel bridge as
//!    `Request::InvokeService`, and absence yields
//!    `PluginError::ServiceProviderUnreachable`.
//!
//! The map is keyed by plugin id string rather than `TypeId` so it can
//! be populated from information available at load time, before any
//! typed state structs exist.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

/// Where a plugin's service handlers actually live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceLocation {
    /// The current process activated this plugin; dispatch in-process.
    Local,
    /// Another process activated this plugin; forward calls via the
    /// host kernel bridge.
    Remote,
}

/// Process-wide map of plugin id → [`ServiceLocation`].
#[derive(Default, Debug)]
pub struct ServiceLocationMap {
    entries: RwLock<HashMap<String, ServiceLocation>>,
}

impl ServiceLocationMap {
    /// Create an empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `plugin_id` is activated in the current process.
    ///
    /// Replaces any prior entry.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn mark_local(&self, plugin_id: &str) {
        let mut guard = self
            .entries
            .write()
            .expect("service location map lock poisoned");
        guard.insert(plugin_id.to_string(), ServiceLocation::Local);
    }

    /// Record that `plugin_id` lives in a different process and that
    /// typed calls should be forwarded via the host kernel bridge.
    ///
    /// Does not replace an existing `Local` entry — once a plugin is
    /// known to be activated locally, it stays local. This lets
    /// bootstraps mark a batch of plugins as remote without clobbering
    /// the self-registration performed by the local activate path.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn mark_remote(&self, plugin_id: &str) {
        let mut guard = self
            .entries
            .write()
            .expect("service location map lock poisoned");
        guard
            .entry(plugin_id.to_string())
            .or_insert(ServiceLocation::Remote);
    }

    /// Look up the recorded location of `plugin_id`. Returns `None`
    /// when no entry has been registered.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn get(&self, plugin_id: &str) -> Option<ServiceLocation> {
        let guard = self
            .entries
            .read()
            .expect("service location map lock poisoned");
        guard.get(plugin_id).copied()
    }

    /// Remove every recorded entry. Test-only helper.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn clear(&self) {
        let mut guard = self
            .entries
            .write()
            .expect("service location map lock poisoned");
        guard.clear();
    }

    /// Number of recorded entries.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .expect("service location map lock poisoned")
            .len()
    }

    /// `true` when no entries are recorded.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Process-wide shared [`ServiceLocationMap`] instance.
///
/// Typed service dispatch, server bootstrap, and client bootstrap all
/// reference the same singleton so locality decisions stay consistent
/// across crates.
#[must_use]
pub fn global_service_locations() -> Arc<ServiceLocationMap> {
    static GLOBAL: OnceLock<Arc<ServiceLocationMap>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| Arc::new(ServiceLocationMap::new()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::{ServiceLocation, ServiceLocationMap, global_service_locations};
    use std::sync::Arc;

    #[test]
    fn mark_local_records_local_location() {
        let map = ServiceLocationMap::new();
        map.mark_local("bmux.contexts");
        assert_eq!(map.get("bmux.contexts"), Some(ServiceLocation::Local));
    }

    #[test]
    fn mark_remote_records_remote_location_when_absent() {
        let map = ServiceLocationMap::new();
        map.mark_remote("bmux.contexts");
        assert_eq!(map.get("bmux.contexts"), Some(ServiceLocation::Remote));
    }

    #[test]
    fn mark_remote_does_not_override_local() {
        let map = ServiceLocationMap::new();
        map.mark_local("bmux.contexts");
        map.mark_remote("bmux.contexts");
        assert_eq!(map.get("bmux.contexts"), Some(ServiceLocation::Local));
    }

    #[test]
    fn mark_local_overrides_remote() {
        let map = ServiceLocationMap::new();
        map.mark_remote("bmux.contexts");
        map.mark_local("bmux.contexts");
        assert_eq!(map.get("bmux.contexts"), Some(ServiceLocation::Local));
    }

    #[test]
    fn get_missing_plugin_returns_none() {
        let map = ServiceLocationMap::new();
        assert_eq!(map.get("bmux.unknown"), None);
    }

    #[test]
    fn concurrent_mark_and_get_is_safe() {
        use std::thread;

        let map = Arc::new(ServiceLocationMap::new());
        let mut handles = Vec::new();
        for i in 0..8 {
            let map = Arc::clone(&map);
            handles.push(thread::spawn(move || {
                for j in 0..500 {
                    let id = format!("plugin-{}-{}", i, j % 16);
                    if j % 2 == 0 {
                        map.mark_local(&id);
                    } else {
                        map.mark_remote(&id);
                    }
                    let _ = map.get(&id);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(!map.is_empty());
    }

    #[test]
    fn clear_resets_all_entries() {
        let map = ServiceLocationMap::new();
        map.mark_local("a");
        map.mark_remote("b");
        assert_eq!(map.len(), 2);
        map.clear();
        assert!(map.is_empty());
    }

    #[test]
    fn global_service_locations_returns_same_instance() {
        let a = global_service_locations();
        let b = global_service_locations();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
