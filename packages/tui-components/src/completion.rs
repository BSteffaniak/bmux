//! Domain-neutral completion interaction, separate from candidate acquisition and editing.
//!
//! Applications map their input vocabulary to [`CompletionCommand`] before routing
//! to an editor or parent submit control. A consumed command need not mutate state.
//! Render the caller-owned list state with [`SelectableListComponent`], inside a
//! [`crate::picker_frame::PickerFrameComponent`] using field-anchored placement.
//! Retain the successfully presented list node for pointer routing; never measure
//! fresh geometry while handling a pointer. Use `SelectableListComponent::reveal_focus`
//! against the measured list before painting after semantic navigation. Acceptance callbacks may use
//! `TextInputState::buffer_mut().replace_byte_range` for one editor transaction.

use std::cell::Cell;

use bmux_tui::component::{EventCx, LayoutNode};
use bmux_tui::event::{Event, EventOutcome};

use crate::selectable_list::{
    SelectableListComponent, SelectableListItem, SelectableListOutcome, SelectableListState,
};

/// Semantic commands supplied by the application's keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionCommand {
    /// Attempt to accept the current candidate.
    Accept,
    /// Close completion without editing.
    Dismiss,
    /// Focus the previous enabled candidate, wrapping at the start.
    Previous,
    /// Focus the next enabled candidate, wrapping at the end.
    Next,
}

/// Application-provided availability; messages and acquisition remain caller-owned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStatus {
    /// Current candidates can be accepted.
    Candidates,
    /// Acquisition is pending; old candidates cannot be accepted.
    Loading,
    /// Acquisition succeeded without candidates.
    Empty,
    /// Acquisition failed; old candidates cannot be accepted.
    Error,
}

/// Caller-owned interaction state, with selection tracked by stable candidate ID.
#[derive(Debug, Clone)]
pub struct CompletionState {
    active: bool,
    selected_id: Option<String>,
    identities: Vec<(String, bool)>,
    list: Cell<SelectableListState>,
}

impl CompletionState {
    /// Create inactive completion state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            active: false,
            selected_id: None,
            identities: Vec::new(),
            list: Cell::new(SelectableListState::new(None)),
        }
    }

    /// Open completion. Candidate reconciliation does not require a paint pass.
    pub const fn activate(&mut self) {
        self.active = true;
    }

    /// Close completion and discard pending pointer gestures.
    pub fn dismiss(&mut self) {
        self.active = false;
        self.list.set(SelectableListState::new(None));
    }

    /// Whether commands currently belong to this completion interaction.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Stable ID of the current enabled candidate, if any.
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    /// State for the measured list component. Candidate IDs must be unique.
    #[must_use]
    pub const fn list_state(&self) -> &Cell<SelectableListState> {
        &self.list
    }

    /// Preserve selection across reordering; invalidate pointer gestures when
    /// the identity/availability set changes. Disabled candidates are skipped.
    pub fn reconcile(&mut self, items: &[SelectableListItem]) {
        let identities: Vec<_> = items
            .iter()
            .map(|item| (item.id.clone(), item.disabled))
            .collect();
        let index = self
            .selected_id
            .as_ref()
            .and_then(|id| {
                items
                    .iter()
                    .position(|item| item.id == *id && !item.disabled)
            })
            .or_else(|| items.iter().position(|item| !item.disabled));
        let mut list = if self.identities == identities {
            self.list.get()
        } else {
            self.identities = identities;
            SelectableListState::new(None)
        };
        self.selected_id = index.map(|index| items[index].id.clone());
        // Completion focus is not a committed selection: clicking the already
        // focused candidate must still produce a Selected activation on release.
        list.set_selected(None);
        list.set_focused(index);
        self.list.set(list);
    }
}

impl Default for CompletionState {
    fn default() -> Self {
        Self::new()
    }
}

/// Generic completion controller over caller-provided visible candidates.
#[derive(Debug, Clone, Copy)]
pub struct CompletionInteraction<'a> {
    items: &'a [SelectableListItem],
    status: CompletionStatus,
}

impl<'a> CompletionInteraction<'a> {
    /// Compose interaction over current candidates and availability.
    #[must_use]
    pub const fn new(items: &'a [SelectableListItem], status: CompletionStatus) -> Self {
        Self { items, status }
    }

