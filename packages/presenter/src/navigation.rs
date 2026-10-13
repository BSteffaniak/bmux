//! Pure reveal/navigation reducer. No terminal or runtime authority.
use crate::model::{Deck, Slide, SlideId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationState {
    pub slide_id: Option<SlideId>,
    pub reveal_step: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NavigationAction {
    Next,
    Previous,
    First,
    Last,
    GoTo(SlideId),
    Reconcile,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NavigationOutcome {
    pub changed: bool,
    pub slide_changed: bool,
}

impl PresentationState {
    pub fn new(deck: &Deck) -> Self {
        Self {
            slide_id: deck.slides.first().map(|slide| slide.id.clone()),
            reveal_step: 0,
        }
    }
    pub fn current_slide<'a>(&self, deck: &'a Deck) -> Option<&'a Slide> {
        self.slide_id.as_ref().and_then(|id| deck.slide(id))
    }
}

pub fn reduce(
    deck: &Deck,
    state: &mut PresentationState,
    action: NavigationAction,
) -> NavigationOutcome {
    let before = state.clone();
    let index = state.slide_id.as_ref().and_then(|id| deck.slide_index(id));
    let Some(mut index) = index.or_else(|| (!deck.slides.is_empty()).then_some(0)) else {
        *state = PresentationState::default();
        return NavigationOutcome {
            changed: before != *state,
            slide_changed: before.slide_id != state.slide_id,
        };
    };
    if state.slide_id.as_ref() != Some(&deck.slides[index].id) {
        state.reveal_step = 0;
    }
    state.reveal_step = state.reveal_step.min(deck.slides[index].max_reveal_step());
    match action {
        NavigationAction::Next => {
            if state.reveal_step < deck.slides[index].max_reveal_step() {
                state.reveal_step += 1;
            } else if index + 1 < deck.slides.len() {
                index += 1;
                state.reveal_step = 0;
            }
        }
        NavigationAction::Previous => {
            if state.reveal_step > 0 {
                state.reveal_step -= 1;
            } else if index > 0 {
                index -= 1;
                state.reveal_step = deck.slides[index].max_reveal_step();
            }
        }
        NavigationAction::First => {
            index = 0;
            state.reveal_step = 0;
        }
        NavigationAction::Last => {
            index = deck.slides.len() - 1;
            state.reveal_step = deck.slides[index].max_reveal_step();
        }
        NavigationAction::GoTo(id) => {
            if let Some(target) = deck.slide_index(&id) {
                index = target;
                state.reveal_step = 0;
            }
        }
        NavigationAction::Reconcile => {}
    }
    state.slide_id = Some(deck.slides[index].id.clone());
    NavigationOutcome {
        changed: before != *state,
        slide_changed: before.slide_id != state.slide_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{DeckBuilder, SlideBuilder};
    #[test]
    fn wire_actions_round_trip_without_a_second_reducer_contract() {
        let action = NavigationAction::GoTo("stable-slide".into());
        let wire = serde_json::to_string(&action).unwrap();
        assert_eq!(
            serde_json::from_str::<NavigationAction>(&wire).unwrap(),
            action
        );
    }
    #[test]
    fn previous_inverts_next_including_slide_boundaries() {
        let deck = DeckBuilder::new("test")
            .slide(
                SlideBuilder::new("a")
                    .text("x", "first")
                    .pause()
                    .text("y", "second")
                    .build(),
            )
            .slide(SlideBuilder::new("b").text("z", "third").build())
            .build()
            .unwrap();
        let mut state = PresentationState::new(&deck);
        for _ in 0..2 {
            let before = state.clone();
            reduce(&deck, &mut state, NavigationAction::Next);
            let advanced = state.clone();
            reduce(&deck, &mut state, NavigationAction::Previous);
            assert_eq!(state, before);
            state = advanced;
        }
    }
    #[test]
    fn reload_preserves_identity_and_clamps_reveal() {
        let mut deck = DeckBuilder::new("d")
            .slide(SlideBuilder::new("a").pause().build())
            .slide(SlideBuilder::new("b").build())
            .build()
            .unwrap();
        let mut state = PresentationState {
            slide_id: Some("a".into()),
            reveal_step: 8,
        };
        deck.slides.reverse();
        reduce(&deck, &mut state, NavigationAction::Reconcile);
        assert_eq!(state.slide_id, Some("a".into()));
        assert_eq!(state.reveal_step, 1);
        deck.slides.clear();
        reduce(&deck, &mut state, NavigationAction::Reconcile);
        assert_eq!(state, PresentationState::default());
    }
}
