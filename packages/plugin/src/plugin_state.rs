//! Host-side shared state registry keyed by `TypeId`.
//!
//! `PluginStateRegistry` is a generic typemap. Plugins register an
//! `Arc<RwLock<T>>` during their `initialize` lifecycle; any code holding
//! a reference to the host can later look the state up by concrete type.
//!
//! This primitive lets plugins own domain state that the core runtime
//! (or other plugins) need to access directly, without introducing a
//! compile-time dependency from core crates on plugin crates. The
//! shared state type `T` itself must live in a neutral crate so both
//! owner and consumer can name it (for M4 this is
//! `bmux_plugin_domain_compat`).
//!
//! # Lifecycle
//!
//! 1. Plugin's `initialize` callback constructs its state and calls
//!    [`PluginStateRegistry::register`].
//! 2. After every foundational plugin has initialized, core code (or
//!    other plugins) may call [`PluginStateRegistry::get`] to obtain an
//!    `Arc<RwLock<T>>` handle.
//! 3. State remains alive for the lifetime of the registry (typically
//!    the server process). There is no deregister API — plugins are
//!    expected to live for the full server lifetime for foundational
//!    state roles.
//!
//! # Concurrency
//!
//! The registry itself is `RwLock`-protected. Individual state
//! entries are stored as opaque `Arc<dyn Any + Send + Sync>` and
//! downcast on lookup. Registering the same `TypeId` twice replaces
//! the earlier entry; callers should coordinate to avoid that.
//!
//! # Global accessor
//!
//! For convenience when wiring FFI bundled plugins whose `initialize`
//! callback does not carry a registry reference in its context,
//! [`global_registry`] returns a process-wide shared instance. Core
//! code constructing the server and plugins registering state both
//! reference the same singleton.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

/// Shared-state registry keyed by concrete type.
///
/// See module docs for the intended usage pattern.
#[derive(Default)]
pub struct PluginStateRegistry {
    entries: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl std::fmt::Debug for PluginStateRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.entries.read().map_or(0, |g| g.len());
        f.debug_struct("PluginStateRegistry")
            .field("entries", &count)
            .finish()
    }
}

impl PluginStateRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a state handle keyed by concrete type `T`.
    ///
    /// If a state of the same type was already registered it is
    /// replaced. Returns the previously-registered handle if any.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    pub fn register<T>(&self, state: &Arc<RwLock<T>>) -> Option<Arc<RwLock<T>>>
    where
        T: Any + Send + Sync,
    {
        let mut guard = self
            .entries
            .write()
            .expect("plugin state registry lock poisoned");
        let boxed: Arc<dyn Any + Send + Sync> = Arc::new(state.clone());
        let previous = guard.insert(TypeId::of::<Arc<RwLock<T>>>(), boxed);
        drop(guard);
        previous.and_then(|arc| {
            arc.downcast::<Arc<RwLock<T>>>()
                .ok()
                .map(|boxed| (*boxed).clone())
        })
    }

    /// Retrieve the registered state for concrete type `T` if any.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    #[must_use]
    pub fn get<T>(&self) -> Option<Arc<RwLock<T>>>
    where
        T: Any + Send + Sync,
    {
        let guard = self
            .entries
            .read()
            .expect("plugin state registry lock poisoned");
        let arc = guard.get(&TypeId::of::<Arc<RwLock<T>>>()).cloned();
        drop(guard);
        arc.and_then(|arc| arc.downcast::<Arc<RwLock<T>>>().ok())
            .map(|boxed| (*boxed).clone())
    }

    /// Retrieve the registered state for concrete type `T`, panicking
    /// if missing.
    ///
    /// Use this on core hot paths where the state is expected to have
    /// been registered during plugin initialization. Missing state
    /// indicates either a foundational plugin failed to load or a
    /// lookup occurred before initialization completed.
    ///
    /// # Panics
    ///
    /// Panics with a diagnostic message naming `T` if no state of
    /// type `T` has been registered.
    #[must_use]
    pub fn expect_state<T>(&self) -> Arc<RwLock<T>>
    where
        T: Any + Send + Sync,
    {
        self.get::<T>().unwrap_or_else(|| {
            panic!(
                "plugin state for `{}` not registered — foundational plugin missing or lookup too early",
                std::any::type_name::<T>()
            )
        })
    }

    /// Number of distinct state types currently registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().map_or(0, |guard| guard.len())
    }

    /// `true` when no state is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Process-wide shared registry instance.
