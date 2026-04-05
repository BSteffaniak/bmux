#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_session_models::{LayoutError, PaneId};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ============================================================================
// Terminal Models
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PaneSize {
    pub width: u16,
    pub height: u16,
}

impl PaneSize {
    /// Create a new pane size
    #[must_use]
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    /// Calculate the area (total character cells) of this pane
    #[must_use]
    pub fn area(&self) -> u32 {
        u32::from(self.width) * u32::from(self.height)
    }

    /// Check if this pane size is valid (non-zero dimensions)
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

impl Default for PaneSize {
    fn default() -> Self {
        Self::new(80, 24)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PaneLayout {
    Single(PaneId),
    HSplit {
        top: Box<Self>,
        bottom: Box<Self>,
        ratio: f32, // 0.0 to 1.0, percentage for top pane
    },
    VSplit {
        left: Box<Self>,
        right: Box<Self>,
        ratio: f32, // 0.0 to 1.0, percentage for left pane
    },
}

impl PaneLayout {
    #[must_use]
    pub const fn single(pane_id: PaneId) -> Self {
        Self::Single(pane_id)
    }

    #[must_use]
    pub fn hsplit(top: Self, bottom: Self, ratio: f32) -> Self {
        Self::HSplit {
            top: Box::new(top),
            bottom: Box::new(bottom),
            ratio: ratio.clamp(0.1, 0.9),
        }
    }

    #[must_use]
    pub fn vsplit(left: Self, right: Self, ratio: f32) -> Self {
        Self::VSplit {
            left: Box::new(left),
            right: Box::new(right),
            ratio: ratio.clamp(0.1, 0.9),
        }
    }

    #[must_use]
    pub fn contains_pane(&self, pane_id: &PaneId) -> bool {
        match self {
            Self::Single(id) => id == pane_id,
            Self::HSplit { top, bottom, .. } => {
                top.contains_pane(pane_id) || bottom.contains_pane(pane_id)
            }
            Self::VSplit { left, right, .. } => {
                left.contains_pane(pane_id) || right.contains_pane(pane_id)
            }
        }
    }

    #[must_use]
    pub fn collect_panes(&self) -> Vec<PaneId> {
        match self {
            Self::Single(id) => vec![*id],
            Self::HSplit { top, bottom, .. } => {
                let mut panes = top.collect_panes();
                panes.extend(bottom.collect_panes());
                panes
            }
            Self::VSplit { left, right, .. } => {
                let mut panes = left.collect_panes();
                panes.extend(right.collect_panes());
                panes
            }
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Pane {
    pub id: PaneId,
    pub size: PaneSize,
    pub title: Option<String>,
    pub working_directory: Option<String>,
    pub shell_command: Option<String>,
    pub is_active: bool,
    pub created_at: std::time::SystemTime,
    pub last_activity: std::time::SystemTime,
}

impl Pane {
    #[must_use]
    pub fn new(size: PaneSize) -> Self {
        let now = std::time::SystemTime::now();
        Self {
            id: PaneId::new(),
            size,
            title: None,
            working_directory: None,
            shell_command: None,
            is_active: false,
            created_at: now,
            last_activity: now,
        }
    }

    #[must_use]
    pub fn with_title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }

    #[must_use]
    pub fn with_working_directory(mut self, working_directory: String) -> Self {
        self.working_directory = Some(working_directory);
        self
    }

    #[must_use]
    pub fn with_shell_command(mut self, shell_command: String) -> Self {
        self.shell_command = Some(shell_command);
        self
    }

    pub fn set_active(&mut self, active: bool) {
        self.is_active = active;
        if active {
            self.update_activity();
        }
    }

    /// Resize the pane to a new size
    ///
    /// # Errors
    ///
    /// * Invalid dimensions (width or height is 0)
    pub fn resize(&mut self, new_size: PaneSize) -> Result<(), bmux_session_models::PaneError> {
        if !new_size.is_valid() {
            return Err(bmux_session_models::PaneError::InvalidDimensions {
                width: new_size.width,
                height: new_size.height,
            });
        }
        self.size = new_size;
        self.update_activity();
        Ok(())
    }

    /// Update the last activity timestamp
    pub fn update_activity(&mut self) {
        self.last_activity = std::time::SystemTime::now();
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PaneGroup {
    pub id: PaneId,
    pub name: Option<String>,
    pub panes: BTreeMap<PaneId, Pane>,
    pub layout: PaneLayout,
    pub active_pane: Option<PaneId>,
    pub size: PaneSize,
    pub created_at: std::time::SystemTime,
    pub last_activity: std::time::SystemTime,
}

impl PaneGroup {
    #[must_use]
    pub fn new(size: PaneSize, name: Option<String>) -> Self {
        let now = std::time::SystemTime::now();
        let mut pane_group = Self {
            id: PaneId::new(),
            name,
            panes: BTreeMap::new(),
            layout: PaneLayout::Single(PaneId::new()), // Temporary, will be replaced
            active_pane: None,
            size,
            created_at: now,
            last_activity: now,
        };

        // Create initial pane
        let pane = Pane::new(size);
        let pane_id = pane.id;
        pane_group.panes.insert(pane_id, pane);
        pane_group.layout = PaneLayout::Single(pane_id);
        pane_group.active_pane = Some(pane_id);

        pane_group
    }

    #[must_use]
    pub fn with_name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    #[must_use]
    pub fn get_pane(&self, pane_id: &PaneId) -> Option<&Pane> {
        self.panes.get(pane_id)
    }

    pub fn get_pane_mut(&mut self, pane_id: &PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(pane_id)
    }

    pub fn add_pane(&mut self, pane: Pane) {
        self.panes.insert(pane.id, pane);
        self.update_activity();
    }

    pub fn remove_pane(&mut self, pane_id: &PaneId) -> Option<Pane> {
        let pane = self.panes.remove(pane_id);
        if pane.is_some() {
            // Update active pane if necessary
            if self.active_pane == Some(*pane_id) {
                self.active_pane = self.panes.keys().next().copied();
            }
            self.update_activity();
        }
        pane
    }

    /// Set the active pane in this pane group
    ///
    /// # Errors
    ///
    /// * Pane not found in this pane group
    pub fn set_active_pane(
        &mut self,
        pane_id: PaneId,
    ) -> Result<(), bmux_session_models::PaneError> {
        if self.panes.contains_key(&pane_id) {
            // Deactivate old pane
            if let Some(old_pane_id) = self.active_pane
                && let Some(old_pane) = self.panes.get_mut(&old_pane_id)
            {
                old_pane.set_active(false);
            }

            // Activate new pane
            if let Some(new_pane) = self.panes.get_mut(&pane_id) {
                new_pane.set_active(true);
            }

            self.active_pane = Some(pane_id);
            self.update_activity();
            Ok(())
        } else {
            Err(bmux_session_models::PaneError::NotFound(pane_id))
        }
    }

    /// Split an existing pane into two panes
    ///
    /// # Errors
    ///
    /// * Pane not found in this pane group
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn split_pane(
        &mut self,
        pane_id: PaneId,
        direction: SplitDirection,
        ratio: f32,
    ) -> Result<PaneId, LayoutError> {
        if !self.panes.contains_key(&pane_id) {
            return Err(LayoutError::PaneNotFound(pane_id));
        }

        // Create new pane with appropriate size
        let new_pane_size = match direction {
            SplitDirection::Horizontal => PaneSize::new(
                self.size.width,
                (f32::from(self.size.height) * (1.0 - ratio)) as u16,
            ),
            SplitDirection::Vertical => PaneSize::new(
                (f32::from(self.size.width) * (1.0 - ratio)) as u16,
                self.size.height,
            ),
        };

        let new_pane = Pane::new(new_pane_size);
        let new_pane_id = new_pane.id;
        self.panes.insert(new_pane_id, new_pane);

        // Update layout - this is simplified, would need more complex logic for nested layouts
        match direction {
            SplitDirection::Horizontal => {
                self.layout = PaneLayout::hsplit(
                    PaneLayout::Single(pane_id),
                    PaneLayout::Single(new_pane_id),
                    ratio,
                );
            }
            SplitDirection::Vertical => {
                self.layout = PaneLayout::vsplit(
                    PaneLayout::Single(pane_id),
                    PaneLayout::Single(new_pane_id),
                    ratio,
                );
            }
        }

        self.update_activity();
        Ok(new_pane_id)
    }

    /// Resize the pane group to a new size
    ///
    /// # Errors
    ///
    /// * Invalid pane group dimensions (width or height is 0)
    pub fn resize(&mut self, new_size: PaneSize) -> Result<(), LayoutError> {
        if !new_size.is_valid() {
            return Err(LayoutError::InvalidLayout(format!(
                "Invalid pane group size: {}x{}",
                new_size.width, new_size.height
            )));
        }

        self.size = new_size;
        // Would need to resize all panes according to layout
        // This is simplified - real implementation would recalculate layout
        self.update_activity();
        Ok(())
    }

    fn update_activity(&mut self) {
        self.last_activity = std::time::SystemTime::now();
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PaneGroupInfo {
    pub id: PaneId,
    pub name: Option<String>,
    pub pane_count: usize,
    pub active_pane: Option<PaneId>,
    pub size: PaneSize,
    pub created_at: std::time::SystemTime,
    pub last_activity: std::time::SystemTime,
}

impl From<&PaneGroup> for PaneGroupInfo {
    fn from(group: &PaneGroup) -> Self {
        Self {
            id: group.id,
            name: group.name.clone(),
            pane_count: group.panes.len(),
            active_pane: group.active_pane,
            size: group.size,
            created_at: group.created_at,
            last_activity: group.last_activity,
        }
    }
}
