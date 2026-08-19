//! Deterministic layout of plugin-owned regions within a host viewport.
//!
//! Layout requests describe geometry only. Product meaning, rendering, and
//! interaction remain with the owning plugin. The resolver is pure so hosts can
//! validate and cache a layout revision without invoking plugins on a render
//! pass.

use crate::ExtensionRect;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{OnceLock, RwLock};

/// Stable identity of one plugin-owned layout request.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginLayoutId {
    pub owner_plugin_id: String,
    pub local_id: String,
}

impl PluginLayoutId {
    #[must_use]
    pub fn new(owner_plugin_id: impl Into<String>, local_id: impl Into<String>) -> Self {
        Self {
            owner_plugin_id: owner_plugin_id.into(),
            local_id: local_id.into(),
        }
    }
}

/// Viewport edge from which a region is split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutEdge {
    Top,
    Right,
    Bottom,
    Left,
}

/// Extent of a split along its edge's axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutExtent {
    Cells(u16),
    Percent(u8),
}

impl LayoutExtent {
    fn resolve(self, available: u16) -> u16 {
        match self {
            Self::Cells(cells) => cells.min(available),
            Self::Percent(percent) => {
                let percent = percent.min(100);
                let scaled = u32::from(available) * u32::from(percent) / 100;
                u16::try_from(scaled).unwrap_or(available)
            }
        }
    }
}

/// How a request participates in allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutOperation {
    /// Allocate an edge region and remove it from the remaining region.
    Split {
        edge: LayoutEdge,
        extent: LayoutExtent,
    },
    /// Allocate the current remaining region without consuming it.
    Overlay,
}

/// One independently owned layout request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLayoutRequest {
    pub id: PluginLayoutId,
    pub order: i32,
    pub operation: LayoutOperation,
}

/// Resolved region for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLayoutAllocation {
    pub id: PluginLayoutId,
    pub rect: ExtensionRect,
}

/// Complete deterministic resolution of a request set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLayoutResolution {
    pub allocations: Vec<PluginLayoutAllocation>,
    pub remaining: ExtensionRect,
}

/// Retained layout intent published by one plugin owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLayoutSnapshot {
    pub revision: u64,
    pub requests: Vec<PluginLayoutRequest>,
}

/// Outcome of replacing one owner's retained layout intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginLayoutPublishOutcome {
    Applied,
    Unchanged,
    Stale,
}

/// Validation failure for retained layout publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginLayoutPublishError {
    EmptyOwner,
    TooManyRequests { count: usize, maximum: usize },
    OwnerMismatch { id: PluginLayoutId },
    DuplicateId { id: PluginLayoutId },
    ConflictingRevision,
}

/// Thread-safe retained snapshots keyed by plugin owner.
///
/// Publication validates and clones only on update. Render/layout consumers can
/// snapshot the flattened requests after an event wakes them; no plugin call is
/// required on the frame path.
#[derive(Debug)]
pub struct PluginLayoutRegistry {
    maximum_requests_per_owner: usize,
    snapshots: RwLock<BTreeMap<String, PluginLayoutSnapshot>>,
}

impl PluginLayoutRegistry {
    #[must_use]
    pub const fn new(maximum_requests_per_owner: usize) -> Self {
        Self {
            maximum_requests_per_owner,
            snapshots: RwLock::new(BTreeMap::new()),
        }
    }

