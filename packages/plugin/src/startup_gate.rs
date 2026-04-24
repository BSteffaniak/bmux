//! Plugin-registered attach startup ready gates.
//!
//! A startup ready gate is a plugin-declared `(plugin_id, signal)`
//! pair plus a per-gate timeout. The attach runtime polls every
//! registered gate before kicking off its first render pass, so any
//! plugin that needs its initial async work (e.g. publishing a scene,
//! priming a cache) to complete before user-visible behavior starts
//! can self-register a gate instead of core attaching runtime
//! hardcoding plugin-specific waits.
//!
//! # Lifecycle
//!
//! 1. Plugin's activation code calls [`register_startup_ready_gate`]
//!    with its own plugin id, a signal name already declared in the
//!    plugin manifest, and a timeout.
//! 2. Attach runtime iterates [`registered_startup_ready_gates`] on
//!    startup; for each gate, it calls the shared `ReadyTracker` to
//!    await the signal, honoring the per-gate timeout.
//! 3. When every gate has fired (or timed out), the first render
//!    pass begins.
//!
//! Gates are additive: plugin authors can register multiple gates
//! across multiple signals, and several plugins can gate against
//! different signals concurrently.

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

/// A single registered startup gate. Plugins construct one per signal
/// they want core attach startup to wait on.
#[derive(Debug, Clone)]
pub struct StartupReadyGate {
    /// Plugin id that owns the signal. Matches `PluginId::as_str()`.
    pub plugin_id: String,
    /// Signal name declared in the plugin manifest.
    pub signal: String,
    /// Maximum time the attach runtime should wait for this signal
    /// before giving up and continuing (logging a warning). Callers
    /// should pick a value generous enough to cover slow cold starts
    /// but short enough that a missing plugin doesn't hang startup.
    pub timeout: Duration,
}

/// Thread-safe registry of startup ready gates.
#[derive(Default)]
pub struct StartupReadyGateRegistry {
    entries: RwLock<Vec<StartupReadyGate>>,
}

impl std::fmt::Debug for StartupReadyGateRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.entries.read().map_or(0, |g| g.len());
        f.debug_struct("StartupReadyGateRegistry")
            .field("entries", &count)
            .finish()
    }
}

impl StartupReadyGateRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a gate. Duplicate `(plugin_id, signal)` pairs with
    /// different timeouts all wait; callers coordinate timeouts if
    /// that matters.
    pub fn register(&self, gate: StartupReadyGate) {
        if let Ok(mut guard) = self.entries.write() {
            guard.push(gate);
        }
    }

    /// Snapshot of currently-registered gates.
    #[must_use]
    pub fn snapshot(&self) -> Vec<StartupReadyGate> {
        self.entries.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Number of registered gates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().map_or(0, |g| g.len())
    }

    /// `true` when no gate is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Process-wide shared gate registry.
#[must_use]
pub fn global_startup_ready_gate_registry() -> Arc<StartupReadyGateRegistry> {
    static GLOBAL: OnceLock<Arc<StartupReadyGateRegistry>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| Arc::new(StartupReadyGateRegistry::new()))
        .clone()
}

/// Register a startup gate on the process-wide registry.
pub fn register_startup_ready_gate(plugin_id: &str, signal: &str, timeout: Duration) {
    global_startup_ready_gate_registry().register(StartupReadyGate {
        plugin_id: plugin_id.to_string(),
        signal: signal.to_string(),
        timeout,
    });
}

/// Snapshot of currently-registered startup gates.
#[must_use]
pub fn registered_startup_ready_gates() -> Vec<StartupReadyGate> {
    global_startup_ready_gate_registry().snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tracks_entries() {
        let registry = StartupReadyGateRegistry::new();
        assert!(registry.is_empty());
        registry.register(StartupReadyGate {
            plugin_id: "plug.a".to_string(),
            signal: "ready".to_string(),
            timeout: Duration::from_millis(500),
        });
        registry.register(StartupReadyGate {
            plugin_id: "plug.b".to_string(),
            signal: "warm".to_string(),
            timeout: Duration::from_millis(100),
        });
        assert_eq!(registry.len(), 2);
        let snap = registry.snapshot();
        assert_eq!(snap[0].plugin_id, "plug.a");
        assert_eq!(snap[1].signal, "warm");
    }
}
