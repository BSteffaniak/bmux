#[cfg(test)]
mod tests {
    use super::*;
    use bmux_presentation::{DeckBuilder, SlideBuilder};
    #[test]
    fn generated_projection_keeps_notes_separate_and_cursor_exact() {
        let mut deck = DeckBuilder::new("d")
            .slide(SlideBuilder::new("s").text("t", "public").build())
            .build()
            .unwrap();
        deck.slides[0].notes.text = "PRIVATE-NOTES".into();
        let s = bmux_presentation_runtime::Snapshot {
            cursor: Cursor {
                run_id: "r".into(),
                deck_revision: 2,
                revision: 3,
            },
            state: bmux_presentation::PresentationState::new(&deck),
            deck,
            notes: Some("PRIVATE-NOTES".into()),
            surfaces: vec![],
            assets: vec![],
        };
        let view::Poll::Snapshot { snapshot, .. } = project_poll(Poll::Snapshot {
            resync: true,
            snapshot: Box::new(s),
        })
        .unwrap() else {
            panic!("snapshot expected");
        };
        assert!(
            !String::from_utf8(snapshot.public_deck_json)
                .unwrap()
                .contains("PRIVATE-NOTES")
        );
        assert_eq!(snapshot.notes.as_deref(), Some("PRIVATE-NOTES"));
        assert_eq!(snapshot.cursor.deck_revision, 2);
        assert_eq!(snapshot.cursor.revision, 3);
    }
    #[test]
    fn action_failures_are_not_collapsed_into_generic_transport_errors() {
        assert_eq!(
            error(HostError::ActionConflict),
            view::Error::ActionConflict
        );
        assert_eq!(
            error(HostError::OutcomeEvicted),
            view::Error::OutcomeEvicted
        );
        assert_eq!(
            error(HostError::ActionSequence),
            view::Error::ActionSequence
        );
        assert_eq!(
            error(HostError::EffectUncertain),
            view::Error::EffectUncertain
        );
    }
}