    /// Replace all retained requests owned by `owner_plugin_id`.
    ///
    /// # Errors
    ///
    /// Returns validation or conflicting-revision errors. A poisoned registry
    /// lock is reported as [`PluginLayoutPublishError::ConflictingRevision`]
    /// rather than panicking on a plugin publication path.
    pub fn publish(
        &self,
        owner_plugin_id: &str,
        snapshot: PluginLayoutSnapshot,
    ) -> Result<PluginLayoutPublishOutcome, PluginLayoutPublishError> {
        validate_snapshot(owner_plugin_id, &snapshot, self.maximum_requests_per_owner)?;
        let mut snapshots = self
            .snapshots
            .write()
            .map_err(|_| PluginLayoutPublishError::ConflictingRevision)?;
        if let Some(current) = snapshots.get(owner_plugin_id) {
            if snapshot.revision < current.revision {
                return Ok(PluginLayoutPublishOutcome::Stale);
            }
            if snapshot.revision == current.revision {
                return if snapshot == *current {
                    Ok(PluginLayoutPublishOutcome::Unchanged)
                } else {
                    Err(PluginLayoutPublishError::ConflictingRevision)
                };
            }
        }
        snapshots.insert(owner_plugin_id.to_string(), snapshot);
        drop(snapshots);
        Ok(PluginLayoutPublishOutcome::Applied)
    }

    /// Remove all retained intent for an owner, as on plugin disable or unload.
    pub fn remove_owner(&self, owner_plugin_id: &str) -> bool {
        self.snapshots
            .write()
            .ok()
            .and_then(|mut snapshots| snapshots.remove(owner_plugin_id))
            .is_some()
    }

