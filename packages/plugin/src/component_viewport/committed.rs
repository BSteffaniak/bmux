//! Caller-owned geometry becomes interactive only after successful presentation.
use super::ComponentViewport;
use std::collections::VecDeque;

#[derive(Debug, Clone, Default)]
pub struct CommittedComponentViewport {
    pending: VecDeque<(u64, Option<ComponentViewport>)>,
    committed: Option<ComponentViewport>,
}

impl CommittedComponentViewport {
    pub fn stage(&mut self, revision: u64, viewport: Option<ComponentViewport>) {
        if self.pending.len() == 32 {
            self.pending.pop_front();
        }
        self.pending.push_back((revision, viewport));
    }

    pub fn acknowledge(&mut self, revision: u64) {
        if let Some((_, viewport)) = self.pending.iter().find(|(id, _)| *id == revision) {
            self.committed = viewport.clone();
        } else if self.pending.front().is_some_and(|(id, _)| revision < *id) {
            self.committed = None;
        }
        self.pending.retain(|(id, _)| *id > revision);
    }

    #[must_use]
    pub const fn get(&self) -> Option<&ComponentViewport> {
        self.committed.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::{
        component::{LayoutId, LayoutNode, LogicalSize},
        geometry::{Point, Rect},
    };

    fn viewport(x: u16) -> Option<ComponentViewport> {
        ComponentViewport::new(
            LayoutNode::leaf(LayoutId::new("control"), LogicalSize::new(5, 3)),
            Rect::new(x, 0, 5, 3),
            Point::new(0, 0),
        )
    }

    #[test]
    fn publication_does_not_advance_committed_geometry() {
        let mut state = CommittedComponentViewport::default();
        state.stage(1, viewport(2));
        assert!(state.get().is_none());
        state.acknowledge(1);
        state.stage(2, viewport(8));
        assert_eq!(state.get().unwrap().visible_rect().x, 2);
        state.acknowledge(2);
        assert_eq!(state.get().unwrap().visible_rect().x, 8);
        state.stage(3, None);
        assert!(state.get().is_some());
        state.acknowledge(3);
        assert!(state.get().is_none());
    }

    #[test]
    fn evicted_revision_cannot_guess_interaction_geometry() {
        let mut state = CommittedComponentViewport::default();
        for revision in 1..=34 {
            state.stage(revision, viewport(2));
        }
        state.acknowledge(1);
        assert!(state.get().is_none());
        state.acknowledge(34);
        assert!(state.get().is_some());
    }
}