    /// Route one remapped command before editor/parent input. The callback
    /// returns true only when the application accepted the candidate; rejection
    /// leaves completion open but still consumes the command.
    pub fn handle_command(
        &self,
        command: CompletionCommand,
        state: &mut CompletionState,
        mut accept: impl FnMut(&str) -> bool,
    ) -> EventOutcome {
        if !state.active {
            return EventOutcome::Ignored;
        }
        state.reconcile(self.items);
        match command {
            CompletionCommand::Dismiss => {
                state.dismiss();
                EventOutcome::Redraw
            }
            CompletionCommand::Accept => {
                if self.status == CompletionStatus::Candidates
                    && state.selected_id().is_some_and(&mut accept)
                {
                    state.dismiss();
                    EventOutcome::Redraw
                } else {
                    EventOutcome::Handled
                }
            }
            CompletionCommand::Previous | CompletionCommand::Next => {
                if self.status != CompletionStatus::Candidates {
                    return EventOutcome::Handled;
                }
                let enabled: Vec<_> = self
                    .items
                    .iter()
                    .enumerate()
                    .filter_map(|(index, item)| (!item.disabled).then_some(index))
                    .collect();
                let Some(current) = enabled
                    .iter()
                    .position(|index| Some(self.items[*index].id.as_str()) == state.selected_id())
                else {
                    return EventOutcome::Handled;
                };
                let next = match command {
                    CompletionCommand::Previous => (current + enabled.len() - 1) % enabled.len(),
                    _ => (current + 1) % enabled.len(),
                };
                if next == current {
                    return EventOutcome::Handled;
                }
                let index = enabled[next];
                state.selected_id = Some(self.items[index].id.clone());
                let mut list = state.list.get();
                list.set_selected(Some(index));
                list.set_focused(Some(index));
                state.list.set(list);
                EventOutcome::Redraw
            }
        }
    }

