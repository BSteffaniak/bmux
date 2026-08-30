#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

//! Neutral attach view change and client-local presentation protocol DTOs.

/// Coarse components of an attached view that may need resynchronization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachViewComponent {
    Scene,
    SurfaceContent,
    Layout,
}

/// Neutral client-local presentation state for one attached view.
///
/// This carries no domain catalog or permission model. Workflow owners resolve
/// user-facing labels and hints before publishing; presentation companions only
/// place and style the resulting state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachLocalPresentationSnapshot {
    pub revision: u64,
    pub mode_id: String,
    pub mode_label: String,
    pub role_label: String,
    pub follow_label: Option<String>,
    pub mode_modifier: Option<String>,
    pub hint: String,
    pub session_label: Option<String>,
    pub session_count: u32,
    pub context_label: Option<String>,
    /// Resolved neutral appearance colors for the active local mode.
    pub foreground: String,
    pub background: String,
    pub status_background: String,
    pub status_foreground: String,
    pub status_active: String,
    pub status_mode: String,
    /// Current terminal width in cells. Layout-owning companions use this for
    /// width-aware retained projection; zero means no usable viewport yet.
    pub viewport_cols: u16,
}

impl AttachLocalPresentationSnapshot {
    #[must_use]
    pub fn initial() -> Self {
        Self {
            revision: 0,
            mode_id: "normal".to_string(),
            mode_label: "NORMAL".to_string(),
            role_label: "write".to_string(),
            follow_label: None,
            mode_modifier: None,
            hint: String::new(),
            session_label: None,
            session_count: 0,
            context_label: None,
            foreground: "#ffffff".to_string(),
            background: "#000000".to_string(),
            status_background: "#1e1e1e".to_string(),
            status_foreground: "#ffffff".to_string(),
            status_active: "#00ff00".to_string(),
            status_mode: "#ffff00".to_string(),
            viewport_cols: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_local_presentation_snapshot_is_neutral_and_writable() {
        let snapshot = AttachLocalPresentationSnapshot::initial();
        assert_eq!(snapshot.revision, 0);
        assert_eq!(snapshot.mode_id, "normal");
        assert_eq!(snapshot.mode_label, "NORMAL");
        assert_eq!(snapshot.role_label, "write");
        assert!(snapshot.follow_label.is_none());
        assert!(snapshot.hint.is_empty());
    }
}