///
/// Used by bundled plugins whose `activate` / `initialize` FFI path
/// does not carry a registry reference in its context. Server code
/// and plugin code both go through this singleton.
///
/// The returned `Arc` reference is cheap to clone.
#[must_use]
pub fn global_registry() -> Arc<PluginStateRegistry> {
    static GLOBAL: OnceLock<Arc<PluginStateRegistry>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| Arc::new(PluginStateRegistry::new()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::{PluginStateRegistry, global_registry};
    use std::sync::{Arc, RwLock};

    #[derive(Debug, Default, PartialEq, Eq)]
    struct Counter {
        value: u32,
    }

    #[derive(Debug, Default)]
    struct Other {
        flag: bool,
    }

    #[test]
    fn registers_and_retrieves_state() {
        let reg = PluginStateRegistry::new();
        let state = Arc::new(RwLock::new(Counter { value: 42 }));
        assert!(reg.register(&state).is_none());

        let retrieved = reg.get::<Counter>().expect("state not found");
        assert_eq!(retrieved.read().unwrap().value, 42);

        retrieved.write().unwrap().value = 99;
        let retrieved2 = reg.get::<Counter>().expect("state not found");
        assert_eq!(retrieved2.read().unwrap().value, 99);
    }

    #[test]
    fn re_register_replaces_entry_and_returns_previous() {
        let reg = PluginStateRegistry::new();
        let first = Arc::new(RwLock::new(Counter { value: 1 }));
        let second = Arc::new(RwLock::new(Counter { value: 2 }));

        assert!(reg.register(&first).is_none());
        let previous = reg.register(&second).expect("expected previous");
        assert_eq!(previous.read().unwrap().value, 1);
        assert_eq!(reg.get::<Counter>().unwrap().read().unwrap().value, 2);
    }

    #[test]
    fn different_types_are_independent() {
        let reg = PluginStateRegistry::new();
        let counter = Arc::new(RwLock::new(Counter { value: 7 }));
        let other = Arc::new(RwLock::new(Other { flag: true }));
        reg.register(&counter);
        reg.register(&other);
        assert_eq!(reg.get::<Counter>().unwrap().read().unwrap().value, 7);
        assert!(reg.get::<Other>().unwrap().read().unwrap().flag);
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn missing_state_returns_none() {
        let reg = PluginStateRegistry::new();
        assert!(reg.get::<Counter>().is_none());
    }

    #[test]
    #[should_panic(expected = "not registered")]
    fn expect_panics_when_missing() {
        let reg = PluginStateRegistry::new();
        let _ = reg.expect_state::<Counter>();
    }

    #[test]
    fn concurrent_access_is_safe() {
        use std::thread;

        let reg = Arc::new(PluginStateRegistry::new());
        let counter = Arc::new(RwLock::new(Counter { value: 0 }));
        reg.register(&counter);

        let mut handles = vec![];
        for _ in 0..8 {
            let reg = reg.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    let state = reg.get::<Counter>().unwrap();
                    state.write().unwrap().value += 1;
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let final_value = reg.get::<Counter>().unwrap().read().unwrap().value;
        assert_eq!(final_value, 8 * 1000);
    }

    /// A state type only used by the global-registry smoke test so it
    /// doesn't collide with other tests' `Counter`.
    #[derive(Debug, Default)]
    struct GlobalRegistrySentinel {
        value: u32,
    }

    #[test]
    fn global_registry_returns_same_instance() {
        let a = global_registry();
        let b = global_registry();
        // Same Arc target.
        assert!(Arc::ptr_eq(&a, &b));

        let sentinel = Arc::new(RwLock::new(GlobalRegistrySentinel { value: 13 }));
        a.register(&sentinel);

        let retrieved = b.get::<GlobalRegistrySentinel>().expect("not found");
        assert_eq!(retrieved.read().unwrap().value, 13);
    }
}
