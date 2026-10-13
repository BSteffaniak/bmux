use bmux_presenter::{Block, BlockKind, Deck, DeckBuilder, SlideBuilder, TextFormat};
use bmux_presenter_runtime::{
    Asset, Credential, Host, HostConfig, HostError, InteractionMode, Owner, OwnerContext,
    OwnerUpdate, PROTOCOL_VERSION, Poll, Scopes, SurfaceInput,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn deck(version: u64) -> Deck {
    let mut deck = DeckBuilder::new("reload")
        .slide(SlideBuilder::new("a").build())
        .build()
        .unwrap();
    deck.slides[0].blocks.push(Block {
        id: "stable".into(),
        reveal_step: 0,
        source: None,
        kind: BlockKind::Custom {
            name: "linked".into(),
            config: serde_json::json!(version),
        },
    });
    deck
}
fn join(host: &mut Host, mode: InteractionMode) -> (String, String) {
    let credential = Credential::generate(Scopes::default(), Duration::from_secs(60));
    let secret = credential.secret.clone();
    host.add_credential(credential).unwrap();
    (
        host.authenticate(PROTOCOL_VERSION, Some(&secret), mode)
            .unwrap(),
        secret,
    )
}

#[test]
fn rich_projection_preserves_structure_and_serves_only_opaque_authorized_images() {
    let mut source = deck(1);
    source.metadata.theme = Some("light".into());
    source.slides[0].blocks.push(Block {
        id: "rich".into(),
        reveal_step: 0,
        source: None,
        kind: BlockKind::Text {
            format: TextFormat::Markdown,
            content: concat!(
                "## Heading\n\n**bold** *emphasis* ~~strike~~ `inline`\n\n",
                "> quote\n\n- [x] task\n\n1. ordered\n\n",
                "| A | B |\n| --- | --- |\n| one | two |\n\n",
                "```rust\nlet x = 1;\n```\n\n",
                "[**label**](/private/link \"private title\") ![*alt*][image]\n\n",
                "[image]: /private/image.png \"private title\"\n\n",
                "[image]: /private/duplicate\n  \"private duplicate title\"\n\n",
                "<file:///private/auto> <span data-private='/private/raw'>visible</span>\n"
            )
            .into(),
        },
    });
    let mut host = Host::new(
        source,
        vec![Asset {
            id: "/private/image.png".into(),
            media_type: "image/png".into(),
            bytes: vec![1],
        }],
        HostConfig::default(),
    )
    .unwrap();
    let (id, _) = join(&mut host, InteractionMode::Shared);
    let snapshot = host.snapshot(&id).unwrap();
    let BlockKind::Text { content, format } = &snapshot.deck.slides[0].blocks[1].kind else {
        panic!("text")
    };
    assert_eq!(*format, TextFormat::Markdown);
    for rich in [
        "## Heading",
        "**bold**",
        "*emphasis*",
        "~~strike~~",
        "`inline`",
        "> quote",
        "- [x] task",
        "1. ordered",
        "| one | two |",
        "```rust",
        "**label**",
        "![*alt*](",
    ] {
        assert!(content.contains(rich), "lost {rich}");
    }
    let wire = serde_json::to_string(&snapshot).unwrap();
    for private in [
        "/private/",
        "private title",
        "data-private",
        "<span",
        "[image]:",
    ] {
        assert!(!wire.contains(private), "leaked {private}");
    }
    assert_eq!(snapshot.deck.metadata.theme.as_deref(), Some("light"));
    assert_eq!(snapshot.assets.len(), 1);
    assert!(content.contains(&snapshot.assets[0].id));
    assert!(matches!(
        host.asset(&id, "/private/image.png", 0, 1),
        Err(HostError::NotFound)
    ));
}

// Test owner implements the application policy, not runtime-owned component state.
// Each map value is isolated; Arc is used only for external observation counters.
struct Retained {
    key: (String, String, u64),
    values: BTreeMap<String, usize>,
    live_calls: Arc<AtomicUsize>,
    candidate_calls: Arc<AtomicUsize>,
    is_candidate: bool,
    fail: bool,
}
impl Owner for Retained {
    fn prepare_reload(&self, deck: &Deck, assets: &[Asset]) -> Result<Box<dyn Owner>, HostError> {
        assert_eq!(
            assets.first().map(|asset| asset.id.as_str()),
            Some("new-assets")
        );
        let block = &deck.slides[0].blocks[0];
        let BlockKind::Custom { name, config } = &block.kind else {
            return Err(HostError::Invalid);
        };
        let key = (block.id.0.clone(), name.clone(), config.as_u64().unwrap());
        if key.2 == 99 {
            return Err(HostError::Invalid);
        }
        Ok(Box::new(Self {
            values: if key == self.key {
                self.values.clone()
            } else {
                BTreeMap::new()
            },
            key,
            live_calls: self.live_calls.clone(),
            candidate_calls: self.candidate_calls.clone(),
            is_candidate: true,
            fail: self.fail,
        }))
    }
    fn scene(
        &mut self,
        _: OwnerContext<'_>,
    ) -> Result<Vec<bmux_presenter_runtime::mirror::MirrorFrame>, String> {
        self.live_calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }
    fn reconcile_reload(
        &mut self,
        cx: OwnerContext<'_>,
    ) -> Result<Vec<bmux_presenter_runtime::mirror::MirrorFrame>, String> {
        assert!(self.is_candidate);
        self.candidate_calls.fetch_add(1, Ordering::SeqCst);
        let expected = if self.key == ("stable".into(), "linked".into(), 1) {
            42
        } else {
            0
        };
        assert_eq!(
            self.values.get(cx.instance).copied().unwrap_or_default(),
            expected
        );
        if self.fail && cx.instance != "shared" {
            return Err("candidate failure".into());
        }
        // Geometry is rebuilt here; this inert fixture has no surfaces.
        Ok(vec![])
    }
    fn demo_input(
        &mut self,
        _: OwnerContext<'_>,
        _: &SurfaceInput,
    ) -> Result<OwnerUpdate, HostError> {
        Err(HostError::Unauthorized)
    }
    fn effect(&mut self, _: OwnerContext<'_>, _: &str, _: &str) -> Result<OwnerUpdate, HostError> {
        Err(HostError::Unauthorized)
    }
}
fn assets() -> Vec<Asset> {
    vec![Asset {
        id: "new-assets".into(),
        media_type: "image/png".into(),
        bytes: vec![1],
    }]
}

#[test]
fn immutable_prepare_retains_or_resets_keys_and_rebuilds_shared_live_and_cached_instances() {
    for (version, fail, block_id, component_type) in [
        (1, false, "stable", "linked"),
        (2, false, "stable", "linked"),
        (1, false, "changed", "linked"),
        (1, false, "stable", "changed"),
        (1, true, "stable", "linked"),
        (99, true, "stable", "linked"),
    ] {
        let mut host = Host::new(deck(1), vec![], HostConfig::default()).unwrap();
        let (shared, _) = join(&mut host, InteractionMode::Shared);
        let (live, _) = join(&mut host, InteractionMode::Independent);
        let (cached, secret) = join(&mut host, InteractionMode::Independent);
        // The application receives principal instance IDs from owner contexts.
        let live_calls = Arc::new(AtomicUsize::new(0));
        let candidate_calls = Arc::new(AtomicUsize::new(0));
        // Capture instance keys in initial scene callbacks using a small wrapper.
        struct Seed {
            inner: Retained,
        }
        impl Owner for Seed {
            fn prepare_reload(
                &self,
                deck: &Deck,
                assets: &[Asset],
            ) -> Result<Box<dyn Owner>, HostError> {
                self.inner.prepare_reload(deck, assets)
            }
            fn scene(
                &mut self,
                cx: OwnerContext<'_>,
            ) -> Result<Vec<bmux_presenter_runtime::mirror::MirrorFrame>, String> {
                self.inner.values.insert(cx.instance.into(), 42);
                self.inner.scene(cx)
            }
            fn demo_input(
                &mut self,
                cx: OwnerContext<'_>,
                input: &SurfaceInput,
            ) -> Result<OwnerUpdate, HostError> {
                self.inner.demo_input(cx, input)
            }
            fn effect(
                &mut self,
                cx: OwnerContext<'_>,
                name: &str,
                input: &str,
            ) -> Result<OwnerUpdate, HostError> {
                self.inner.effect(cx, name, input)
            }
        }
        host.set_owner(Box::new(Seed {
            inner: Retained {
                key: ("stable".into(), "linked".into(), 1),
                values: BTreeMap::new(),
                live_calls: live_calls.clone(),
                candidate_calls: candidate_calls.clone(),
                is_candidate: false,
                fail,
            },
        }))
        .unwrap();
        host.disconnect(&cached);
        let before_shared = serde_json::to_value(host.snapshot(&shared).unwrap()).unwrap();
        let before_live = serde_json::to_value(host.snapshot(&live).unwrap()).unwrap();
        let count = live_calls.load(Ordering::SeqCst);
        let mut next = deck(version);
        next.slides[0].blocks[0].id = block_id.into();
        if let BlockKind::Custom { name, .. } = &mut next.slides[0].blocks[0].kind {
            *name = component_type.into();
        }
        let result = host.reload(next, assets());
        assert_eq!(
            live_calls.load(Ordering::SeqCst),
            count,
            "active owner mutated"
        );
        if fail {
            assert_eq!(result, Err(HostError::Invalid));
            // Preparation failure cannot reach reconciliation; a scene failure
            // stops after shared geometry and the first independent candidate.
            assert_eq!(
                candidate_calls.load(Ordering::SeqCst),
                if version == 99 { 0 } else { 2 }
            );
            assert_eq!(
                serde_json::to_value(host.snapshot(&shared).unwrap()).unwrap(),
                before_shared
            );
            assert_eq!(
                serde_json::to_value(host.snapshot(&live).unwrap()).unwrap(),
                before_live
            );
        } else {
            result.unwrap();
            assert_eq!(candidate_calls.load(Ordering::SeqCst), 3);
            let resumed = host
                .authenticate(
                    PROTOCOL_VERSION,
                    Some(&secret),
                    InteractionMode::Independent,
                )
                .unwrap();
            assert_eq!(host.cursor(&resumed).unwrap().deck_revision, 2);
        }
    }
}

#[test]
fn theme_and_oversized_asset_errors_preserve_snapshot_and_slow_poll_resyncs() {
    let mut config = HostConfig::default();
    config.limits.asset_bytes = 2;
    let mut host = Host::new(deck(1), vec![], config).unwrap();
    let (id, _) = join(&mut host, InteractionMode::Shared);
    let before = host.snapshot(&id).unwrap();
    let mut invalid = deck(1);
    invalid.metadata.theme = Some("/private/theme".into());
    assert_eq!(host.reload(invalid, vec![]), Err(HostError::Invalid));
    let mut missing = deck(1);
    missing.slides[0].blocks.push(Block {
        id: "unauthorized".into(),
        reveal_step: 0,
        source: None,
        kind: BlockKind::Text {
            content: "![image](/private/not-authorized.png)".into(),
            format: TextFormat::Markdown,
        },
    });
    assert_eq!(host.reload(missing, vec![]), Err(HostError::NotFound));
    let mut oversized = assets();
    oversized[0].bytes = vec![0; 3];
    assert_eq!(host.reload(deck(1), oversized), Err(HostError::Limit));
    assert_eq!(
        serde_json::to_value(host.snapshot(&id).unwrap()).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    for _ in 0..4 {
        host.reload(deck(1), vec![]).unwrap();
    }
    assert!(matches!(
        host.poll(&id, Some(&before.cursor)).unwrap(),
        Poll::Snapshot { resync: true, .. }
    ));
    assert_eq!(
        host.authenticate(1, None, InteractionMode::ReceiveOnly),
        Err(HostError::Version)
    );
}
