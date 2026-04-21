//! `StatefulPlugin` — forward-looking trait declaring per-plugin snapshot
//! and restore hooks.
//!
//! Today the server owns a monolithic snapshot writer that knows about
//! every plugin domain (follow state, context state, session manager,
//! recording runtime, performance settings). That coupling is what the
//! Slice 12 core/plugin split exists to eliminate.
//!
//! This trait declares the surface a plugin needs to own its own
//! persistence: produce opaque bytes representing "everything this
//! plugin needs to restore itself" and consume those bytes on server
//! restart. The opaque-bytes shape keeps the orchestration (which
//! plugins get snapshotted, in what order, with what storage backend)
//! out of core. A dedicated snapshot-orchestration plugin (tracked as
//! Slice 13) walks every `StatefulPlugin` handle registered in the
//! plugin state registry and drives the save/restore cycle.
//!
//! Slice 12 **declares** the trait in the SDK so plugin-impl crates can
//! implement it alongside their state writer traits. Slice 12 does not
//! reroute the server's existing snapshot pipeline through it — that
//! happens in Slice 13.

use std::sync::Arc;

use crate::ident::PluginEventKind;

/// Errors returned by [`StatefulPlugin`] snapshot/restore hooks.
#[derive(Debug, thiserror::Error)]
pub enum StatefulPluginError {
    /// The plugin failed to serialize its state for snapshotting.
    #[error("stateful plugin '{plugin}' failed to snapshot: {details}")]
    SnapshotFailed { plugin: String, details: String },
    /// The plugin failed to deserialize or apply a previously-saved
    /// snapshot during restore.
    #[error("stateful plugin '{plugin}' failed to restore: {details}")]
    RestoreFailed { plugin: String, details: String },
    /// The snapshot payload was for a schema version this plugin does
    /// not understand. Callers should treat this as a non-fatal reset:
    /// discard the saved bytes and continue with default state.
    #[error(
        "stateful plugin '{plugin}' received unsupported snapshot version: {version} (expected one of {expected:?})"
    )]
    UnsupportedVersion {
        plugin: String,
        version: u32,
        expected: Vec<u32>,
    },
}

/// Result alias for stateful-plugin operations.
pub type StatefulPluginResult<T> = std::result::Result<T, StatefulPluginError>;

/// A versioned, opaque snapshot payload produced by a [`StatefulPlugin`].
///
/// `id` identifies the plugin surface being snapshotted — typically the
/// plugin id string (e.g. `"bmux.windows"`) so the orchestration plugin
/// can route a payload back to the same plugin on restore. `version`
/// lets the plugin evolve its internal format without breaking older
/// snapshots: on restore the plugin can branch on `version` or reject
/// unknown values with [`StatefulPluginError::UnsupportedVersion`].
///
/// `bytes` is arbitrary; serde-JSON, bmux-codec, or a bespoke binary
/// format are all valid. Core does not introspect it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatefulPluginSnapshot {
    pub id: PluginEventKind,
    pub version: u32,
    pub bytes: Vec<u8>,
}

impl StatefulPluginSnapshot {
    #[must_use]
    pub const fn new(id: PluginEventKind, version: u32, bytes: Vec<u8>) -> Self {
        Self { id, version, bytes }
    }

    /// Borrow the snapshot's opaque bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// A plugin that participates in server-level persistence.
///
/// Implementations snapshot their own state to opaque bytes and
/// restore from those bytes on server restart. Implementations must be
/// `Send + Sync` so the orchestration plugin can hold an
/// `Arc<dyn StatefulPlugin>` in the shared registry alongside the
/// plugin's other state handles.
///
/// Hooks are synchronous on the assumption that snapshot bodies are
/// small enough to produce without blocking — the plugin typically
/// clones an `Arc<Mutex<State>>`, serializes under the lock, and
/// returns. Long-running snapshot work should be done asynchronously
/// inside the plugin via an internal task and `snapshot()` should
/// publish the most recent cached result.
pub trait StatefulPlugin: Send + Sync {
    /// Stable identifier for this snapshot surface. The orchestration
    /// plugin keys saved payloads by this id and routes restores back
    /// to the plugin that declared it.
    fn id(&self) -> PluginEventKind;

    /// Produce a snapshot of the plugin's current state.
    ///
    /// # Errors
    ///
    /// Returns [`StatefulPluginError::SnapshotFailed`] if the plugin
    /// cannot serialize its state (lock poisoned, serde failure,
    /// referenced resource gone). The orchestration plugin treats this
    /// as a per-plugin failure and continues snapshotting other
    /// plugins.
    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot>;

