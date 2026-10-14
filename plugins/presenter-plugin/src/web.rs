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
                control: None,
                broadcast: Some(crate::broadcast::Source::start(source, paths.clone())),
                scene: bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 80, 24)),
                area: Rect::new(0, 0, 80, 24),
                generation: uuid::Uuid::new_v4().to_string(),
                revision: 0,
            }));
        }
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
            eprintln!("Local browser URL: {}", host.bootstrap_url());
            eprintln!(
                "LAN access: replace the URL host with this computer's IP or DNS name (keep the port and any token fragment)."
            );
        } else {
            eprintln!("Read-only audience browser: {}", host.bootstrap_url());
        }
        Ok(Some(Self {
            only,
            host: Some(host),
            requests: Some(requests),
            publisher: Some(publisher),
            broadcast: options
                .broadcast
                .map(|source| crate::broadcast::Source::start(source, paths.clone())),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn audience_without_listener_has_no_viewport_lease_and_retains_scene() {
        let area = Rect::new(0, 0, 80, 24);
        let mut audience = Browser {
            only: false,
            host: None,
            requests: None,
            publisher: None,
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
