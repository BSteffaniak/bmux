//! Composition state and candidate placement. Preedit is presentation-only.
//! Candidate rectangles must come from the same successfully committed geometry
//! as cursor drawing; this module never infers a rectangle from terminal dimensions.

use crate::geometry::PhysicalRect;
use bmux_frontend_runtime::input::ImeEvent;

/// Window-local IME lifecycle. Disabled/focus loss clears pending composition.
#[derive(Debug, Default)]
pub struct ImeState {
    composing: bool,
}

impl ImeState {
    /// Whether platform keyboard text must be suppressed in favor of IME commits.
    #[must_use]
    pub const fn composing(&self) -> bool {
        self.composing
    }

    /// Normalize native IME events. Invalid UTF-8 cursor ranges are removed,
    /// never rounded into a different source position.
    pub fn update(&mut self, event: ImeEvent) -> ImeEvent {
        match event {
            ImeEvent::Preedit { text, cursor } => {
                self.composing = !text.is_empty();
                let cursor = cursor.filter(|&(start, end)| {
                    start <= end && text.is_char_boundary(start) && text.is_char_boundary(end)
                });
                ImeEvent::Preedit { text, cursor }
            }
            ImeEvent::Commit(text) => {
                self.composing = false;
                ImeEvent::Commit(text)
            }
            ImeEvent::Disabled => {
                self.cancel();
                ImeEvent::Disabled
            }
            ImeEvent::Enabled => ImeEvent::Enabled,
        }
    }

    pub const fn cancel(&mut self) {
        self.composing = false;
    }
}

/// Candidate placement tagged with the committed presentation revision. The
/// caller supplies that revision only after successful output and clears it on
/// invalidation/resize; queued or failed frame geometry must never be installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImeCursorArea {
    pub presentation_revision: u64,
    pub rect: PhysicalRect,
}

impl ImeCursorArea {
    /// Reject candidate geometry from a frame other than the interaction owner.
    #[must_use]
    pub const fn for_committed_revision(self, revision: u64) -> Option<PhysicalRect> {
        if self.presentation_revision == revision {
            Some(self.rect)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preedit_is_not_commit_and_invalid_cursor_is_removed() {
        let mut state = ImeState::default();
        assert_eq!(
            state.update(ImeEvent::Preedit {
                text: "é".into(),
                cursor: Some((1, 2)),
            }),
            ImeEvent::Preedit {
                text: "é".into(),
                cursor: None
            }
        );
        assert!(state.composing());
        assert_eq!(
            state.update(ImeEvent::Commit("é".into())),
            ImeEvent::Commit("é".into())
        );
        assert!(!state.composing());
    }

    #[test]
    fn cancellation_and_stale_candidate_geometry() {
        let mut state = ImeState::default();
        state.update(ImeEvent::Preedit {
            text: "x".into(),
            cursor: Some((0, 1)),
        });
        state.cancel();
        assert!(!state.composing());
        let area = ImeCursorArea {
            presentation_revision: 4,
            rect: PhysicalRect::default(),
        };
        assert_eq!(area.for_committed_revision(3), None);
        assert_eq!(
            area.for_committed_revision(4),
            Some(PhysicalRect::default())
        );
    }
}
