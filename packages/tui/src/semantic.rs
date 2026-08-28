//! Protocol-neutral semantic regions committed with terminal presentation.

use crate::geometry::Rect;

/// Stable semantic role exposed by a rendered component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticRegion {
    /// Stable caller-owned semantic identity.
    pub id: String,
    /// Visible semantic bounds in terminal cells.
    pub area: Rect,
    /// Domain-neutral role or label.
    pub role: String,
}

impl SemanticRegion {
    /// Create one semantic region.
    #[must_use]
    pub fn new(id: impl Into<String>, area: Rect, role: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            area,
            role: role.into(),
        }
    }
}

/// Ordered semantic scene from one committed frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SemanticScene {
    regions: Vec<SemanticRegion>,
}

impl SemanticScene {
    /// Create an empty scene.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            regions: Vec::new(),
        }
    }

    /// Add one region in paint order.
    pub fn push(&mut self, region: SemanticRegion) {
        if !region.area.is_empty() {
            self.regions.push(region);
        }
    }

    /// Ordered semantic regions.
    #[must_use]
    pub fn regions(&self) -> &[SemanticRegion] {
        &self.regions
    }

    /// Remove regions intersecting any replaced terminal area.
    pub fn retain_outside(&mut self, replaced: &[Rect]) {
        self.regions.retain(|region| {
            !replaced
                .iter()
                .any(|area| !region.area.intersection(*area).is_empty())
        });
    }

    /// Merge a partial scene over retained committed semantics.
    #[must_use]
    pub fn merge_regions(&self, replacement: &Self, replaced: &[Rect]) -> Self {
        let mut merged = self.clone();
        merged.retain_outside(replaced);
        merged.regions.extend(
            replacement
                .regions
                .iter()
                .filter(|region| {
                    replaced
                        .iter()
                        .any(|area| !region.area.intersection(*area).is_empty())
                })
                .cloned(),
        );
        merged
    }
}