    /// Snapshot all retained requests for deterministic host-side resolution.
    #[must_use]
    pub fn requests(&self) -> Vec<PluginLayoutRequest> {
        self.snapshots
            .read()
            .map(|snapshots| {
                snapshots
                    .values()
                    .flat_map(|snapshot| snapshot.requests.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn owner_count(&self) -> usize {
        self.snapshots.read().map_or(0, |snapshots| snapshots.len())
    }
}

fn validate_snapshot(
    owner_plugin_id: &str,
    snapshot: &PluginLayoutSnapshot,
    maximum_requests_per_owner: usize,
) -> Result<(), PluginLayoutPublishError> {
    if owner_plugin_id.is_empty() {
        return Err(PluginLayoutPublishError::EmptyOwner);
    }
    if snapshot.requests.len() > maximum_requests_per_owner {
        return Err(PluginLayoutPublishError::TooManyRequests {
            count: snapshot.requests.len(),
            maximum: maximum_requests_per_owner,
        });
    }
    let mut ids = BTreeSet::new();
    for request in &snapshot.requests {
        if request.id.owner_plugin_id != owner_plugin_id {
            return Err(PluginLayoutPublishError::OwnerMismatch {
                id: request.id.clone(),
            });
        }
        if !ids.insert(request.id.clone()) {
            return Err(PluginLayoutPublishError::DuplicateId {
                id: request.id.clone(),
            });
        }
    }
    Ok(())
}

static GLOBAL_PLUGIN_LAYOUT_REGISTRY: OnceLock<PluginLayoutRegistry> = OnceLock::new();

/// Process-global retained layout registry used by the attach host.
#[must_use]
pub fn global_plugin_layout_registry() -> &'static PluginLayoutRegistry {
    GLOBAL_PLUGIN_LAYOUT_REGISTRY.get_or_init(|| PluginLayoutRegistry::new(64))
}

/// Resolve requests in `(order, owner_plugin_id, local_id)` order.
///
/// `minimum_remaining` reserves a lower bound for the final host-owned region.
/// Each split is clamped independently, so malformed or oversized requests
/// cannot consume that region. Duplicate identities are rejected because they
/// would make update/removal ownership ambiguous.
///
/// # Errors
///
/// Returns the duplicated identity when two requests use the same ID.
pub fn resolve_plugin_layout(
    viewport: ExtensionRect,
    minimum_remaining: (u16, u16),
    requests: &[PluginLayoutRequest],
) -> Result<PluginLayoutResolution, PluginLayoutId> {
    let mut identities = BTreeSet::new();
    for request in requests {
        if !identities.insert(request.id.clone()) {
            return Err(request.id.clone());
        }
    }

    let mut ordered = requests.to_vec();
    ordered.sort_by(|left, right| {
        (left.order, &left.id.owner_plugin_id, &left.id.local_id).cmp(&(
            right.order,
            &right.id.owner_plugin_id,
            &right.id.local_id,
        ))
    });

    let minimum_width = minimum_remaining.0.min(viewport.w);
    let minimum_height = minimum_remaining.1.min(viewport.h);
    let mut remaining = viewport;
    let mut allocations = Vec::with_capacity(ordered.len());

    for request in ordered {
        let rect = match request.operation {
            LayoutOperation::Overlay => remaining,
            LayoutOperation::Split { edge, extent } => {
                split_region(&mut remaining, edge, extent, minimum_width, minimum_height)
            }
        };
        allocations.push(PluginLayoutAllocation {
            id: request.id,
            rect,
        });
    }

    Ok(PluginLayoutResolution {
        allocations,
        remaining,
    })
}

fn split_region(
    remaining: &mut ExtensionRect,
    edge: LayoutEdge,
    extent: LayoutExtent,
    minimum_width: u16,
    minimum_height: u16,
) -> ExtensionRect {
    match edge {
        LayoutEdge::Top | LayoutEdge::Bottom => {
            let available = remaining.h.saturating_sub(minimum_height);
            let height = extent.resolve(remaining.h).min(available);
            let y = if matches!(edge, LayoutEdge::Bottom) {
                remaining
                    .y
                    .saturating_add(remaining.h.saturating_sub(height))
            } else {
                remaining.y
            };
            let allocated = ExtensionRect::new(remaining.x, y, remaining.w, height);
            remaining.h = remaining.h.saturating_sub(height);
            if matches!(edge, LayoutEdge::Top) {
                remaining.y = remaining.y.saturating_add(height);
            }
            allocated
        }
        LayoutEdge::Left | LayoutEdge::Right => {
            let available = remaining.w.saturating_sub(minimum_width);
            let width = extent.resolve(remaining.w).min(available);
            let x = if matches!(edge, LayoutEdge::Right) {
                remaining
                    .x
                    .saturating_add(remaining.w.saturating_sub(width))
            } else {
                remaining.x
            };
            let allocated = ExtensionRect::new(x, remaining.y, width, remaining.h);
            remaining.w = remaining.w.saturating_sub(width);
            if matches!(edge, LayoutEdge::Left) {
                remaining.x = remaining.x.saturating_add(width);
            }
            allocated
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        owner: &str,
        local: &str,
        order: i32,
        operation: LayoutOperation,
    ) -> PluginLayoutRequest {
        PluginLayoutRequest {
            id: PluginLayoutId::new(owner, local),
            order,
            operation,
        }
    }

    #[test]
    fn edge_splits_compose_and_preserve_the_minimum_remainder() {
        let resolved = resolve_plugin_layout(
            ExtensionRect::new(0, 0, 120, 40),
            (20, 5),
            &[
                request(
                    "second",
                    "bottom",
                    20,
                    LayoutOperation::Split {
                        edge: LayoutEdge::Bottom,
                        extent: LayoutExtent::Cells(1),
                    },
                ),
                request(
                    "first",
                    "left",
                    10,
                    LayoutOperation::Split {
                        edge: LayoutEdge::Left,
                        extent: LayoutExtent::Cells(28),
                    },
                ),
            ],
        )
        .unwrap();

        assert_eq!(
            resolved.allocations[0].rect,
            ExtensionRect::new(0, 0, 28, 40)
        );
        assert_eq!(
            resolved.allocations[1].rect,
            ExtensionRect::new(28, 39, 92, 1)
        );
        assert_eq!(resolved.remaining, ExtensionRect::new(28, 0, 92, 39));
    }

    #[test]
    fn stable_identity_breaks_equal_order_ties() {
        let resolved = resolve_plugin_layout(
            ExtensionRect::new(0, 0, 10, 10),
            (1, 1),
            &[
                request("z", "one", 0, LayoutOperation::Overlay),
                request("a", "one", 0, LayoutOperation::Overlay),
            ],
        )
        .unwrap();

        assert_eq!(resolved.allocations[0].id.owner_plugin_id, "a");
        assert_eq!(resolved.allocations[1].id.owner_plugin_id, "z");
    }

    #[test]
    fn oversized_splits_cannot_consume_the_minimum_remainder() {
        let resolved = resolve_plugin_layout(
            ExtensionRect::new(5, 7, 30, 12),
            (8, 4),
            &[
                request(
                    "one",
                    "left",
                    0,
                    LayoutOperation::Split {
                        edge: LayoutEdge::Left,
                        extent: LayoutExtent::Percent(100),
                    },
                ),
                request(
                    "two",
                    "top",
                    1,
                    LayoutOperation::Split {
                        edge: LayoutEdge::Top,
                        extent: LayoutExtent::Cells(u16::MAX),
                    },
                ),
            ],
        )
        .unwrap();

        assert_eq!(resolved.remaining, ExtensionRect::new(27, 15, 8, 4));
    }

    #[test]
    fn duplicate_identity_is_rejected() {
        let duplicate = PluginLayoutId::new("owner", "same");
        let result = resolve_plugin_layout(
            ExtensionRect::new(0, 0, 10, 10),
            (1, 1),
            &[
                PluginLayoutRequest {
                    id: duplicate.clone(),
                    order: 0,
                    operation: LayoutOperation::Overlay,
                },
                request("between", "other", 1, LayoutOperation::Overlay),
                PluginLayoutRequest {
                    id: duplicate.clone(),
                    order: 2,
                    operation: LayoutOperation::Overlay,
                },
            ],
        );

        assert_eq!(result, Err(duplicate));
    }

    #[test]
    fn top_right_and_overlay_allocations_use_the_current_remainder() {
        let resolved = resolve_plugin_layout(
            ExtensionRect::new(10, 20, 40, 30),
            (1, 1),
            &[
                request(
                    "one",
                    "top",
                    0,
                    LayoutOperation::Split {
                        edge: LayoutEdge::Top,
                        extent: LayoutExtent::Cells(3),
                    },
                ),
                request(
                    "two",
                    "right",
                    1,
                    LayoutOperation::Split {
                        edge: LayoutEdge::Right,
                        extent: LayoutExtent::Cells(5),
                    },
                ),
                request("three", "overlay", 2, LayoutOperation::Overlay),
            ],
        )
        .unwrap();

        assert_eq!(
            resolved.allocations[0].rect,
            ExtensionRect::new(10, 20, 40, 3)
        );
        assert_eq!(
            resolved.allocations[1].rect,
            ExtensionRect::new(45, 23, 5, 27)
        );
        assert_eq!(
            resolved.allocations[2].rect,
            ExtensionRect::new(10, 23, 35, 27)
        );
        assert_eq!(resolved.remaining, ExtensionRect::new(10, 23, 35, 27));
    }

    #[test]
    fn bounded_exhaustive_splits_stay_inside_viewport_and_preserve_minimum() {
        let edges = [
            LayoutEdge::Top,
            LayoutEdge::Right,
            LayoutEdge::Bottom,
            LayoutEdge::Left,
        ];
        for width in 0..=12 {
            for height in 0..=8 {
                let viewport = ExtensionRect::new(7, 9, width, height);
                let minimum = (width.min(3), height.min(2));
                for first_edge in edges {
                    for second_edge in edges {
                        let resolved = resolve_plugin_layout(
                            viewport,
                            minimum,
                            &[
                                request(
                                    "one",
                                    "first",
                                    0,
                                    LayoutOperation::Split {
                                        edge: first_edge,
                                        extent: LayoutExtent::Percent(67),
                                    },
                                ),
                                request(
                                    "two",
                                    "second",
                                    1,
                                    LayoutOperation::Split {
                                        edge: second_edge,
                                        extent: LayoutExtent::Cells(5),
                                    },
                                ),
                            ],
                        )
                        .unwrap();

                        assert!(contains(viewport, resolved.remaining));
                        assert!(resolved.remaining.w >= minimum.0);
                        assert!(resolved.remaining.h >= minimum.1);
                        for allocation in &resolved.allocations {
                            assert!(contains(viewport, allocation.rect));
                            assert!(!intersects(allocation.rect, resolved.remaining));
                        }
                        assert!(!intersects(
                            resolved.allocations[0].rect,
                            resolved.allocations[1].rect
                        ));
                    }
                }
            }
        }
    }

    #[test]
    fn registry_replaces_owner_snapshot_and_ignores_stale_updates() {
        let registry = PluginLayoutRegistry::new(4);
        let first = PluginLayoutSnapshot {
            revision: 2,
            requests: vec![request("owner", "first", 0, LayoutOperation::Overlay)],
        };
        assert_eq!(
            registry.publish("owner", first.clone()),
            Ok(PluginLayoutPublishOutcome::Applied)
        );
        assert_eq!(
            registry.publish("owner", first),
            Ok(PluginLayoutPublishOutcome::Unchanged)
        );
        assert_eq!(
            registry.publish(
                "owner",
                PluginLayoutSnapshot {
                    revision: 1,
                    requests: Vec::new(),
                },
            ),
            Ok(PluginLayoutPublishOutcome::Stale)
        );
        assert_eq!(registry.requests().len(), 1);

        assert_eq!(
            registry.publish(
                "owner",
                PluginLayoutSnapshot {
                    revision: 3,
                    requests: vec![request("owner", "replacement", 0, LayoutOperation::Overlay,)],
                },
            ),
            Ok(PluginLayoutPublishOutcome::Applied)
        );
        assert_eq!(registry.requests()[0].id.local_id, "replacement");
        assert!(registry.remove_owner("owner"));
        assert!(registry.requests().is_empty());
    }

    #[test]
    fn registry_rejects_invalid_owner_duplicate_limit_and_conflict() {
        let registry = PluginLayoutRegistry::new(1);
        let valid = PluginLayoutSnapshot {
            revision: 1,
            requests: vec![request("owner", "one", 0, LayoutOperation::Overlay)],
        };
        assert_eq!(
            registry.publish("", valid.clone()),
            Err(PluginLayoutPublishError::EmptyOwner)
        );
        assert!(matches!(
            registry.publish("other", valid.clone()),
            Err(PluginLayoutPublishError::OwnerMismatch { .. })
        ));
        assert!(matches!(
            registry.publish(
                "owner",
                PluginLayoutSnapshot {
                    revision: 1,
                    requests: vec![
                        request("owner", "same", 0, LayoutOperation::Overlay),
                        request("owner", "same", 1, LayoutOperation::Overlay),
                    ],
                },
            ),
            Err(PluginLayoutPublishError::TooManyRequests { .. })
        ));
        assert_eq!(
            registry.publish("owner", valid),
            Ok(PluginLayoutPublishOutcome::Applied)
        );
        assert_eq!(
            registry.publish(
                "owner",
                PluginLayoutSnapshot {
                    revision: 1,
                    requests: Vec::new(),
                },
            ),
            Err(PluginLayoutPublishError::ConflictingRevision)
        );
    }

    #[test]
    fn registry_rejects_duplicate_ids_within_the_owner_limit() {
        let registry = PluginLayoutRegistry::new(2);
        assert!(matches!(
            registry.publish(
                "owner",
                PluginLayoutSnapshot {
                    revision: 1,
                    requests: vec![
                        request("owner", "same", 0, LayoutOperation::Overlay),
                        request("owner", "same", 1, LayoutOperation::Overlay),
                    ],
                },
            ),
            Err(PluginLayoutPublishError::DuplicateId { .. })
        ));
    }

    fn contains(outer: ExtensionRect, inner: ExtensionRect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.right() <= outer.right()
            && inner.bottom() <= outer.bottom()
    }

    fn intersects(left: ExtensionRect, right: ExtensionRect) -> bool {
        !left.is_empty()
            && !right.is_empty()
            && left.x < right.right()
            && left.right() > right.x
            && left.y < right.bottom()
            && left.bottom() > right.y
    }
}
