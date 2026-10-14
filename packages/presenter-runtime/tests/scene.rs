use bmux_presenter::{DeckBuilder, NavigationAction, SlideBuilder};
use bmux_presenter_runtime::mirror::{MirrorFrame, WireCell};
use bmux_presenter_runtime::{
    Action, Credential, Host, HostConfig, HostError, InteractionMode, Owner, OwnerContext,
    OwnerUpdate, PROTOCOL_VERSION, Scopes, Submission, SurfaceInput,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Scenes {
    scenes: Arc<AtomicUsize>,
    effects: Arc<AtomicUsize>,
}
impl Owner for Scenes {
    fn prepare_reload(
        &self,
        _deck: &bmux_presenter::Deck,
        _assets: &[bmux_presenter_runtime::Asset],
    ) -> Result<Box<dyn Owner>, HostError> {
        Ok(Box::new(Self {
            scenes: self.scenes.clone(),
            effects: self.effects.clone(),
        }))
    }
    fn scene(&mut self, cx: OwnerContext<'_>) -> Result<Vec<MirrorFrame>, String> {
        self.scenes.fetch_add(1, Ordering::SeqCst);
        Ok(vec![MirrorFrame {
            interaction_revision: None,
            private_scene: None,
            surface_id: "demo".into(),
            slide_id: cx.state.slide_id.as_ref().unwrap().as_str().into(),
            revision: cx.cursor.revision,
            width: 2,
            height: 1,
            cells: vec![
                WireCell {
                    symbol: "x".into(),
                    width: 1,
                    continuation: false,
                    fg: None,
                    bg: None,
                    modifiers: 0
                };
                2
            ],
            images: vec![],
        }])
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
struct Replacement {
    fail_instance: Option<String>,
    scenes: Scenes,
}
impl Owner for Replacement {
    fn scene(&mut self, cx: OwnerContext<'_>) -> Result<Vec<MirrorFrame>, String> {
        if self.fail_instance.as_deref() == Some(cx.instance) {
            return Err("preparation failed".into());
        }
        self.scenes.scene(cx)
    }
    fn demo_input(
        &mut self,
        cx: OwnerContext<'_>,
        input: &SurfaceInput,
    ) -> Result<OwnerUpdate, HostError> {
        self.scenes.demo_input(cx, input)
    }
    fn effect(
        &mut self,
        cx: OwnerContext<'_>,
        name: &str,
        input: &str,
    ) -> Result<OwnerUpdate, HostError> {
        self.scenes.effect(cx, name, input)
    }
}
#[test]
fn replacement_failure_preserves_owner_bundle_cursors_and_all_views() {
    let deck = DeckBuilder::new("old")
        .slide(SlideBuilder::new("a").build())
        .build()
        .unwrap();
    let mut host = Host::new(deck, vec![], HostConfig::default()).unwrap();
    let old_calls = Arc::new(AtomicUsize::new(0));
    let old_effects = Arc::new(AtomicUsize::new(0));
    host.set_owner(Box::new(Scenes {
        scenes: old_calls.clone(),
        effects: old_effects.clone(),
    }))
    .unwrap();
    let credential = Credential::generate(
        Scopes {
            effects: true,
            ..Scopes::default()
        },
        Duration::from_secs(60),
    );
    let secret = credential.secret.clone();
    let grant = host.add_credential(credential).unwrap();
    let shared = host
        .authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::Shared)
        .unwrap();
    let private = host
        .authenticate(
            PROTOCOL_VERSION,
            Some(&secret),
            InteractionMode::Independent,
        )
        .unwrap();
    let before_shared = serde_json::to_value(host.snapshot(&shared).unwrap()).unwrap();
    let before_private = serde_json::to_value(host.snapshot(&private).unwrap()).unwrap();
    let old_count = old_calls.load(Ordering::SeqCst);
    let replacement = DeckBuilder::new("new")
        .slide(SlideBuilder::new("b").build())
        .build()
        .unwrap();
    let new_calls = Arc::new(AtomicUsize::new(0));
    let new_effects = Arc::new(AtomicUsize::new(0));
    // Shared preparation succeeds, then the independent view fails: no partial swap.
    let failing = Replacement {
        fail_instance: Some(grant),
        scenes: Scenes {
            scenes: new_calls.clone(),
            effects: new_effects.clone(),
        },
    };
    assert_eq!(
        host.reload_with_owner(replacement.clone(), vec![], Box::new(failing)),
        Err(HostError::Invalid)
    );
    assert_eq!(
        serde_json::to_value(host.snapshot(&shared).unwrap()).unwrap(),
        before_shared
    );
    assert_eq!(
        serde_json::to_value(host.snapshot(&private).unwrap()).unwrap(),
        before_private
    );
    assert_eq!(old_calls.load(Ordering::SeqCst), old_count);
    host.submit(
        &shared,
        Submission {
            action_id: 1,
            expected: host.cursor(&shared).unwrap(),
            action: Action::Effect {
                name: "test".into(),
                input: String::new(),
            },
        },
    )
    .unwrap();
    assert_eq!(old_effects.load(Ordering::SeqCst), 1);
    assert_eq!(new_effects.load(Ordering::SeqCst), 0);
    host.reload_with_owner(
        replacement,
        vec![],
        Box::new(Replacement {
            fail_instance: None,
            scenes: Scenes {
                scenes: new_calls,
                effects: new_effects.clone(),
            },
        }),
    )
    .unwrap();
    assert_eq!(host.snapshot(&shared).unwrap().deck.id.as_str(), "new");
    assert_eq!(host.snapshot(&private).unwrap().surfaces[0].slide_id, "b");
    host.submit(
        &shared,
        Submission {
            action_id: 2,
            expected: host.cursor(&shared).unwrap(),
            action: Action::Effect {
                name: "test".into(),
                input: String::new(),
            },
        },
    )
    .unwrap();
    assert_eq!(new_effects.load(Ordering::SeqCst), 1);
    assert_eq!(old_effects.load(Ordering::SeqCst), 1);
}

#[test]
fn owner_seeds_navigation_reload_and_independent_scenes_without_effects() {
    let deck = DeckBuilder::new("d")
        .slide(SlideBuilder::new("a").build())
        .slide(SlideBuilder::new("b").build())
        .build()
        .unwrap();
    let mut host = Host::new(deck.clone(), vec![], HostConfig::default()).unwrap();
    let scenes = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    host.set_owner(Box::new(Scenes {
        scenes: scenes.clone(),
        effects: effects.clone(),
    }))
    .unwrap();
    let credential = Credential::generate(
        Scopes {
            navigate: true,
            ..Scopes::default()
        },
        Duration::from_secs(60),
    );
    let secret = credential.secret.clone();
    host.add_credential(credential).unwrap();
    let id = host
        .authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::Shared)
        .unwrap();
    assert_eq!(host.snapshot(&id).unwrap().surfaces[0].slide_id, "a");
    host.submit(
        &id,
        Submission {
            action_id: 1,
            expected: host.cursor(&id).unwrap(),
            action: Action::Navigate(NavigationAction::Next),
        },
    )
    .unwrap();
    assert_eq!(host.snapshot(&id).unwrap().surfaces[0].slide_id, "b");
    let independent = host
        .authenticate(
            PROTOCOL_VERSION,
            Some(&secret),
            InteractionMode::Independent,
        )
        .unwrap();
    assert_eq!(
        host.snapshot(&independent).unwrap().surfaces[0].slide_id,
        "b"
    );
    let count = scenes.load(Ordering::SeqCst);
    let snapshot = host.snapshot(&id).unwrap();
    snapshot.surfaces[0].placement(1, 1);
    snapshot.surfaces[0].placement(100, 40);
    // Client ingestion/painting is derived from the retained owner snapshot,
    // never an owner callback or an alternate product scene authority.
    let retained = snapshot.surfaces[0].retained_surface(1, 100, 8192).unwrap();
    let component = bmux_tui::embedded_surface::EmbeddedSurface::new(&retained);
    use bmux_tui::component::Component;
    let layout = component.layout(
        bmux_tui::component::Constraints::tight(bmux_tui::geometry::Size::new(6, 3)),
        &mut bmux_tui::component::LayoutCx::default(),
    );
    let state =
        bmux_tui::presentation::PresentationState::new(bmux_tui::geometry::Rect::new(0, 0, 6, 3));
    let prepared = state
        .prepare(bmux_tui::damage::Damage::Full, |cx| {
            component.paint(&layout, cx)
        })
        .unwrap();
    assert_eq!(prepared.buffer().cells()[8].symbol, "x");
    assert_eq!(component.input_point(&layout, 2, 1).unwrap().x, 0);
    host.snapshot(&id).unwrap();
    assert_eq!(scenes.load(Ordering::SeqCst), count);
    host.reload(deck, vec![]).unwrap();
    assert!(!host.snapshot(&id).unwrap().surfaces.is_empty());
    assert!(!host.snapshot(&independent).unwrap().surfaces.is_empty());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}
