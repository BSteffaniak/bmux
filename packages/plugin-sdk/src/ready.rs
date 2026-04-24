//! Plugin ready signals.
//!
//! A plugin may declare one or more named readiness signals in its manifest.
//! During activation the plugin performs any async initialization it needs
//! (subscribing to events, publishing an initial snapshot, warming caches,
//! etc.) and marks each signal ready once that specific capability is
//! available.
//!
//! Host subsystems that depend on a signal wait for it before running
//! dependent logic (e.g. the attach runtime gating its first render
//! on a plugin's `scene-published` ready signal). Readiness is a
//! cooperative lifecycle concept owned by the plugin; the host only
//! observes and sequences against it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// Declaration of a named readiness signal.
///
/// Parsed from the plugin manifest's `[[ready_signals]]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadySignalDecl {
    /// Stable identifier for the signal, referenced by dependent
    /// subsystems (e.g. `"scene-published"`). Matched case-sensitively.
    pub name: String,
    /// Human-readable explanation of what "ready" means for this signal.
    /// Informational only.
    #[serde(default)]
    pub description: Option<String>,
}

/// Runtime status of a single ready signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyStatus {
    /// The plugin has declared the signal but not yet marked it ready.
    Pending,
    /// The plugin has marked the signal ready.
    Ready,
}

/// Shared readiness tracker used by a plugin host to observe signal
/// transitions across all loaded plugins.
///
/// The tracker is cloneable and thread-safe; one instance is handed to
/// plugins during activation so they can call
/// [`Self::mark_ready`] when their init work completes, and another
/// handle is kept by the host to observe signal state via
/// [`Self::is_ready`] / [`Self::await_ready`].
#[derive(Debug, Clone, Default)]
pub struct ReadyTracker {
    inner: Arc<RwLock<BTreeMap<(String, String), ReadyStatus>>>,
}

impl ReadyTracker {
    /// Construct a fresh tracker with no declared signals.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin's declared signals as [`ReadyStatus::Pending`].
    ///
    /// Idempotent — signals already present are left at their current
    /// status. Typically called by the host once per plugin during
    /// plugin registration.
    pub fn declare(&self, plugin_id: &str, signals: &[ReadySignalDecl]) {
        if signals.is_empty() {
            return;
        }
        if let Ok(mut map) = self.inner.write() {
            for signal in signals {
                map.entry((plugin_id.to_string(), signal.name.clone()))
                    .or_insert(ReadyStatus::Pending);
            }
        }
    }

    /// Mark a signal ready. Idempotent; calling multiple times has no
    /// effect.
    pub fn mark_ready(&self, plugin_id: &str, signal_name: &str) {
        if let Ok(mut map) = self.inner.write() {
            map.insert(
                (plugin_id.to_string(), signal_name.to_string()),
                ReadyStatus::Ready,
            );
        }
    }

    /// Whether the signal has been marked ready.
    ///
    /// Returns `false` if the signal is unknown or still pending.
    #[must_use]
    pub fn is_ready(&self, plugin_id: &str, signal_name: &str) -> bool {
        self.inner.read().ok().is_some_and(|map| {
            map.get(&(plugin_id.to_string(), signal_name.to_string()))
                .copied()
                .is_some_and(|status| status == ReadyStatus::Ready)
        })
    }

    /// Get the current status of a signal, or `None` if the signal has
    /// not been declared.
    #[must_use]
    pub fn status(&self, plugin_id: &str, signal_name: &str) -> Option<ReadyStatus> {
        self.inner.read().ok().and_then(|map| {
            map.get(&(plugin_id.to_string(), signal_name.to_string()))
                .copied()
        })
    }

    /// Block (polling with a short sleep) until a signal is ready, or
    /// until `timeout` elapses.
    ///
    /// Returns `true` if the signal became ready, `false` on timeout.
    ///
    /// Intended for test harnesses and startup-sequencing scenarios;
    /// async-capable hosts can build notification on top of
    /// [`Self::is_ready`] if preferred.
    #[must_use]
    pub fn await_ready(
        &self,
        plugin_id: &str,
        signal_name: &str,
        timeout: std::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.is_ready(plugin_id, signal_name) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Snapshot of all declared signals and their current status.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<(String, String), ReadyStatus> {
        self.inner.read().map(|map| map.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declare_and_mark_ready_round_trip() {
        let tracker = ReadyTracker::new();
        let decls = vec![
            ReadySignalDecl {
                name: "first".into(),
                description: None,
            },
            ReadySignalDecl {
                name: "second".into(),
                description: Some("second signal".into()),
            },
        ];
        tracker.declare("plug.a", &decls);
        assert_eq!(
            tracker.status("plug.a", "first"),
            Some(ReadyStatus::Pending)
        );
        assert!(!tracker.is_ready("plug.a", "first"));
        tracker.mark_ready("plug.a", "first");
        assert!(tracker.is_ready("plug.a", "first"));
        assert!(!tracker.is_ready("plug.a", "second"));
    }

    #[test]
    fn await_ready_returns_true_when_signal_fires() {
        let tracker = ReadyTracker::new();
        tracker.declare(
            "plug.a",
            &[ReadySignalDecl {
                name: "ready".into(),
                description: None,
            }],
        );
        let tracker_bg = tracker.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(25));
            tracker_bg.mark_ready("plug.a", "ready");
        });
        assert!(tracker.await_ready("plug.a", "ready", std::time::Duration::from_millis(500)));
    }

    #[test]
    fn await_ready_times_out_when_signal_never_fires() {
        let tracker = ReadyTracker::new();
        tracker.declare(
            "plug.a",
            &[ReadySignalDecl {
                name: "stuck".into(),
                description: None,
            }],
        );
        assert!(!tracker.await_ready("plug.a", "stuck", std::time::Duration::from_millis(30)));
    }

    #[test]
    fn status_returns_none_for_unknown_signal() {
        let tracker = ReadyTracker::new();
        assert_eq!(tracker.status("plug.a", "missing"), None);
    }
}
