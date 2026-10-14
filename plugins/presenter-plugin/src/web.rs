//! Presenter-owned lifecycle over the shared neutral browser publisher.
use anyhow::{Context, Result};
use bmux_config::PresenterWebMode;
use bmux_tui::geometry::Rect;

pub(crate) struct Browser {
    pub only: bool,
    host: Option<bmux_web_host::HostHandle>,
    requests: Option<bmux_web_host::ViewportRequests>,
    publisher: Option<bmux_web_host::BrowserPublisher>,
    broadcast: Option<crate::broadcast::Source>,
    documents: Option<bmux_web_host::SnapshotPublisher>,
    projection: Option<AudienceProjection>,
    scene: bmux_tui::presentation::PresentationState,
    area: Rect,
    control: Option<bmux_web_host::ViewControl>,
    generation: String,
    revision: u64,
}

pub(crate) fn mode(
    options: &crate::cli::WebOptions,
    configured: PresenterWebMode,
    environment: std::result::Result<String, std::env::VarError>,
) -> Result<PresenterWebMode> {
    if options.only {
        return Ok(PresenterWebMode::Only);
    }
    if options.enabled || options.assets.is_some() {
        return Ok(PresenterWebMode::Mirror);
    }
    match environment {
        Ok(value) => match value.as_str() {
            "off" => Ok(PresenterWebMode::Off),
            "mirror" => Ok(PresenterWebMode::Mirror),
            "only" => Ok(PresenterWebMode::Only),
            _ => anyhow::bail!("BMUX_PRESENTER_WEB_MODE must be off, mirror or only"),
        },
        Err(std::env::VarError::NotPresent) => Ok(configured),
        Err(error) => Err(error.into()),
    }
}

