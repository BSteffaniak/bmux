use bmux_presenter::{Block, BlockKind, Deck, Metadata, NavigationAction, Slide, SpeakerNotes};
use bmux_presenter_runtime::{
    Action, Asset, Credential, Host, HostConfig, HostError, InteractionMode, Owner, OwnerContext,
    OwnerUpdate, PROTOCOL_VERSION, Poll, Scopes, Submission, SurfaceInput,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
fn deck() -> Deck {
    Deck {
        id: "test".into(),
        metadata: Metadata::default(),
        slides: vec![
            Slide {
                id: "one".into(),
                blocks: vec![],
                reveal_steps: 1,
                notes: SpeakerNotes {
                    text: "PRIVATE-NOTES".into(),
                    source: None,
                },
            },
            Slide {
                id: "two".into(),
                blocks: vec![],
                reveal_steps: 0,
                notes: SpeakerNotes::default(),
            },
        ],
    }
}
fn join(host: &mut Host, mode: InteractionMode, scopes: Scopes) -> (String, String, String) {
    let credential = Credential::generate(scopes, Duration::from_secs(600));
    let secret = credential.secret.clone();
    let grant = host.add_credential(credential).unwrap();
    let id = host
        .authenticate(PROTOCOL_VERSION, Some(&secret), mode)
        .unwrap();
    (id, secret, grant)
}
#[test]
fn audience_requires_explicit_opt_in_and_never_receives_notes() {
    let mut host = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    assert_eq!(
        host.authenticate(PROTOCOL_VERSION, None, InteractionMode::ReceiveOnly),
        Err(HostError::Authentication)
    );
    let mut host = Host::new(
        deck(),
        vec![],
        HostConfig {
            public_audience: true,
            ..HostConfig::default()
        },
    )
    .unwrap();
    assert_eq!(
        host.authenticate(PROTOCOL_VERSION, None, InteractionMode::Shared),
        Err(HostError::Authentication)
    );
    let id = host
        .authenticate(PROTOCOL_VERSION, None, InteractionMode::ReceiveOnly)
        .unwrap();
    let snapshot = host.snapshot(&id).unwrap();
    assert!(snapshot.notes.is_none());
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("PRIVATE-NOTES")
    );
    let submission = Submission {
        action_id: 1,
        expected: host.cursor(&id).unwrap(),
        action: Action::Navigate(NavigationAction::Next),
    };
    assert_eq!(
        host.submit(&id, submission).unwrap_err(),
        HostError::Unauthorized
    );
}
#[test]
fn submit_only_bootstraps_but_has_no_poll_subscription() {
    let mut host = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    let (id, _, grant) = join(
        &mut host,
        InteractionMode::SubmitOnly,
        Scopes {
            navigate: true,
            ..Scopes::default()
        },
    );
    let initial = host.snapshot(&id).unwrap();
    assert_eq!(host.poll(&id, None).unwrap_err(), HostError::Unauthorized);
    let ack = host
        .submit(
            &id,
            Submission {
                action_id: 1,
                expected: initial.cursor,
                action: Action::Navigate(NavigationAction::Next),
            },
        )
        .unwrap();
    assert_eq!(ack.action_id, 1);
    host.revoke_credential(&grant);
    assert_eq!(host.snapshot(&id).unwrap_err(), HostError::Authentication);
}
struct Counter(Arc<AtomicUsize>);
impl Owner for Counter {
    fn demo_input(
        &mut self,
        _: OwnerContext<'_>,
        _: &SurfaceInput,
    ) -> Result<OwnerUpdate, HostError> {
        Err(HostError::Invalid)
    }
    fn effect(&mut self, _: OwnerContext<'_>, _: &str, _: &str) -> Result<OwnerUpdate, HostError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(HostError::EffectUncertain)
    }
}
#[test]
fn retained_reads_and_receive_only_control_never_execute_owner_effects() {
    let mut host = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    host.set_owner(Box::new(Counter(calls.clone()))).unwrap();
    let (viewer, _, _) = join(
        &mut host,
        InteractionMode::ReceiveOnly,
        Scopes {
            effects: true,
            ..Scopes::default()
        },
    );
    let initial = host.snapshot(&viewer).unwrap();
    for _ in 0..8 {
        let snapshot = host.snapshot(&viewer).unwrap();
        assert_eq!(snapshot.cursor, initial.cursor);
        assert!(matches!(
            host.poll(&viewer, Some(&initial.cursor)).unwrap(),
            Poll::Unchanged(cursor) if cursor == initial.cursor
        ));
        assert!(matches!(
            host.poll(&viewer, None).unwrap(),
            Poll::Snapshot { .. }
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        host.submit(
            &viewer,
            Submission {
                action_id: 1,
                expected: initial.cursor.clone(),
                action: Action::Effect {
                    name: "run".into(),
                    input: "manual".into(),
                },
            },
        )
        .unwrap_err(),
        HostError::Unauthorized
    );
    assert_eq!(host.snapshot(&viewer).unwrap().cursor, initial.cursor);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn effect_scope_is_required_even_for_an_interactive_owner_view() {
    let mut host = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    host.set_owner(Box::new(Counter(calls.clone()))).unwrap();
    let (viewer, _, _) = join(
        &mut host,
        InteractionMode::Shared,
        Scopes {
            demo_input: true,
            navigate: true,
            ..Scopes::default()
        },
    );
    let initial = host.snapshot(&viewer).unwrap();
    assert_eq!(
        host.submit(
            &viewer,
            Submission {
                action_id: 1,
                expected: initial.cursor.clone(),
                action: Action::Effect {
                    name: "run".into(),
                    input: "manual".into(),
                },
            },
        )
        .unwrap_err(),
        HostError::Unauthorized
    );
    assert_eq!(host.snapshot(&viewer).unwrap().cursor, initial.cursor);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

struct ScheduledOwner {
    advances: Arc<AtomicUsize>,
    effects: Arc<AtomicUsize>,
}
impl Owner for ScheduledOwner {
    fn advance(&mut self, _: OwnerContext<'_>) -> Result<bool, HostError> {
        self.advances.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
    fn demo_input(
        &mut self,
        _: OwnerContext<'_>,
        _: &SurfaceInput,
    ) -> Result<OwnerUpdate, HostError> {
        Err(HostError::Unauthorized)
    }
    fn effect(&mut self, _: OwnerContext<'_>, _: &str, _: &str) -> Result<OwnerUpdate, HostError> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        Ok(OwnerUpdate::default())
    }
}
#[test]
fn scheduled_owner_updates_publish_without_read_side_effects() {
    let mut host = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    let advances = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    host.set_owner(Box::new(ScheduledOwner {
        advances: advances.clone(),
        effects: effects.clone(),
    }))
    .unwrap();
    let (viewer, _, _) = join(&mut host, InteractionMode::ReceiveOnly, Scopes::default());
    let initial = host.snapshot(&viewer).unwrap();
    assert_eq!(advances.load(Ordering::SeqCst), 0);
    assert!(host.advance_owner().unwrap());
    assert_eq!(advances.load(Ordering::SeqCst), 1);
    let current = host.snapshot(&viewer).unwrap();
    assert!(current.cursor.revision > initial.cursor.revision);
    assert!(matches!(
        host.poll(&viewer, Some(&current.cursor)).unwrap(),
        Poll::Unchanged(_)
    ));
    assert_eq!(advances.load(Ordering::SeqCst), 1);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}

#[test]
fn uncertain_effect_is_deduplicated_across_reauthentication_and_eviction() {
    let mut config = HostConfig::default();
    config.limits.retained_actions = 1;
    let mut host = Host::new(deck(), vec![], config).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    host.set_owner(Box::new(Counter(calls.clone()))).unwrap();
    let (id, secret, _) = join(
        &mut host,
        InteractionMode::Shared,
        Scopes {
            effects: true,
            navigate: true,
            ..Scopes::default()
        },
    );
    let submission = Submission {
        action_id: 1,
        expected: host.cursor(&id).unwrap(),
        action: Action::Effect {
            name: "demo".into(),
            input: "input".into(),
        },
    };
    assert_eq!(
        host.submit(&id, submission.clone()).unwrap_err(),
        HostError::EffectUncertain
    );
    host.disconnect(&id);
    let id = host
        .authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::Shared)
        .unwrap();
    assert_eq!(
        host.submit(&id, submission.clone()).unwrap_err(),
        HostError::EffectUncertain
    );
    let mut conflicting = submission.clone();
    conflicting.action = Action::Navigate(NavigationAction::Next);
    assert_eq!(
        host.submit(&id, conflicting).unwrap_err(),
        HostError::ActionConflict
    );
    host.submit(
        &id,
        Submission {
            action_id: 2,
            expected: host.cursor(&id).unwrap(),
            action: Action::Navigate(NavigationAction::Next),
        },
    )
    .unwrap();
    assert_eq!(
        host.submit(&id, submission).unwrap_err(),
        HostError::OutcomeEvicted
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[test]
fn shared_ordering_and_independent_state_are_distinct() {
    let mut host = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    let scopes = Scopes {
        navigate: true,
        ..Scopes::default()
    };
    let (a, _, _) = join(&mut host, InteractionMode::Shared, scopes.clone());
    let (b, _, _) = join(&mut host, InteractionMode::Shared, scopes.clone());
    let (c, _, _) = join(&mut host, InteractionMode::Independent, scopes);
    let old = host.cursor(&b).unwrap();
    host.submit(
        &a,
        Submission {
            action_id: 1,
            expected: host.cursor(&a).unwrap(),
            action: Action::Navigate(NavigationAction::Next),
        },
    )
    .unwrap();
    assert_eq!(
        host.submit(
            &b,
            Submission {
                action_id: 1,
                expected: old,
                action: Action::Navigate(NavigationAction::Next)
            }
        )
        .unwrap_err(),
        HostError::Stale
    );
    assert_eq!(host.snapshot(&a).unwrap().state.reveal_step, 1);
    assert_eq!(host.snapshot(&c).unwrap().state.reveal_step, 0);
}
#[test]
fn malformed_owner_scene_and_stale_surface_input_are_rejected() {
    use bmux_presenter_runtime::mirror::{MirrorFrame, WireCell};
    let mut host = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    let (id, _, _) = join(
        &mut host,
        InteractionMode::Shared,
        Scopes {
            demo_input: true,
            ..Scopes::default()
        },
    );
    let expected = host.cursor(&id).unwrap();
    let mut frame = MirrorFrame {
        surface_id: "demo".into(),
        slide_id: "one".into(),
        revision: 7,
        width: 1,
        height: 1,
        cells: vec![WireCell {
            symbol: "a".into(),
            width: 1,
            continuation: false,
            fg: None,
            bg: None,
            modifiers: 0,
        }],
        images: vec![],
    };
    frame.cells[0].symbol = "ab".into();
    assert_eq!(
        host.commit_surfaces(&expected, None, vec![frame.clone()]),
        Err(HostError::Invalid)
    );
    assert_eq!(host.cursor(&id).unwrap(), expected);
    frame.cells[0].symbol = "a".into();
    host.commit_surfaces(&expected, None, vec![frame]).unwrap();
    let stale = Submission {
        action_id: 1,
        expected: host.cursor(&id).unwrap(),
        action: Action::DemoInput(SurfaceInput {
            surface_id: "demo".into(),
            surface_revision: 6,
            x: 0,
            y: 0,
            input: "key".into(),
        }),
    };
    assert_eq!(host.submit(&id, stale).unwrap_err(), HostError::Stale);
    let unknown = Submission {
        action_id: 2,
        expected: host.cursor(&id).unwrap(),
        action: Action::DemoInput(SurfaceInput {
            surface_id: "unknown".into(),
            surface_revision: 7,
            x: 0,
            y: 0,
            input: "key".into(),
        }),
    };
    assert_eq!(host.submit(&id, unknown).unwrap_err(), HostError::Stale);
}

#[test]
fn invalid_reload_is_atomic_and_restart_changes_run() {
    let mut host = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    let (id, _, _) = join(&mut host, InteractionMode::Shared, Scopes::default());
    let before = host.cursor(&id).unwrap();
    let mut invalid = deck();
    invalid.slides[1].id = invalid.slides[0].id.clone();
    assert_eq!(host.reload(invalid, vec![]), Err(HostError::Invalid));
    assert_eq!(host.cursor(&id).unwrap(), before);
    host.reload(deck(), vec![]).unwrap();
    assert!(matches!(
        host.poll(&id, Some(&before)).unwrap(),
        Poll::Snapshot { resync: true, .. }
    ));
    let mut restart = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    let (new_id, _, _) = join(&mut restart, InteractionMode::Shared, Scopes::default());
    assert_ne!(restart.cursor(&new_id).unwrap().run_id, before.run_id);
}
#[test]
fn independent_view_resumes_committed_state_under_same_credential() {
    let mut host = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    let (id, secret, _) = join(
        &mut host,
        InteractionMode::Independent,
        Scopes {
            navigate: true,
            ..Scopes::default()
        },
    );
    host.submit(
        &id,
        Submission {
            action_id: 1,
            expected: host.cursor(&id).unwrap(),
            action: Action::Navigate(NavigationAction::Next),
        },
    )
    .unwrap();
    let before = host.snapshot(&id).unwrap();
    host.disconnect(&id);
    let id = host
        .authenticate(
            PROTOCOL_VERSION,
            Some(&secret),
            InteractionMode::Independent,
        )
        .unwrap();
    let after = host.snapshot(&id).unwrap();
    assert_eq!(after.cursor, before.cursor);
    assert_eq!(after.state, before.state);
    assert_eq!(host.next_action_id(&id).unwrap(), Some(2));
}

#[test]
fn private_paths_and_unreferenced_assets_do_not_enter_audience_wire() {
    let mut deck = deck();
    deck.slides[0].blocks = vec![
        Block {
            id: "image".into(),
            reveal_step: 0,
            source: None,
            kind: BlockKind::Image {
                destination: "/private/home/image.png".into(),
                alt: "picture".into(),
                rows: 4,
            },
        },
        Block {
            id: "text".into(),
            reveal_step: 0,
            source: None,
            kind: BlockKind::Text {
                content: "[label](file:///private/home/secret) ![alt](/private/home/image.png)"
                    .into(),
                format: bmux_presenter::TextFormat::Markdown,
            },
        },
    ];
    let mut host = Host::new(
        deck,
        vec![
            Asset {
                id: "/private/home/image.png".into(),
                media_type: "image/png".into(),
                bytes: vec![1],
            },
            Asset {
                id: "PRIVATE-UNREFERENCED".into(),
                media_type: "text/plain".into(),
                bytes: vec![2],
            },
        ],
        HostConfig {
            public_audience: true,
            ..HostConfig::default()
        },
    )
    .unwrap();
    let id = host
        .authenticate(PROTOCOL_VERSION, None, InteractionMode::ReceiveOnly)
        .unwrap();
    let snapshot = host.snapshot(&id).unwrap();
    let wire = serde_json::to_string(&snapshot).unwrap();
    assert!(!wire.contains("/private/"));
    assert!(!wire.contains("PRIVATE-UNREFERENCED"));
    assert_eq!(snapshot.assets.len(), 1);
    assert_eq!(
        host.asset(&id, "/private/home/image.png", 0, 1)
            .unwrap_err(),
        HostError::NotFound
    );
    assert!(host.asset(&id, &snapshot.assets[0].id, 0, 1).is_ok());
}
