//! Retained plugin-owned surfaces independent of pane surfaces.
//!
//! Plugins publish complete owner-scoped snapshots. The attach host resolves
//! layout targets, converts the retained paint operations into compositor
//! surfaces, and never calls the plugin from the frame path.

use crate::layout::PluginLayoutId;
use crate::{ExtensionRect, RenderOp};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{OnceLock, RwLock};
use uuid::Uuid;

/// Stable namespaced identity for an independently composed surface.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginSurfaceId {
    pub owner_plugin_id: String,
    pub local_id: String,
    /// Stable compositor identity supplied once by the owning plugin.
    pub retained_id: Uuid,
}

impl PluginSurfaceId {
    #[must_use]
    pub fn new(
        owner_plugin_id: impl Into<String>,
        local_id: impl Into<String>,
        retained_id: Uuid,
    ) -> Self {
        Self {
            owner_plugin_id: owner_plugin_id.into(),
            local_id: local_id.into(),
            retained_id,
        }
    }
}

/// Geometry source for a plugin-owned surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSurfaceTarget {
    /// Follow one allocation from the generic plugin layout resolver.
    Layout(PluginLayoutId),
    /// Use explicit terminal-cell geometry, normally for overlays.
    Explicit(ExtensionRect),
}

/// Stable namespaced identity for one input region.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginSurfaceRegionId {
    pub owner_plugin_id: String,
    pub surface_local_id: String,
    pub region_local_id: String,
}