impl Browser {
    pub async fn start(
        options: crate::cli::WebOptions,
        paths: &bmux_config::ConfigPaths,
    ) -> Result<Option<Self>> {
        let config = bmux_config::BmuxConfig::load_with_paths_and_overrides(
            paths,
            &bmux_config::ConfigLoadOverrides::for_process(),
        )?;
        let mode = mode(
            &options,
            config.presenter.web.mode,
            std::env::var("BMUX_PRESENTER_WEB_MODE"),
        )?;
        if mode == PresenterWebMode::Off {
            return Ok(options.broadcast.map(|source| Self {
                only: false,
                host: None,
                requests: None,
                publisher: None,
                documents: None,
                projection: None,
                control: None,
                broadcast: Some(crate::broadcast::Source::start(source, paths.clone())),
                scene: bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 80, 24)),
                area: Rect::new(0, 0, 80, 24),
                generation: uuid::Uuid::new_v4().to_string(),
                revision: 0,
            }));
        }
        let fixed = match std::env::var("BMUX_PRESENTER_WEB_DELIVERY").as_deref() {
            Ok("frame") => true,
            Ok("document") | Err(std::env::VarError::NotPresent) => false,
            _ => anyhow::bail!("BMUX_PRESENTER_WEB_DELIVERY must be document or frame"),
        };
        let only = mode == PresenterWebMode::Only;
        let assets = bmux_config::web::assets(options.assets, config.presenter.web.assets);
        let cache_root = bmux_config::web::cache_root(paths);
        let settings = bmux_config::web::resolve(
            config.web,
            bmux_config::web::WebOptions {
                port: options.port,
                auth: options.auth.as_deref(),
                host: options.host,
                public_host: options.public_host,
                allowed_hosts: options.allowed_hosts.as_deref(),
            },
        )?;
        let assets = bmux_web_host::resolve_assets_with_cache(assets, cache_root)
            .await
            .context("presenter browser asset setup")?;
        let mut config = bmux_web_host::HostConfig::new(assets);
        config.port = settings.port;
        config.host = settings.host;
        config.public_host = settings.public_host;
        config.allowed_hosts = settings.allowed_hosts;
        config.auth_mode = match settings.auth {
            bmux_config::WebAuth::None => bmux_web_host::AuthMode::None,
            bmux_config::WebAuth::Token => bmux_web_host::AuthMode::Token,
        };
        config.viewport_mode = if only {
            bmux_web_host::ViewportMode::Resizable
        } else {
            bmux_web_host::ViewportMode::Observer
        };
        let (publisher, feed) = bmux_web_host::snapshot_feed(config.limits.max_snapshot_bytes)?;
        let mut host = bmux_web_host::start(config, feed).await?;
        let requests = host
            .take_viewport_requests()
            .context("browser viewport channel")?;
        let control = host.view_control();
        let documents = publisher.clone();
        let publisher = bmux_web_host::BrowserPublisher::new(publisher, control.clone());
        if host.address().ip().is_unspecified() {
            eprintln!(
                "Listening on {} (all {} interfaces)",
                host.address(),
                if host.address().is_ipv4() {
                    "IPv4"
                } else {
                    "IPv6"
                }
            );
            eprintln!(
                "Local browser URL: {}",
                audience_url(&host.bootstrap_url(), fixed)
            );
            eprintln!(
                "LAN access: replace the URL host with this computer's IP or DNS name (keep the port and any token fragment)."
            );
        } else {
            eprintln!(
                "Read-only audience browser: {}",
                audience_url(&host.bootstrap_url(), fixed)
            );
        }
        Ok(Some(Self {
            only,
            host: Some(host),
            requests: Some(requests),
            publisher: fixed.then_some(publisher),
            broadcast: options
                .broadcast
                .map(|source| crate::broadcast::Source::start(source, paths.clone())),
            documents: (!fixed).then_some(documents),
            projection: None,
            scene: bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 80, 24)),
            area: Rect::new(0, 0, 80, 24),
            control: Some(control),
            generation: uuid::Uuid::new_v4().to_string(),
            revision: 0,
        }))
    }

    pub fn publish(
        &mut self,
        player: &mut crate::player::Player,
        width: u16,
        height: u16,
    ) -> Result<()> {
        if width == 0 || height == 0 || usize::from(width) * usize::from(height) > 262_144 {
            if let Some(control) = &self.control {
                control.set_presentation_status(Some("browser presentation rejected"));
            }
            anyhow::bail!("audience viewport exceeds cell budget");
        }
        if let Some(documents) = &self.documents {
            let revision = self
                .revision
                .checked_add(1)
                .context("audience revision exhausted")?;
            let (document, resources) =
                audience_document(player, &self.generation, revision, &mut self.projection)?;
            documents.publish_document(
                revision,
                bmux_web_host::DocumentPublication {
                    generation: self.generation.clone(),
                    revision,
                    capability: bmux_presenter::audience::CAPABILITY.into(),
                    content: std::sync::Arc::from(serde_json::to_vec(&document)?),
                },
                resources,
            )?;
            if let Some(control) = &self.control {
                control.set_generation(&self.generation)?;
                control.set_presentation_status(None);
            }
            // A document-mode listener and a broadcast have independent transports:
            // the broadcast still receives the audience-only retained raster below.
            if self.broadcast.is_none() {
                self.revision = revision;
                return Ok(());
            }
        }
        let area = Rect::new(0, 0, width, height);
        let staged =
            (area != self.area).then(|| bmux_tui::presentation::PresentationState::new(area));
        let scene = staged.as_ref().unwrap_or(&self.scene);
        let prepared = scene
            .prepare_bounded(
                bmux_tui::presentation::PresentationLimits {
                    max_cells: 262_144,
                    max_entries: 262_144,
                    max_bytes: 32 * 1024 * 1024,
                },
                bmux_tui::damage::Damage::Full,
                |cx| crate::player::paint_browser_audience(player, cx),
            )
            .inspect_err(|_| {
                if let Some(control) = &self.control {
                    control.set_presentation_status(Some("browser presentation rejected"));
                }
            })?;
        let revision = self
            .revision
            .checked_add(1)
            .context("audience revision exhausted")?;
        // Publish before replacing retained authority, especially for a rejected resize.
        if let Some(prepared) = prepared.as_ref() {
            if let Some(publisher) = &mut self.publisher {
                publisher.publish_prepared(&self.generation, revision, prepared)?;
            }
            if let Some(source) = &self.broadcast {
                // Conversion uses the exact audience-only prepared compositor scene.
                // Encoding and all service IO happen in the asynchronous source task.
                match bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
                    &self.generation,
                    revision,
                    prepared,
                ) {
                    Ok(frame) => source.publish(frame),
                    Err(_) => eprintln!(
                        "Broadcast audience preparation rejected; local playback continues"
                    ),
                }
            }
        }
        if let Some(staged) = staged {
            self.scene = staged;
        }
        self.scene
            .present_prepared(prepared, |_, _| Ok::<_, anyhow::Error>(()))?;
        crate::player::commit_browser_view(player);
        self.area = area;
        self.revision = revision;
        Ok(())
    }

    pub fn broadcast_only(&self) -> bool {
        self.host.is_none()
    }

    pub async fn viewport_request(&mut self) -> Option<bmux_web_host::ViewportRequest> {
        match &mut self.requests {
            Some(requests) => requests.recv().await,
            None => std::future::pending().await,
        }
    }

    pub fn viewport_is_current(&self, request: &bmux_web_host::ViewportRequest) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|requests| requests.is_current(request))
    }

    pub async fn shutdown(self) {
        if let Some(source) = self.broadcast {
            source.shutdown().await;
        }
        if let Some(host) = self.host {
            host.shutdown().await;
        }
    }
}