    /// Restore the plugin's state from a previously saved snapshot.
    ///
    /// Implementations should be idempotent and tolerate being called
    /// before the plugin has received any live traffic. The default
    /// implementation ignores the payload — plugins opt in to restore
    /// behavior explicitly.
    ///
    /// # Errors
    ///
    /// Returns [`StatefulPluginError::RestoreFailed`] if the payload
    /// was corrupt or internally inconsistent with the plugin's
    /// current invariants, or [`StatefulPluginError::UnsupportedVersion`]
    /// if `snapshot.version` is a format the plugin does not
    /// understand.
    fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        let _ = snapshot;
        Ok(())
    }
}

/// Type-erased registry handle for a [`StatefulPlugin`].
///
/// The orchestration plugin discovers these by iterating a well-known
/// slot in the plugin state registry (`Vec<StatefulPluginHandle>`) and
/// drives snapshot/restore across all of them.
#[derive(Clone)]
pub struct StatefulPluginHandle(pub Arc<dyn StatefulPlugin>);

impl StatefulPluginHandle {
    #[must_use]
    pub fn new<P: StatefulPlugin + 'static>(plugin: P) -> Self {
        Self(Arc::new(plugin))
    }

    #[must_use]
    pub fn from_arc(plugin: Arc<dyn StatefulPlugin>) -> Self {
        Self(plugin)
    }

    /// Borrow the underlying trait object.
    #[must_use]
    pub fn as_dyn(&self) -> &(dyn StatefulPlugin + 'static) {
        self.0.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const TEST_ID: PluginEventKind = PluginEventKind::from_static("bmux.test/stateful");

    struct Counter {
        value: Mutex<u32>,
    }

    impl StatefulPlugin for Counter {
        fn id(&self) -> PluginEventKind {
            TEST_ID
        }

        fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
            let value = *self
                .value
                .lock()
                .map_err(|_| StatefulPluginError::SnapshotFailed {
                    plugin: TEST_ID.as_str().to_string(),
                    details: "poisoned".into(),
                })?;
            Ok(StatefulPluginSnapshot::new(
                TEST_ID,
                1,
                value.to_le_bytes().to_vec(),
            ))
        }

        fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
            if snapshot.version != 1 {
                return Err(StatefulPluginError::UnsupportedVersion {
                    plugin: TEST_ID.as_str().to_string(),
                    version: snapshot.version,
                    expected: vec![1],
                });
            }
            let bytes: [u8; 4] =
                snapshot
                    .bytes
                    .try_into()
                    .map_err(|_| StatefulPluginError::RestoreFailed {
                        plugin: TEST_ID.as_str().to_string(),
                        details: "expected 4 bytes".into(),
                    })?;
            *self
                .value
                .lock()
                .map_err(|_| StatefulPluginError::RestoreFailed {
                    plugin: TEST_ID.as_str().to_string(),
                    details: "poisoned".into(),
                })? = u32::from_le_bytes(bytes);
            Ok(())
        }
    }

    #[test]
    fn snapshot_round_trips_through_restore() {
        let counter = Counter {
            value: Mutex::new(42),
        };
        let snap = counter.snapshot().expect("snapshot");
        assert_eq!(snap.id, TEST_ID);
        assert_eq!(snap.version, 1);

        let target = Counter {
            value: Mutex::new(0),
        };
        target.restore_snapshot(snap).expect("restore");
        assert_eq!(*target.value.lock().expect("lock"), 42);
    }

    #[test]
    fn unknown_version_is_reported() {
        let counter = Counter {
            value: Mutex::new(0),
        };
        let bad = StatefulPluginSnapshot::new(TEST_ID, 99, vec![0, 0, 0, 0]);
        let err = counter.restore_snapshot(bad).unwrap_err();
        assert!(matches!(
            err,
            StatefulPluginError::UnsupportedVersion { version: 99, .. }
        ));
    }

    #[test]
    fn default_restore_is_no_op() {
        struct EmptyStateful;
        impl StatefulPlugin for EmptyStateful {
            fn id(&self) -> PluginEventKind {
                TEST_ID
            }
            fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
                Ok(StatefulPluginSnapshot::new(TEST_ID, 1, Vec::new()))
            }
        }

        let plugin = EmptyStateful;
        let snap = plugin.snapshot().expect("snapshot");
        plugin
            .restore_snapshot(snap)
            .expect("default restore succeeds");
    }

    #[test]
    fn handle_as_dyn_preserves_trait_object_identity() {
        let handle = StatefulPluginHandle::new(Counter {
            value: Mutex::new(7),
        });
        let snap = handle.as_dyn().snapshot().expect("snapshot via handle");
        assert_eq!(snap.id, TEST_ID);
    }
}