impl PluginSurfaceRegionId {
    #[must_use]
    pub fn new(surface_id: &PluginSurfaceId, region_local_id: impl Into<String>) -> Self {
        Self {
            owner_plugin_id: surface_id.owner_plugin_id.clone(),
            surface_local_id: surface_id.local_id.clone(),
            region_local_id: region_local_id.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSurfaceCursor {
    Default,
    Pointer,
    Text,
    Crosshair,
    ResizeHorizontal,
    ResizeVertical,
    Hidden,
}

/// Stable owner-local input region within one retained surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSurfaceRegion {
    pub local_id: String,
    pub rect: ExtensionRect,
    pub focusable: bool,
    pub cursor: PluginSurfaceCursor,
}

/// Complete retained visual state for one independent surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSurface {
    pub id: PluginSurfaceId,
    /// Must match the containing owner snapshot revision.
    pub revision: u64,
    pub target: PluginSurfaceTarget,
    /// Optional clip rectangle local to the resolved surface bounds.
    pub clip_rect: Option<ExtensionRect>,
    /// Input regions local to the resolved surface bounds.
    pub interactive_regions: Vec<PluginSurfaceRegion>,
    pub accepts_input: bool,
    pub layer: i16,
    pub z: i32,
    pub opaque: bool,
    pub visible: bool,
    pub ops: Vec<RenderOp>,
}

/// Atomic owner-scoped replacement of independent surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSurfaceSnapshot {
    pub revision: u64,
    pub surfaces: Vec<PluginSurface>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSurfacePublishOutcome {
    Applied,
    Unchanged,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSurfacePublishError {
    EmptyOwner,
    TooManySurfaces {
        count: usize,
        maximum: usize,
    },
    TooManyRetainedItems {
        id: PluginSurfaceId,
        count: usize,
        maximum: usize,
    },
    TextTooLong {
        id: PluginSurfaceId,
        bytes: usize,
        maximum: usize,
    },
    SnapshotTooLarge {
        bytes: usize,
        maximum: usize,
    },
    OwnerMismatch {
        id: PluginSurfaceId,
    },
    DuplicateId {
        id: PluginSurfaceId,
    },
    DuplicateRetainedId {
        retained_id: Uuid,
    },
    EmptyRegionId {
        id: PluginSurfaceId,
    },
    DuplicateRegionId {
        id: PluginSurfaceId,
        local_id: String,
    },
    RegionsWithoutInput {
        id: PluginSurfaceId,
    },
    SurfaceRevisionMismatch {
        id: PluginSurfaceId,
        surface_revision: u64,
        snapshot_revision: u64,
    },
    ForeignLayoutTarget {
        id: PluginLayoutId,
    },
    ConflictingRevision,
}

/// Thread-safe retained surface snapshots keyed by plugin owner.
#[derive(Debug)]
pub struct PluginSurfaceRegistry {
    maximum_surfaces_per_owner: usize,
    maximum_retained_items_per_surface: usize,
    maximum_text_bytes: usize,
    maximum_snapshot_bytes: usize,
    snapshots: RwLock<BTreeMap<String, PluginSurfaceSnapshot>>,
    revision_tx: tokio::sync::watch::Sender<u64>,
}

impl PluginSurfaceRegistry {
    #[must_use]
    pub fn new(maximum_surfaces_per_owner: usize) -> Self {
        Self::with_limits(
            maximum_surfaces_per_owner,
            4_096,
            64 * 1_024,
            4 * 1_024 * 1_024,
        )
    }

    #[must_use]
    pub fn with_limits(
        maximum_surfaces_per_owner: usize,
        maximum_retained_items_per_surface: usize,
        maximum_text_bytes: usize,
        maximum_snapshot_bytes: usize,
    ) -> Self {
        let (revision_tx, _) = tokio::sync::watch::channel(0);
        Self {
            maximum_surfaces_per_owner,
            maximum_retained_items_per_surface,
            maximum_text_bytes,
            maximum_snapshot_bytes,
            snapshots: RwLock::new(BTreeMap::new()),
            revision_tx,
        }
    }

    /// Replace all retained surfaces owned by `owner_plugin_id`.
    ///
    /// # Errors
    ///
    /// Returns ownership, identity, resource-limit, or revision validation
    /// failures. A poisoned lock is reported as a conflicting revision.
    pub fn publish(
        &self,
        owner_plugin_id: &str,
        snapshot: PluginSurfaceSnapshot,
    ) -> Result<PluginSurfacePublishOutcome, PluginSurfacePublishError> {
        validate_snapshot(
            owner_plugin_id,
            &snapshot,
            self.maximum_surfaces_per_owner,
            self.maximum_retained_items_per_surface,
            self.maximum_text_bytes,
            self.maximum_snapshot_bytes,
        )?;
        let mut snapshots = self
            .snapshots
            .write()
            .map_err(|_| PluginSurfacePublishError::ConflictingRevision)?;
        if let Some(current) = snapshots.get(owner_plugin_id) {
            if snapshot.revision < current.revision {
                return Ok(PluginSurfacePublishOutcome::Stale);
            }
            if snapshot.revision == current.revision {
                return if snapshot == *current {
                    Ok(PluginSurfacePublishOutcome::Unchanged)
                } else {
                    Err(PluginSurfacePublishError::ConflictingRevision)
                };
            }
        }
        let retained_ids = snapshots
            .iter()
            .filter(|(owner, _)| owner.as_str() != owner_plugin_id)
            .flat_map(|(_, current)| {
                current
                    .surfaces
                    .iter()
                    .map(|surface| surface.id.retained_id)
            })
            .collect::<BTreeSet<_>>();
        if let Some(retained_id) = snapshot
            .surfaces
            .iter()
            .map(|surface| surface.id.retained_id)
            .find(|retained_id| retained_ids.contains(retained_id))
        {
            return Err(PluginSurfacePublishError::DuplicateRetainedId { retained_id });
        }
        snapshots.insert(owner_plugin_id.to_owned(), snapshot);
        drop(snapshots);
        self.notify_changed();
        Ok(PluginSurfacePublishOutcome::Applied)
    }

    pub fn remove_owner(&self, owner_plugin_id: &str) -> bool {
        let removed = self
            .snapshots
            .write()
            .ok()
            .and_then(|mut snapshots| snapshots.remove(owner_plugin_id))
            .is_some();
        if removed {
            self.notify_changed();
        }
        removed
    }

    #[must_use]
    pub fn surfaces(&self) -> Vec<PluginSurface> {
        self.snapshots
            .read()
            .map(|snapshots| {
                snapshots
                    .values()
                    .flat_map(|snapshot| snapshot.surfaces.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revision_tx.subscribe()
    }

    fn notify_changed(&self) {
        self.revision_tx.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }
}

fn validate_snapshot(
    owner_plugin_id: &str,
    snapshot: &PluginSurfaceSnapshot,
    maximum_surfaces_per_owner: usize,
    maximum_retained_items_per_surface: usize,
    maximum_text_bytes: usize,
    maximum_snapshot_bytes: usize,
) -> Result<(), PluginSurfacePublishError> {
    if owner_plugin_id.is_empty() {
        return Err(PluginSurfacePublishError::EmptyOwner);
    }
    if snapshot.surfaces.len() > maximum_surfaces_per_owner {
        return Err(PluginSurfacePublishError::TooManySurfaces {
            count: snapshot.surfaces.len(),
            maximum: maximum_surfaces_per_owner,
        });
    }
    let mut ids = BTreeSet::new();
    let mut retained_ids = BTreeSet::new();
    let mut snapshot_bytes = 0_usize;
    for surface in &snapshot.surfaces {
        if surface.revision != snapshot.revision {
            return Err(PluginSurfacePublishError::SurfaceRevisionMismatch {
                id: surface.id.clone(),
                surface_revision: surface.revision,
                snapshot_revision: snapshot.revision,
            });
        }
        if surface.ops.len() > maximum_retained_items_per_surface {
            return Err(PluginSurfacePublishError::TooManyRetainedItems {
                id: surface.id.clone(),
                count: surface.ops.len(),
                maximum: maximum_retained_items_per_surface,
            });
        }
        snapshot_bytes = snapshot_bytes.saturating_add(std::mem::size_of::<PluginSurface>());
        for op in &surface.ops {
            let text_bytes = render_op_text_bytes(op);
            if text_bytes > maximum_text_bytes {
                return Err(PluginSurfacePublishError::TextTooLong {
                    id: surface.id.clone(),
                    bytes: text_bytes,
                    maximum: maximum_text_bytes,
                });
            }
            snapshot_bytes = snapshot_bytes
                .saturating_add(std::mem::size_of::<RenderOp>())
                .saturating_add(text_bytes);
        }
        if snapshot_bytes > maximum_snapshot_bytes {
            return Err(PluginSurfacePublishError::SnapshotTooLarge {
                bytes: snapshot_bytes,
                maximum: maximum_snapshot_bytes,
            });
        }
        if surface.id.owner_plugin_id != owner_plugin_id {
            return Err(PluginSurfacePublishError::OwnerMismatch {
                id: surface.id.clone(),
            });
        }
        if !ids.insert(surface.id.clone()) {
            return Err(PluginSurfacePublishError::DuplicateId {
                id: surface.id.clone(),
            });
        }
        if !retained_ids.insert(surface.id.retained_id) {
            return Err(PluginSurfacePublishError::DuplicateRetainedId {
                retained_id: surface.id.retained_id,
            });
        }
        if surface.accepts_input {
            let mut region_ids = BTreeSet::new();
            for region in &surface.interactive_regions {
                if region.local_id.is_empty() {
                    return Err(PluginSurfacePublishError::EmptyRegionId {
                        id: surface.id.clone(),
                    });
                }
                if !region_ids.insert(region.local_id.clone()) {
                    return Err(PluginSurfacePublishError::DuplicateRegionId {
                        id: surface.id.clone(),
                        local_id: region.local_id.clone(),
                    });
                }
            }
        } else if !surface.interactive_regions.is_empty() {
            return Err(PluginSurfacePublishError::RegionsWithoutInput {
                id: surface.id.clone(),
            });
        }
        if let PluginSurfaceTarget::Layout(layout_id) = &surface.target
            && layout_id.owner_plugin_id != owner_plugin_id
        {
            return Err(PluginSurfacePublishError::ForeignLayoutTarget {
                id: layout_id.clone(),
            });
        }
    }
    Ok(())
}

fn render_op_text_bytes(op: &RenderOp) -> usize {
    match op {
        RenderOp::TextRun { text, .. } => text.len(),
        RenderOp::StyledText { spans, .. } => spans.iter().map(|span| span.text.len()).sum(),
        RenderOp::CellGrid { rows, .. } => rows
            .iter()
            .flatten()
            .filter_map(|cell| cell.ch)
            .map(char::len_utf8)
            .sum(),
        RenderOp::ClearRect { .. }
        | RenderOp::EraseRowSegment { .. }
        | RenderOp::FillRect { .. }
        | RenderOp::Border { .. } => 0,
    }
}

static GLOBAL_PLUGIN_SURFACE_REGISTRY: OnceLock<PluginSurfaceRegistry> = OnceLock::new();

#[must_use]
pub fn global_plugin_surface_registry() -> &'static PluginSurfaceRegistry {
    GLOBAL_PLUGIN_SURFACE_REGISTRY.get_or_init(|| PluginSurfaceRegistry::new(64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RenderStyle;

    fn surface(owner: &str, local: &str, retained_id: Uuid) -> PluginSurface {
        PluginSurface {
            id: PluginSurfaceId::new(owner, local, retained_id),
            revision: 1,
            target: PluginSurfaceTarget::Explicit(ExtensionRect::new(1, 2, 3, 4)),
            clip_rect: None,
            interactive_regions: Vec::new(),
            accepts_input: false,
            layer: 1,
            z: 2,
            opaque: true,
            visible: true,
            ops: vec![RenderOp::text_run(1, 2, "x", RenderStyle::new())],
        }
    }

    #[test]
    fn registry_replaces_and_removes_owner_surfaces() {
        let registry = PluginSurfaceRegistry::new(2);
        let id = Uuid::from_u128(1);
        let mut changes = registry.subscribe();
        let first = PluginSurfaceSnapshot {
            revision: 1,
            surfaces: vec![surface("owner", "main", id)],
        };
        assert_eq!(
            registry.publish("owner", first.clone()),
            Ok(PluginSurfacePublishOutcome::Applied)
        );
        assert!(changes.has_changed().unwrap());
        changes.borrow_and_update();
        assert_eq!(
            registry.publish("owner", first),
            Ok(PluginSurfacePublishOutcome::Unchanged)
        );
        assert!(!changes.has_changed().unwrap());
        assert!(registry.remove_owner("owner"));
        assert!(registry.surfaces().is_empty());
        assert!(changes.has_changed().unwrap());
    }

    #[test]
    fn registry_rejects_surface_revision_mismatch() {
        let registry = PluginSurfaceRegistry::new(2);
        let mut mismatched = surface("owner", "main", Uuid::from_u128(12));
        mismatched.revision = 2;
        assert!(matches!(
            registry.publish(
                "owner",
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces: vec![mismatched],
                }
            ),
            Err(PluginSurfacePublishError::SurfaceRevisionMismatch { .. })
        ));
    }

    #[test]
    fn registry_rejects_invalid_region_identity_and_disabled_input_regions() {
        let retained_id = Uuid::from_u128(10);
        let mut duplicate = surface("owner", "regions", retained_id);
        duplicate.accepts_input = true;
        duplicate.interactive_regions = vec![
            PluginSurfaceRegion {
                local_id: "same".to_owned(),
                rect: ExtensionRect::new(0, 0, 1, 1),
                focusable: false,
                cursor: PluginSurfaceCursor::Default,
            },
            PluginSurfaceRegion {
                local_id: "same".to_owned(),
                rect: ExtensionRect::new(1, 0, 1, 1),
                focusable: false,
                cursor: PluginSurfaceCursor::Default,
            },
        ];
        let registry = PluginSurfaceRegistry::new(2);
        assert!(matches!(
            registry.publish(
                "owner",
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces: vec![duplicate],
                }
            ),
            Err(PluginSurfacePublishError::DuplicateRegionId { .. })
        ));

        let mut disabled = surface("owner", "disabled", retained_id);
        disabled.interactive_regions = vec![PluginSurfaceRegion {
            local_id: "region".to_owned(),
            rect: ExtensionRect::new(0, 0, 1, 1),
            focusable: false,
            cursor: PluginSurfaceCursor::Default,
        }];
        assert!(matches!(
            registry.publish(
                "owner",
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces: vec![disabled],
                }
            ),
            Err(PluginSurfacePublishError::RegionsWithoutInput { .. })
        ));
    }

    #[test]
    fn namespaced_region_identity_includes_owner_surface_and_region() {
        let surface_id = PluginSurfaceId::new("owner", "surface", Uuid::from_u128(11));
        assert_eq!(
            PluginSurfaceRegionId::new(&surface_id, "region"),
            PluginSurfaceRegionId {
                owner_plugin_id: "owner".to_owned(),
                surface_local_id: "surface".to_owned(),
                region_local_id: "region".to_owned(),
            }
        );
    }

    #[test]
    fn registry_enforces_retained_item_text_and_snapshot_limits() {
        let retained_id = Uuid::from_u128(9);
        let mut item_limited = surface("owner", "items", retained_id);
        item_limited.ops.push(RenderOp::clear_rect(
            ExtensionRect::new(0, 0, 1, 1),
            RenderStyle::new(),
        ));
        let registry = PluginSurfaceRegistry::with_limits(2, 1, 8, usize::MAX);
        assert!(matches!(
            registry.publish(
                "owner",
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces: vec![item_limited],
                }
            ),
            Err(PluginSurfacePublishError::TooManyRetainedItems { .. })
        ));

        let mut text_limited = surface("owner", "text", retained_id);
        text_limited.ops = vec![RenderOp::text_run(0, 0, "too long!", RenderStyle::new())];
        assert!(matches!(
            registry.publish(
                "owner",
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces: vec![text_limited],
                }
            ),
            Err(PluginSurfacePublishError::TextTooLong { .. })
        ));

        let registry = PluginSurfaceRegistry::with_limits(2, 2, 16, 1);
        assert!(matches!(
            registry.publish(
                "owner",
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces: vec![surface("owner", "bytes", retained_id)],
                }
            ),
            Err(PluginSurfacePublishError::SnapshotTooLarge { .. })
        ));
    }

    #[test]
    fn registry_enforces_owner_target_limit_and_compositor_identity() {
        let registry = PluginSurfaceRegistry::new(1);
        let shared_id = Uuid::from_u128(7);
        assert!(matches!(
            registry.publish(
                "owner",
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces: vec![surface("other", "main", shared_id)],
                }
            ),
            Err(PluginSurfacePublishError::OwnerMismatch { .. })
        ));
        let mut foreign = surface("owner", "main", shared_id);
        foreign.target = PluginSurfaceTarget::Layout(PluginLayoutId::new("other", "region"));
        assert!(matches!(
            registry.publish(
                "owner",
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces: vec![foreign],
                }
            ),
            Err(PluginSurfacePublishError::ForeignLayoutTarget { .. })
        ));
        assert_eq!(
            registry.publish(
                "owner",
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces: vec![surface("owner", "main", shared_id)],
                }
            ),
            Ok(PluginSurfacePublishOutcome::Applied)
        );
        assert!(matches!(
            registry.publish(
                "second",
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces: vec![surface("second", "main", shared_id)],
                }
            ),
            Err(PluginSurfacePublishError::DuplicateRetainedId { .. })
        ));
    }
}