fn audience_url(url: &str, fixed: bool) -> String {
    let (base, fragment) = url
        .split_once('#')
        .map_or((url, None), |(base, fragment)| (base, Some(fragment)));
    let delivery = if fixed {
        "?delivery=frame"
    } else {
        "?delivery=document&adapter=/presenter-adapter.js"
    };
    format!(
        "{base}{delivery}{}",
        fragment.map_or(String::new(), |fragment| format!("#{fragment}"))
    )
}
struct AudienceProjection {
    source: bmux_presenter::Deck,
    state: bmux_presenter::PresentationState,
    images: std::sync::Arc<std::collections::BTreeMap<String, bmux_tui::image::ImagePayload>>,
    deck: bmux_presenter::Deck,
    assets: std::collections::BTreeMap<String, String>,
    resources: std::collections::BTreeMap<String, bmux_web_host::Resource>,
}

/// Visibility is an export boundary, not merely a browser paint preference.
/// Filter before canonical privacy/Markdown projection so its resource manifest
/// contains only destinations reachable from the published slide/reveal.
fn visible_audience_deck(
    deck: &bmux_presenter::Deck,
    state: &bmux_presenter::PresentationState,
) -> bmux_presenter::Deck {
    fn filter(blocks: &mut Vec<bmux_presenter::Block>, reveal: u32) {
        blocks.retain(|block| block.reveal_step <= reveal);
        for block in blocks {
            if let bmux_presenter::BlockKind::Columns { columns } = &mut block.kind {
                for column in columns {
                    filter(&mut column.blocks, reveal);
                }
            }
        }
    }
    let mut published = deck.clone();
    published
        .slides
        .retain(|slide| state.slide_id.as_ref() == Some(&slide.id));
    for slide in &mut published.slides {
        filter(&mut slide.blocks, state.reveal_step);
    }
    published
}

fn visible_audience_surfaces(
    player: &mut crate::player::Player,
    deck: &bmux_presenter::Deck,
    generation: &str,
    revision: u64,
) -> Result<Vec<bmux_presenter::audience::AudienceSurface>> {
    fn collect(blocks: &[bmux_presenter::Block], ids: &mut std::collections::BTreeSet<String>) {
        for block in blocks {
            ids.insert(block.id.as_str().to_owned());
            if let bmux_presenter::BlockKind::Columns { columns } = &block.kind {
                for column in columns {
                    collect(&column.blocks, ids);
                }
            }
        }
    }
    let mut ids = std::collections::BTreeSet::new();
    for slide in &deck.slides {
        collect(&slide.blocks, &mut ids);
    }
    Ok(
        crate::player::audience_surfaces(player, generation, revision)?
            .into_iter()
            .filter(|surface| ids.contains(&surface.block_id))
            .collect(),
    )
}

