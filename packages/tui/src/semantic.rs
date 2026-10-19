//! Protocol-neutral semantic regions committed with terminal presentation.

use crate::geometry::Rect;

/// Stable semantic role exposed by a rendered component.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
pub struct SemanticRegion {
    /// Stable caller-owned semantic identity.
    pub id: String,
    /// Visible semantic bounds in terminal cells.
    pub area: Rect,
    /// Domain-neutral role or label.
    pub role: String,
    /// Optional accessibility metadata. Human-readable output omits absent metadata.
    /// Binary sequences always encode the option so subsequent fields remain aligned.
    #[cfg_attr(feature = "serde", serde(default))]
    pub accessibility: Option<SemanticAccessibility>,
}

#[cfg(feature = "serde")]
impl serde::Serialize for SemanticRegion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        // Named JSON maps can omit optional fields. Positional binary structs
        // cannot: skipping an option shifts the enclosing scene's next field.
        let include_accessibility = !serializer.is_human_readable() || self.accessibility.is_some();
        let mut state = serializer
            .serialize_struct("SemanticRegion", if include_accessibility { 4 } else { 3 })?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("area", &self.area)?;
        state.serialize_field("role", &self.role)?;
        if include_accessibility {
            state.serialize_field("accessibility", &self.accessibility)?;
        }
        state.end()
    }
}

impl SemanticRegion {
    /// Create one semantic region.
    #[must_use]
    pub fn new(id: impl Into<String>, area: Rect, role: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            area,
            role: role.into(),
            accessibility: None,
        }
    }
}

/// Optional, explicitly supplied accessibility content for the visible region only.
///
/// Producers must never put scrollback or secrets here. Native adapters suppress
/// label, description and text when `sensitive` is set. Actions describe existing
/// interaction paths; they do not grant permission to mutate application state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SemanticAccessibility {
    /// Human-readable accessible name.
    pub label: Option<String>,
    /// Supplemental accessible description.
    pub description: Option<String>,
    /// Explicit visible text, not reconstructed screen cells or history.
    pub text: Option<String>,
    /// Suppress all textual content from assistive technology.
    pub sensitive: bool,
    /// Whether the region is disabled.
    pub disabled: bool,
    /// Whether the region has committed keyboard focus.
    pub focused: bool,
    /// Selection state when meaningful for this role.
    pub selected: Option<bool>,
    /// Actions supported by the existing interaction owner.
    pub actions: Vec<SemanticAction>,
}

/// Neutral interaction intent, routed through the application's normal input path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum SemanticAction {
    /// Request keyboard focus.
    Focus,
    /// Invoke the region's normal primary interaction.
    Activate,
    /// Invoke the region's normal context interaction.
    ShowContextMenu,
}

/// Ordered semantic scene from one committed frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_constructor_has_no_accessibility_wire_extension() {
        let region = SemanticRegion::new("legacy", Rect::new(0, 0, 2, 1), "label");
        assert!(region.accessibility.is_none());
    }

    #[test]
    fn partial_scene_merge_replaces_accessibility_with_its_visual_owner() {
        let mut original = SemanticScene::new();
        let mut region = SemanticRegion::new("retained", Rect::new(0, 0, 2, 1), "button");
        region.accessibility = Some(SemanticAccessibility {
            label: Some("original".into()),
            actions: vec![SemanticAction::Activate],
            ..Default::default()
        });
        original.push(region.clone());
        let unchanged = original.merge_regions(&SemanticScene::new(), &[Rect::new(4, 0, 2, 1)]);
        assert_eq!(unchanged.regions()[0], region);
        region.accessibility.as_mut().unwrap().label = Some("replacement".into());
        let mut replacement = SemanticScene::new();
        replacement.push(region.clone());
        let merged = original.merge_regions(&replacement, &[region.area]);
        assert_eq!(merged.regions(), &[region]);
    }
}
