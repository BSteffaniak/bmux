#[cfg(test)]
mod tests {
    use super::*;
    use bmux_presenter::{DeckBuilder, SlideBuilder};
    #[test]
    fn generated_projection_keeps_notes_separate_and_cursor_exact() {
        let mut deck = DeckBuilder::new("d")
            .slide(SlideBuilder::new("s").text("t", "public").build())
            .build()
            .unwrap();
        deck.slides[0].notes.text = "PRIVATE-NOTES".into();
        let s = bmux_presenter_runtime::Snapshot {
            cursor: Cursor {
                run_id: "r".into(),
                deck_revision: 2,
                revision: 3,
            },
            state: bmux_presenter::PresentationState::new(&deck),
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
    fn generated_modes_and_cursors_preserve_runtime_identity() {
        for (wire, runtime) in [
            (
                view::InteractionMode::ReceiveOnly,
                InteractionMode::ReceiveOnly,
            ),
            (
                view::InteractionMode::SubmitOnly,
                InteractionMode::SubmitOnly,
            ),
            (view::InteractionMode::Shared, InteractionMode::Shared),
            (
                view::InteractionMode::Independent,
                InteractionMode::Independent,
            ),
        ] {
            assert_eq!(mode(wire), runtime);
        }
        let cursor = Cursor {
            run_id: "run-not-a-revision".into(),
            deck_revision: u64::MAX - 1,
            revision: u64::MAX,
        };
        assert_eq!(runtime_cursor(wire_cursor(cursor.clone())), cursor);
        let view::Poll::Unchanged { cursor: projected } =
            project_poll(Poll::Unchanged(cursor.clone())).unwrap()
        else {
            panic!("unchanged must not become a snapshot");
        };
        assert_eq!(runtime_cursor(projected), cursor);
    }

    #[test]
    fn generated_actions_preserve_navigation_and_owner_input() {
        for (wire, expected) in [
            (view::Action::Next, NavigationAction::Next),
            (view::Action::Previous, NavigationAction::Previous),
            (view::Action::First, NavigationAction::First),
            (view::Action::Last, NavigationAction::Last),
            (
                view::Action::GoTo {
                    slide_id: "stable-slide".into(),
                },
                NavigationAction::GoTo("stable-slide".into()),
            ),
        ] {
            let Action::Navigate(actual) = action(wire) else {
                panic!("navigation action expected");
            };
            assert_eq!(actual, expected);
        }
        let Action::DemoInput(input) = action(view::Action::DemoInput {
            surface_id: "fixed-canvas".into(),
            surface_revision: u64::MAX,
            x: u16::MAX,
            y: 23,
            input: "click".into(),
        }) else {
            panic!("owner input expected");
        };
        assert_eq!(input.surface_id, "fixed-canvas");
        assert_eq!(input.surface_revision, u64::MAX);
        assert_eq!((input.x, input.y), (u16::MAX, 23));
        assert_eq!(input.input, "click");
        let Action::Effect { name, input } = action(view::Action::Effect {
            name: "owner-only".into(),
            input: "opaque payload".into(),
        }) else {
            panic!("effect expected");
        };
        assert_eq!(
            (name.as_str(), input.as_str()),
            ("owner-only", "opaque payload")
        );
    }

    #[test]
    fn generated_snapshot_matches_typed_public_content_and_mirror_payloads() {
        use bmux_presenter_runtime::{AssetInfo, mirror::MirrorFrame};
        let deck = DeckBuilder::new("parity")
            .slide(
                SlideBuilder::new("slide")
                    .text("text", "visible")
                    .pause()
                    .custom(
                        "control",
                        "toggle",
                        serde_json::json!({"private": "config"}),
                    )
                    .notes("private notes")
                    .build(),
            )
            .build()
            .unwrap();
        let mut state = bmux_presenter::PresentationState::new(&deck);
        state.reveal_step = 1;
        let surfaces = vec![MirrorFrame {
            surface_id: "control".into(),
            slide_id: "slide".into(),
            revision: 81,
            width: 80,
            height: 24,
            cells: vec![],
            images: vec![],
        }];
        let assets = vec![AssetInfo {
            id: "opaque-asset".into(),
            media_type: "image/png".into(),
            bytes: 123,
        }];
        let snapshot = bmux_presenter_runtime::Snapshot {
            cursor: Cursor {
                run_id: "parity-run".into(),
                deck_revision: 17,
                revision: 91,
            },
            deck: deck.clone(),
            state,
            notes: None,
            surfaces: surfaces.clone(),
            assets: assets.clone(),
        };
        for resync in [false, true] {
            let view::Poll::Snapshot {
                resync: actual_resync,
                snapshot: projected,
            } = project_poll(Poll::Snapshot {
                resync,
                snapshot: Box::new(snapshot.clone()),
            })
            .unwrap()
            else {
                panic!("snapshot expected");
            };
            assert_eq!(actual_resync, resync);
            let decoded: bmux_presenter::Deck =
                serde_json::from_slice(&projected.public_deck_json).unwrap();
            assert_eq!(decoded, deck.public());
            assert_eq!(projected.slide_id.as_deref(), Some("slide"));
            assert_eq!(projected.reveal_step, 1);
            assert!(projected.notes.is_none());
            assert_eq!(runtime_cursor(projected.cursor), snapshot.cursor);
            let decoded: Vec<MirrorFrame> =
                serde_json::from_slice(&projected.surfaces_json).unwrap();
            assert_eq!(
                serde_json::to_value(decoded).unwrap(),
                serde_json::to_value(&surfaces).unwrap()
            );
            let decoded: Vec<AssetInfo> = serde_json::from_slice(&projected.assets_json).unwrap();
            assert_eq!(
                serde_json::to_value(decoded).unwrap(),
                serde_json::to_value(&assets).unwrap()
            );
        }
    }

    #[test]
    fn generated_errors_retain_authorization_limits_and_repair_distinctions() {
        for (runtime, expected) in [
            (HostError::Authentication, view::Error::Authentication),
            (HostError::Unauthorized, view::Error::Unauthorized),
            (HostError::Version, view::Error::ProtocolVersion),
            (HostError::Limit, view::Error::Limit),
            (HostError::Stale, view::Error::Stale),
            (HostError::NotFound, view::Error::NotFound),
            (HostError::Invalid, view::Error::Invalid),
        ] {
            assert_eq!(error(runtime), expected);
        }
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