fn audience_document(
    player: &mut crate::player::Player,
    generation: &str,
    revision: u64,
    cache: &mut Option<AudienceProjection>,
) -> Result<(
    bmux_presenter::audience::AudienceDocument,
    std::collections::BTreeMap<String, bmux_web_host::Resource>,
)> {
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    if let Some(projection) = cache.as_ref().filter(|projection| {
        projection.source == player.deck
            && projection.state == player.state
            && std::sync::Arc::ptr_eq(&projection.images, &player.rich.images)
    }) {
        let document = bmux_presenter::audience::AudienceDocument {
            deck: projection.deck.clone(),
            state: player.state.clone(),
            assets: projection.assets.clone(),
            surfaces: visible_audience_surfaces(player, &projection.deck, generation, revision)?,
        };
        document.validate().map_err(anyhow::Error::msg)?;
        return Ok((document, projection.resources.clone()));
    }
    let assets = player
        .rich
        .images
        .iter()
        .map(|(id, payload)| {
            let bmux_tui::image::ImagePayload::Png { bytes, .. } = payload else {
                anyhow::bail!("semantic asset must be prepared PNG");
            };
            Ok(bmux_presenter_runtime::Asset {
                id: id.clone(),
                media_type: "image/png".into(),
                bytes: bytes.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    // Reuse the canonical audience privacy projection; no owner is attached and
    // snapshot/asset access cannot start or advance executable work.
    let mut host = bmux_presenter_runtime::Host::new(
        visible_audience_deck(&player.deck, &player.state),
        assets,
        bmux_presenter_runtime::HostConfig {
            public_audience: true,
            limits: bmux_presenter_runtime::Limits {
                requests_per_second: 4096,
                ..Default::default()
            },
        },
    )?;
    let participant = host.authenticate(
        bmux_presenter_runtime::PROTOCOL_VERSION,
        None,
        bmux_presenter_runtime::InteractionMode::ReceiveOnly,
    )?;
    let snapshot = host.snapshot(&participant)?;
    let mut mapping = BTreeMap::new();
    let mut resources = BTreeMap::new();
    for asset in &snapshot.assets {
        let mut bytes = Vec::with_capacity(asset.bytes);
        while bytes.len() < asset.bytes {
            let response = host.asset(&participant, &asset.id, bytes.len(), 64 * 1024)?;
            let bmux_presenter_runtime::Response::Asset { bytes: chunk, .. } = response else {
                anyhow::bail!("invalid projected asset response");
            };
            anyhow::ensure!(
                !chunk.is_empty() && bytes.len() + chunk.len() <= asset.bytes,
                "invalid projected asset size"
            );
            bytes.extend(chunk);
        }
        let id = Sha256::digest(&bytes)
            .iter()
            .flat_map(|byte| {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                [
                    char::from(HEX[usize::from(byte >> 4)]),
                    char::from(HEX[usize::from(byte & 15)]),
                ]
            })
            .collect::<String>();
        mapping.insert(asset.id.clone(), id.clone());
        resources.insert(
            id,
            bmux_web_host::Resource {
                bytes: std::sync::Arc::from(bytes),
                mime_type: asset.media_type.clone(),
            },
        );
    }
    let surfaces = visible_audience_surfaces(player, &snapshot.deck, generation, revision)?;
    let document = bmux_presenter::audience::AudienceDocument {
        deck: snapshot.deck,
        state: player.state.clone(),
        assets: mapping,
        surfaces,
    };
    document.validate().map_err(anyhow::Error::msg)?;
    *cache = Some(AudienceProjection {
        source: player.deck.clone(),
        state: player.state.clone(),
        images: player.rich.images.clone(),
        deck: document.deck.clone(),
        assets: document.assets.clone(),
        resources: resources.clone(),
    });
    Ok((document, resources))
}

#[cfg(test)]
mod tests {
    #[test]
    fn semantic_export_contains_only_current_slide_reveal_and_reachable_resources() {
        use bmux_presenter::{Block, BlockKind, Column, DeckBuilder, SlideBuilder};
        let image = |destination: &str| BlockKind::Image {
            destination: destination.into(),
            alt: "diagram".into(),
            rows: 3,
        };
        let nested = vec![
            Block {
                id: "nested-hidden".into(),
                reveal_step: 1,
                source: None,
                kind: BlockKind::Text {
                    content: "NESTED_HIDDEN_PROSE".into(),
                    format: bmux_presenter::TextFormat::Plain,
                },
            },
            Block {
                id: "nested-image".into(),
                reveal_step: 1,
                source: None,
                kind: image("hidden-image"),
            },
            Block {
                id: "hidden-custom".into(),
                reveal_step: 1,
                source: None,
                kind: BlockKind::Custom {
                    name: "counter".into(),
                    config: serde_json::json!({"secret":"PRIVATE_CONFIG"}),
                },
            },
        ];
        let mut deck = DeckBuilder::new("visibility")
            .slide(
                SlideBuilder::new("first")
                    .text("public", "VISIBLE_PROSE")
                    .columns(
                        "columns",
                        vec![Column {
                            weight: 1,
                            blocks: nested,
                        }],
                    )
                    .pause()
                    .text("hidden", "HIDDEN_PROSE")
                    .markdown("hidden-markdown", "![hidden](hidden-markdown-image)")
                    .notes("PRIVATE_NOTES")
                    .build(),
            )
            .slide(
                SlideBuilder::new("future")
                    .text("future-text", "FUTURE_SLIDE_PROSE")
                    .block("future-image", image("future-image"))
                    .build(),
            )
            .build()
            .unwrap();
        deck.slides[0].blocks[0].source = Some(bmux_presenter::SourceLocation {
            source: "/private/author/deck.md".into(),
            start: 0,
            end: 1,
            line: 1,
            column: 1,
        });
        let mut player = crate::player::Player::local(deck, None, true);
        let png = include_bytes!("../../../packages/presenter/examples/architecture.png");
        player.rich = crate::rich::RichAssets::prepare(
            &["hidden-image", "hidden-markdown-image", "future-image"].map(|id| {
                bmux_presenter_runtime::Asset {
                    id: id.into(),
                    media_type: "image/png".into(),
                    bytes: png.to_vec(),
                }
            }),
        )
        .unwrap();
        let mut cache = None;
        let (before, resources) = audience_document(&mut player, "run", 1, &mut cache).unwrap();
        let encoded = serde_json::to_string(&before).unwrap();
        for secret in [
            "HIDDEN_PROSE",
            "hidden-image",
            "hidden-markdown",
            "hidden-custom",
            "FUTURE_SLIDE_PROSE",
            "future-image",
            "PRIVATE_NOTES",
            "/private/author",
            "PRIVATE_CONFIG",
        ] {
            assert!(!encoded.contains(secret), "unpublished content: {secret}");
        }
        assert_eq!(before.deck.slides.len(), 1);
        assert_eq!(before.deck.slides[0].id.as_str(), "first");
        assert_eq!(before.state.slide_id, Some("first".into()));
        assert!(before.assets.is_empty());
        assert!(before.surfaces.is_empty());
        assert!(resources.is_empty());
        player.state.reveal_step = 1;
        let (revealed, resources) = audience_document(&mut player, "run", 2, &mut cache).unwrap();
        let encoded = serde_json::to_string(&revealed).unwrap();
        assert!(encoded.contains("HIDDEN_PROSE"));
        assert!(encoded.contains("NESTED_HIDDEN_PROSE"));
        assert!(encoded.contains("hidden-markdown"));
        assert!(!encoded.contains("FUTURE_SLIDE_PROSE"));
        assert!(!encoded.contains("PRIVATE_CONFIG"));
        assert_eq!(revealed.assets.len(), 2);
        assert_eq!(resources.len(), 1); // identical PNGs deduplicate by content digest
        assert_eq!(revealed.surfaces[0].block_id, "hidden-custom");
        for resource in revealed.assets.values() {
            assert!(resources.contains_key(resource));
        }
        player.state.slide_id = Some("future".into());
        player.state.reveal_step = 0;
        let (next, resources) = audience_document(&mut player, "run", 3, &mut cache).unwrap();
        let encoded = serde_json::to_string(&next).unwrap();
        assert!(encoded.contains("FUTURE_SLIDE_PROSE"));
        assert!(!encoded.contains("HIDDEN_PROSE"));
        assert!(!encoded.contains("hidden-markdown"));
        assert!(!encoded.contains("hidden-custom"));
        assert_eq!(next.assets.len(), 1);
        assert_eq!(resources.len(), 1);
        assert!(next.surfaces.is_empty());
        player.state.slide_id = Some("first".into());
        let (returned, resources) = audience_document(&mut player, "run", 4, &mut cache).unwrap();
        assert!(returned.assets.is_empty());
        assert!(returned.surfaces.is_empty());
        assert!(resources.is_empty());
    }
    #[test]
    fn semantic_url_preserves_auth_fragment_and_selects_adapter() {
        assert_eq!(
            audience_url("http://localhost:80/#token=secret", false),
            "http://localhost:80/?delivery=document&adapter=/presenter-adapter.js#token=secret"
        );
        assert_eq!(
            audience_url("http://localhost:80/", true),
            "http://localhost:80/?delivery=frame"
        );
    }

    #[test]
    fn semantic_projection_removes_private_config_and_reuses_resources() {
        let deck = bmux_presenter::DeckBuilder::new("semantic")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .text("body", "PUBLIC")
                    .notes("PRIVATE_NOTES")
                    .custom(
                        "demo",
                        "counter",
                        serde_json::json!({"private":"PRIVATE_CONFIG"}),
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = crate::player::Player::local(deck, None, true);
        let mut cache = None;
        let (document, resources) = audience_document(&mut player, "run", 1, &mut cache).unwrap();
        let encoded = serde_json::to_string(&document).unwrap();
        assert!(!encoded.contains("PRIVATE_NOTES"));
        assert!(!encoded.contains("PRIVATE_CONFIG"));
        assert_eq!(document.surfaces[0].block_id, "demo");
        assert!(resources.is_empty());
        let (_, reused) = audience_document(&mut player, "run", 2, &mut cache).unwrap();
        assert!(reused.is_empty());
        assert!(cache.is_some());
    }
    use super::*;

    #[tokio::test]
    async fn document_listener_does_not_bypass_broadcast_raster_preparation() {
        let root =
            std::env::temp_dir().join(format!("bmux-broadcast-test-{}", uuid::Uuid::new_v4()));
        let paths = bmux_config::ConfigPaths::new(
            root.join("config"),
            root.join("runtime"),
            root.join("data"),
            root.join("state"),
        );
        let (documents, feed) = bmux_web_host::snapshot_feed(32 * 1024 * 1024).unwrap();
        let subscription = feed.subscribe_committed();
        let area = Rect::new(0, 0, 80, 24);
        let mut audience = Browser {
            only: false,
            host: None,
            requests: None,
            publisher: None,
            documents: Some(documents),
            projection: None,
            broadcast: Some(crate::broadcast::Source::start(uuid::Uuid::new_v4(), paths)),
            control: None,
            scene: bmux_tui::presentation::PresentationState::new(area),
            area,
            generation: "document-and-broadcast-test".into(),
            revision: 0,
        };
        let deck = bmux_presenter::DeckBuilder::new("deck")
            .slide(
                bmux_presenter::SlideBuilder::new("slide")
                    .text("body", "PUBLIC")
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = crate::player::Player::local(deck, None, true);
        audience.publish(&mut player, 100, 30).unwrap();
        assert_eq!(audience.area, Rect::new(0, 0, 100, 30));
        assert_eq!(audience.revision, 1);
        assert_eq!(subscription.latest().unwrap().revision, 1);
        assert!(subscription.latest().unwrap().binary().is_none());
        audience.publish(&mut player, 120, 40).unwrap();
        assert_eq!(audience.area, Rect::new(0, 0, 120, 40));
        assert_eq!(audience.revision, 2);
        assert_eq!(subscription.latest().unwrap().revision, 2);
        audience.shutdown().await;
    }

    #[tokio::test]
    async fn audience_without_listener_has_no_viewport_lease_and_retains_scene() {
        let area = Rect::new(0, 0, 80, 24);
        let mut audience = Browser {
            only: false,
            host: None,
            requests: None,
            publisher: None,
            documents: None,
            projection: None,
            broadcast: None,
            control: None,
            scene: bmux_tui::presentation::PresentationState::new(area),
            area,
            generation: "broadcast-only-test".into(),
            revision: 0,
        };
        assert!(audience.broadcast_only());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(1),
                audience.viewport_request()
            )
            .await
            .is_err()
        );
        let deck = bmux_presenter::DeckBuilder::new("deck")
            .slide(
                bmux_presenter::SlideBuilder::new("slide")
                    .text("body", "PUBLIC")
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = crate::player::Player::local(deck, None, true);
        audience.publish(&mut player, 80, 24).unwrap();
        assert_eq!(audience.revision, 1);
        assert_eq!(audience.area, area);
        assert!(player.presenter);
        audience.shutdown().await;
    }

    #[test]
    fn explicit_mode_precedence_and_network_only_nonactivation() {
        let mut options = crate::cli::WebOptions {
            port: Some(8080),
            auth: Some("token".into()),
            host: Some("0.0.0.0".parse().unwrap()),
            public_host: Some("127.0.0.1".parse().unwrap()),
            allowed_hosts: Some(vec!["localhost:8080".into()]),
            ..Default::default()
        };
        assert_eq!(
            mode(
                &options,
                PresenterWebMode::Off,
                Err(std::env::VarError::NotPresent)
            )
            .unwrap(),
            PresenterWebMode::Off
        );
        assert_eq!(
            mode(&options, PresenterWebMode::Mirror, Ok("only".into())).unwrap(),
            PresenterWebMode::Only
        );
        assert!(mode(&options, PresenterWebMode::Off, Ok("invalid".into())).is_err());
        options.enabled = true;
        assert_eq!(
            mode(&options, PresenterWebMode::Only, Ok("off".into())).unwrap(),
            PresenterWebMode::Mirror
        );
        assert!(options.assets.is_none());
        options.only = true;
        assert_eq!(
            mode(&options, PresenterWebMode::Off, Ok("invalid".into())).unwrap(),
            PresenterWebMode::Only
        );
        options.enabled = false;
        options.only = false;
        options.assets = Some("browser".into());
        assert_eq!(
            mode(&options, PresenterWebMode::Only, Ok("off".into())).unwrap(),
            PresenterWebMode::Mirror
        );
        options.only = true;
        assert_eq!(
            mode(&options, PresenterWebMode::Off, Ok("invalid".into())).unwrap(),
            PresenterWebMode::Only
        );
        let config: bmux_config::BmuxConfig = bmux_config::BmuxConfig::default();
        assert_eq!(config.presenter.web.mode, PresenterWebMode::Off);
        assert!(config.presenter.web.assets.is_none());
    }

    #[tokio::test]
    async fn rejected_resize_retains_scene_and_next_update_recovers() {
        let directory =
            std::env::temp_dir().join(format!("bmux-presenter-web-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("index.html"), "audience").unwrap();
        let mut config = bmux_web_host::HostConfig::new(directory.clone());
        config.assets = vec![bmux_web_host::Asset {
            url: "/".into(),
            filename: "index.html".into(),
            content_type: bmux_web_host::ContentType::Html,
        }];
        config.port = 0;
        config.viewport_mode = bmux_web_host::ViewportMode::Resizable;
        let (publisher, feed) =
            bmux_web_host::snapshot_feed(config.limits.max_snapshot_bytes).unwrap();
        let mut host = bmux_web_host::start(config, feed).await.unwrap();
        let control = host.view_control();
        let requests = host.take_viewport_requests().unwrap();
        let mut browser = Browser {
            only: true,
            publisher: Some(bmux_web_host::BrowserPublisher::new(
                publisher,
                control.clone(),
            )),
            documents: None,
            projection: None,
            host: Some(host),
            requests: Some(requests),
            control: Some(control),
            broadcast: None,
            area: Rect::new(0, 0, 80, 24),
            scene: bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 80, 24)),
            generation: "test-audience".into(),
            revision: 0,
        };
        let deck = bmux_presenter::DeckBuilder::new("deck")
            .slide(
                bmux_presenter::SlideBuilder::new("slide")
                    .text("body", "PUBLIC")
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = crate::player::Player::local(deck, None, true);
        browser.publish(&mut player, 80, 24).unwrap();
        assert!(browser.publish(&mut player, u16::MAX, u16::MAX).is_err());
        assert_eq!(browser.revision, 1);
        assert_eq!(browser.area, Rect::new(0, 0, 80, 24));
        browser.publish(&mut player, 40, 20).unwrap();
        assert_eq!(browser.revision, 2);
        assert_eq!(browser.area, Rect::new(0, 0, 40, 20));
        assert!(player.presenter);
        browser.shutdown().await;
        std::fs::remove_file(directory.join("index.html")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