    /// Route pointer input using the exact committed list node and its scoped
    /// event context. The caller must route through the measured ancestor
    /// transforms and clip (for example `EventCx::with_child`). Never pass an
    /// unpresented layout. Keyboard input is intentionally not handled here.
    pub fn handle_pointer(
        &self,
        event: &Event,
        layout: &LayoutNode,
        cx: &mut EventCx<'_>,
        state: &mut CompletionState,
        mut accept: impl FnMut(&str) -> bool,
    ) -> EventOutcome {
        if !state.active || !matches!(event, Event::Mouse(_)) {
            return EventOutcome::Ignored;
        }
        state.reconcile(self.items);
        let list = SelectableListComponent::new(layout.id.clone(), self.items, state.list_state());
        let outcome = list.handle_event(event, layout, cx);
        match outcome {
            SelectableListOutcome::Ignored => EventOutcome::Ignored,
            SelectableListOutcome::Redraw => {
                if let Some(index) = state.list.get().focused() {
                    state.selected_id = self.items.get(index).map(|item| item.id.clone());
                }
                EventOutcome::Redraw
            }
            SelectableListOutcome::Focused(index) | SelectableListOutcome::Selected(index) => {
                state.selected_id = self.items.get(index).map(|item| item.id.clone());
                if matches!(outcome, SelectableListOutcome::Selected(_)) {
                    if self.status == CompletionStatus::Candidates
                        && state.selected_id().is_some_and(&mut accept)
                    {
                        state.dismiss();
                        EventOutcome::Redraw
                    } else {
                        EventOutcome::Handled
                    }
                } else {
                    EventOutcome::Redraw
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::geometry::{Point, Rect, Size};

    fn items() -> Vec<SelectableListItem> {
        vec![
            SelectableListItem::new("a", "界"),
            SelectableListItem::new("b", "e\u{301}🦀"),
        ]
    }

    #[test]
    fn active_noop_accept_never_submits_parent_before_first_paint() {
        let mut parent_submits = 0;
        for status in [
            CompletionStatus::Candidates,
            CompletionStatus::Loading,
            CompletionStatus::Empty,
            CompletionStatus::Error,
        ] {
            for candidates in [vec![], items()] {
                let mut state = CompletionState::new();
                state.activate();
                let outcome = CompletionInteraction::new(&candidates, status).handle_command(
                    CompletionCommand::Accept,
                    &mut state,
                    |_| false,
                );
                if outcome == EventOutcome::Ignored {
                    parent_submits += 1;
                }
                assert_eq!(outcome, EventOutcome::Handled);
                assert!(state.is_active());
            }
        }
        assert_eq!(parent_submits, 0);
    }

    #[test]
    fn identity_survives_reorder_and_navigation_skips_disabled() {
        let mut candidates = items();
        let mut state = CompletionState::new();
        state.activate();
        assert_eq!(
            CompletionInteraction::new(&candidates, CompletionStatus::Candidates).handle_command(
                CompletionCommand::Next,
                &mut state,
                |_| false
            ),
            EventOutcome::Redraw
        );
        assert_eq!(state.selected_id(), Some("b"));
        candidates.reverse();
        state.reconcile(&candidates);
        assert_eq!(state.selected_id(), Some("b"));
        assert_eq!(state.list_state().get().focused(), Some(0));
        candidates[0].disabled = true;
        state.reconcile(&candidates);
        assert_eq!(state.selected_id(), Some("a"));
        assert_eq!(
            CompletionInteraction::new(&candidates, CompletionStatus::Candidates).handle_command(
                CompletionCommand::Next,
                &mut state,
                |_| false
            ),
            EventOutcome::Handled
        );
    }

    #[test]
    fn semantic_navigation_reveals_focus_through_shared_measured_scroll() {
        let candidates = items();
        let mut state = CompletionState::new();
        state.activate();
        state.reconcile(&candidates);
        let layout =
            SelectableListComponent::new("completion.list", &candidates, state.list_state())
                .layout(Constraints::tight(Size::new(8, 1)), &mut LayoutCx::new());
        CompletionInteraction::new(&candidates, CompletionStatus::Candidates).handle_command(
            CompletionCommand::Next,
            &mut state,
            |_| false,
        );
        SelectableListComponent::new("completion.list", &candidates, state.list_state())
            .reveal_focus(&layout);
        assert_eq!(state.list_state().get().focused(), Some(1));
        assert_eq!(state.list_state().get().vertical_scroll(), 1);
    }

    #[test]
    fn committed_pointer_accepts_only_matching_press_release() {
        let candidates = items();
        for status in [CompletionStatus::Candidates, CompletionStatus::Loading] {
            for row in [0, 1] {
                let mut state = CompletionState::new();
                state.activate();
                state.reconcile(&candidates);
                let component = SelectableListComponent::new(
                    "completion.list",
                    &candidates,
                    state.list_state(),
                );
                let layout =
                    component.layout(Constraints::tight(Size::new(8, 2)), &mut LayoutCx::new());
                let interaction = CompletionInteraction::new(&candidates, status);
                let mut accepted = 0;
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    interaction.handle_pointer(
                        &Event::Mouse(MouseEvent::new(kind, Point::new(1, row))),
                        &layout,
                        &mut EventCx::with_clip(&layout, Rect::new(0, 0, 8, 2)),
                        &mut state,
                        |id| {
                            assert_eq!(id, candidates[usize::from(row)].id);
                            accepted += 1;
                            true
                        },
                    );
                }
                assert_eq!(
                    accepted,
                    usize::from(status == CompletionStatus::Candidates)
                );
                assert_eq!(state.is_active(), status != CompletionStatus::Candidates);
            }
        }
    }

    #[test]
    fn clipped_pointer_does_not_accept_and_tiny_list_has_no_hit() {
        let candidates = items();
        for width in [0, 1, 8] {
            let mut state = CompletionState::new();
            state.activate();
            state.reconcile(&candidates);
            let component =
                SelectableListComponent::new("completion.list", &candidates, state.list_state());
            let layout = component.layout(
                Constraints::tight(Size::new(width, 2)),
                &mut LayoutCx::new(),
            );
            let interaction = CompletionInteraction::new(&candidates, CompletionStatus::Candidates);
            let mut accepted = 0;
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                let event = Event::Mouse(MouseEvent::new(kind, Point::new(0, 1)));
                interaction.handle_pointer(
                    &event,
                    &layout,
                    &mut EventCx::with_clip(&layout, Rect::new(0, 0, width, 1)),
                    &mut state,
                    |_| {
                        accepted += 1;
                        true
                    },
                );
            }
            assert_eq!(accepted, 0);
            assert!(state.is_active());
        }
    }

    #[test]
    fn accepted_editor_transaction_is_one_undo_and_dismisses() {
        let candidates = items();
        let mut input =
            crate::text_input::TextInputState::new(bmux_text_edit::TextEditBuffer::from_text("x"));
        let mut state = CompletionState::new();
        state.activate();
        let before = input.buffer().edit_state();
        let outcome = CompletionInteraction::new(&candidates, CompletionStatus::Candidates)
            .handle_command(CompletionCommand::Accept, &mut state, |id| {
                assert_eq!(id, "a");
                input
                    .buffer_mut()
                    .replace_byte_range(&before, 0..1, "界", 3, None)
                    == Ok(true)
            });
        assert_eq!(outcome, EventOutcome::Redraw);
        assert!(!state.is_active());
        assert!(input.buffer_mut().undo());
        assert_eq!(input.buffer().text(), "x");
        assert!(!input.buffer_mut().undo());
    }
}
