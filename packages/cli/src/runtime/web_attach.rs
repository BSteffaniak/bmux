//! Local read-only browser attachment and explicit terminal mirroring.
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use bmux_web_renderer::{BrowserFrame, SurfaceProvenance};

use super::attach::observer::{AttachSnapshot, attach_observer_channel};
use super::{ConnectionContext, ConnectionPolicyScope, connect_with_context};

fn generation(snapshot: &AttachSnapshot) -> String {
    format!("{}:{}", snapshot.epoch, snapshot.attached_id)
}

fn project_frame(snapshot: &AttachSnapshot) -> Result<BrowserFrame> {
    let surfaces = snapshot
        .surfaces
        .iter()
        .map(|surface| SurfaceProvenance {
            retained_id: surface.retained_id.to_string(),
            owner_plugin_id: surface.owner_plugin_id.clone(),
            owner_local_id: surface.owner_local_id.clone(),
            revision: surface.revision,
            rect: surface.rect,
            clip: surface.clip,
            layer: surface.layer,
            z: surface.z,
            content_id: surface.content_id.map(|id| id.to_string()),
            stream_end: surface.stream_end,
            selection_available: surface.selection_available,
            semantics_available: surface.semantics_available,
        })
        .collect();
    let frame = BrowserFrame::from_prepared_in_generation(
        &generation(snapshot),
        snapshot.revision,
        &snapshot.prepared,
    )?
    .with_surfaces(surfaces)?
    .with_selection_text(&snapshot.selection_text)?
    .with_unsupported_metadata(
        &snapshot
            .unsupported_metadata
            .iter()
            .map(|kind| match *kind {
                "selection" => Ok(bmux_web_renderer::MetadataKind::Selection),
                "semantics" => Ok(bmux_web_renderer::MetadataKind::Semantics),
                _ => Err(anyhow::anyhow!("unknown native metadata availability")),
            })
            .collect::<Result<Vec<_>>>()?,
    )?;
    Ok(frame)
}

fn start_browser_observer(
    feed_publisher: bmux_web_host::SnapshotPublisher,
    control: bmux_web_host::ViewControl,
) -> (
    super::attach::observer::AttachObserverPublisher,
    super::attach::observer::AttachObserver,
) {
    let (mut publisher, observer) = attach_observer_channel();
    let epoch = observer.epoch();
    let mut revision = 0_u64;
    let mut previous: Option<BrowserFrame> = None;
    publisher.set_delivery(move |snapshot| {
        anyhow::ensure!(
            snapshot.epoch == epoch,
            "browser observer epoch changed unexpectedly"
        );
        let frame = project_frame(snapshot).context("browser presentation rejected")?;
        let next_revision = revision
            .checked_add(1)
            .context("browser transport revision exhausted")?;
        let binary = bmux_web_renderer::wire::encode_binary_with_resources(&frame)?;
        let delta = previous
            .as_ref()
            .filter(|base| {
                base.generation() == frame.generation()
                    && base.buffer().area() == frame.buffer().area()
            })
            .map(|base| bmux_web_renderer::wire::encode_delta_with_resources(&frame, base))
            .transpose()?
            .filter(|bytes| bytes.len() < binary.len())
            .map(|bytes| (revision, Arc::<[u8]>::from(bytes)));
        let resources = bmux_web_renderer::wire::extract_resources(&frame)?
            .into_iter()
            .map(|resource| {
                let (bytes, mime_type) = match resource.payload {
                    bmux_tui::image::ImagePayload::Png { bytes, .. } => (bytes, "image/png"),
                    bmux_tui::image::ImagePayload::Pixels { bytes, .. } => {
                        (bytes, "application/octet-stream")
                    }
                };
                (
                    resource.id,
                    bmux_web_host::Resource {
                        bytes: bytes.into(),
                        mime_type: mime_type.to_owned(),
                    },
                )
            })
            .collect();
        // The diagnostic JSON representation must not veto a deliverable binary view.
        let json = bmux_web_renderer::encode_json(&frame)
            .ok()
            .map(|json| Arc::<[u8]>::from(json.into_bytes()));
        feed_publisher.publish_representations(
            next_revision,
            json,
            bmux_web_host::BinaryPresentation {
                snapshot: binary.into(),
                delta,
            },
            resources,
        )?;
        control.set_generation(&generation(snapshot))?;
        revision = next_revision;
        previous = Some(frame);
        Ok(())
    });
    (publisher, observer)
}

#[derive(Clone, Copy, Default)]
pub(super) struct WebOptions<'a> {
    pub port: Option<u16>,
    pub auth: Option<&'a str>,
    pub host: Option<std::net::IpAddr>,
    pub public_host: Option<std::net::IpAddr>,
}

fn select_ip(
    cli: Option<std::net::IpAddr>,
    env: std::result::Result<String, std::env::VarError>,
    configured: Option<std::net::IpAddr>,
    name: &str,
) -> Result<Option<std::net::IpAddr>> {
    match (cli, env) {
        (Some(ip), _) => Ok(Some(ip)),
        (None, Ok(value)) => {
            Ok(Some(value.parse().with_context(|| {
                format!("{name} must be an IPv4 or IPv6 address")
            })?))
        }
        (None, Err(std::env::VarError::NotPresent)) => Ok(configured),
        (None, Err(error)) => Err(error.into()),
    }
}

fn resolve_web_settings(options: WebOptions<'_>) -> Result<bmux_config::WebConfig> {
    let config = bmux_config::BmuxConfig::load()?.web;
    let mut settings = select_web_settings(
        config,
        (options.port, options.auth),
        std::env::var("BMUX_WEB_PORT"),
        std::env::var("BMUX_WEB_AUTH"),
    )?;
    settings.host = select_ip(
        options.host,
        std::env::var("BMUX_WEB_HOST"),
        Some(config.host),
        "BMUX_WEB_HOST",
    )?
    .context("missing browser bind host")?;
    settings.public_host = select_ip(
        options.public_host,
        std::env::var("BMUX_WEB_PUBLIC_HOST"),
        config.public_host,
        "BMUX_WEB_PUBLIC_HOST",
    )?;
    Ok(settings)
}

fn select_web_settings(
    config: bmux_config::WebConfig,
    options: (Option<u16>, Option<&str>),
    port_env: std::result::Result<String, std::env::VarError>,
    auth_env: std::result::Result<String, std::env::VarError>,
) -> Result<bmux_config::WebConfig> {
    let port = match options.0 {
        Some(port) => port,
        None => match port_env {
            Ok(value) => value
                .parse()
                .context("BMUX_WEB_PORT must be a port from 0 to 65535")?,
            Err(std::env::VarError::NotPresent) => config.port,
            Err(error) => return Err(error.into()),
        },
    };
    let auth_value = match options.1 {
        Some(value) => Some(value.to_owned()),
        None => match auth_env {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(error.into()),
        },
    };
    let auth = match auth_value.as_deref() {
        None => config.auth,
        Some("none") => bmux_config::WebAuth::None,
        Some("token") => bmux_config::WebAuth::Token,
        Some(_) => anyhow::bail!("web authentication must be none or token"),
    };
    Ok(bmux_config::WebConfig {
        port,
        auth,
        ..config
    })
}

fn configure_browser_host(
    assets: std::path::PathBuf,
    web_only: bool,
    options: WebOptions<'_>,
) -> Result<bmux_web_host::HostConfig> {
    let settings = resolve_web_settings(options)?;
    let mut config = bmux_web_host::HostConfig::new(assets);
    config.port = settings.port;
    config.host = settings.host;
    config.public_host = settings.public_host;
    config.auth_mode = match settings.auth {
        bmux_config::WebAuth::None => bmux_web_host::AuthMode::None,
        bmux_config::WebAuth::Token => bmux_web_host::AuthMode::Token,
    };
    config.viewport_mode = if web_only {
        bmux_web_host::ViewportMode::Resizable
    } else {
        bmux_web_host::ViewportMode::Observer
    };
    Ok(config)
}

pub(super) async fn run(
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
    assets: &Path,
    web_only: bool,
    web_options: WebOptions<'_>,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let provider = super::attach::provider::resolve(target)?;
    anyhow::ensure!(
        provider.requires_fallback_client(),
        "browser presentation currently requires a local retained attach"
    );
    let assets = assets.canonicalize().context("browser asset directory")?;
    let limits = bmux_web_host::Limits::default();
    let (feed_publisher, feed) = bmux_web_host::snapshot_feed(limits.max_snapshot_bytes)?;
    let config = configure_browser_host(assets, web_only, web_options)?;
    let mut host = bmux_web_host::start(config, feed).await?;
    let mut requests = host
        .take_viewport_requests()
        .context("browser viewport channel")?;
    let control = host.view_control();
    let resize_control = control.clone();
    let (publisher, mut observer) = start_browser_observer(feed_publisher, control);
    let client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-web-attach",
        connection_context,
    )
    .await?;
    let (driver, viewport) =
        super::attach::headless::headless_attach_channel(super::attach::input::TerminalGeometry {
            cols: 80,
            rows: 24,
        })?;
    eprintln!("Read-only browser view: {}", host.bootstrap_url());
    let attach = async {
        if web_only {
            Box::pin(
                super::attach::runtime::run_session_attach_with_client_headless(
                    client, target, follow, global, None, driver, publisher,
                ),
            )
            .await
        } else {
            Box::pin(
                super::attach::runtime::run_session_attach_with_client_observer(
                    client, target, follow, global, None, publisher,
                ),
            )
            .await
        }
    };
    tokio::pin!(attach);
    // Keep the signal receiver alive across select iterations. Recreating it
    // can discard an interrupt when another ready branch wins the same wake.
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let mut stopping = false;
    let shutdown_deadline = tokio::time::sleep(std::time::Duration::from_secs(2));
    tokio::pin!(shutdown_deadline);
    let outcome: Result<u8> = loop {
        tokio::select! {
            result = &mut attach => break result.map(|outcome| outcome.status_code),
            changed = observer.changed() => {
                if changed.is_err() {
                    host.invalidate_view();
                    break Err(anyhow::anyhow!("browser presentation source closed"));
                }
            }
            request = requests.recv(), if web_only && !stopping => {
                let Some(request) = request else {
                    break Err(anyhow::anyhow!("browser viewport source closed"));
                };
                if observer.latest().is_some_and(|snapshot| generation(&snapshot) == request.generation)
                {
                    let control = resize_control.clone();
                    let request = request.clone();
                    viewport.resize_if(request.cols, request.rows, move || control.is_current(&request))?;
                }
            }
            signal = &mut interrupt, if web_only && !stopping => {
                signal.context("browser attachment shutdown signal")?;
                host.invalidate_view();
                viewport.close();
                shutdown_deadline.as_mut().reset(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                );
                stopping = true;
            }
            () = &mut shutdown_deadline, if stopping => {
                // Detach is best-effort when the server does not acknowledge it.
                // Dropping the attachment releases only this connection, never
                // the server-owned process. The host must still close below.
                break Ok(0);
            }
        }
    };
    host.invalidate_view();
    host.shutdown().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_ip_settings_precedence_and_validation() {
        let absent = || Err(std::env::VarError::NotPresent);
        let local = "127.0.0.1".parse().unwrap();
        let lan = "192.0.2.10".parse().unwrap();
        let wildcard = "0.0.0.0".parse().unwrap();
        assert_eq!(
            select_ip(None, absent(), Some(local), "host").unwrap(),
            Some(local)
        );
        assert_eq!(
            select_ip(None, Ok("192.0.2.10".into()), Some(local), "host").unwrap(),
            Some(lan)
        );
        assert_eq!(
            select_ip(Some(wildcard), Ok("invalid".into()), Some(local), "host").unwrap(),
            Some(wildcard)
        );
        assert_eq!(
            select_ip(None, absent(), None, "public_host").unwrap(),
            None
        );
        assert!(select_ip(None, Ok("hostname.example".into()), None, "host").is_err());
        assert!(select_ip(None, Ok(String::new()), None, "public_host").is_err());
        assert_eq!(
            select_ip(None, Ok("::1".into()), None, "host").unwrap(),
            Some("::1".parse().unwrap())
        );
        let config: bmux_config::WebConfig =
            toml::from_str("host = '0.0.0.0'\npublic_host = '192.0.2.10'\nauth = 'token'").unwrap();
        assert_eq!(config.host, wildcard);
        assert_eq!(config.public_host, Some(lan));
        assert_eq!(config.port, 8080);
        assert_eq!(config.auth, bmux_config::WebAuth::Token);
        assert!(toml::from_str::<bmux_config::WebConfig>("host = 'bad'").is_err());
    }

    #[test]
    fn web_settings_precedence_and_invalid_environment() {
        let absent = || Err(std::env::VarError::NotPresent);
        let defaults = select_web_settings(
            bmux_config::WebConfig::default(),
            (None, None),
            absent(),
            absent(),
        )
        .unwrap();
        assert_eq!(defaults.port, 8080);
        assert_eq!(defaults.auth, bmux_config::WebAuth::None);
        let configured = bmux_config::WebConfig {
            port: 9090,
            auth: bmux_config::WebAuth::Token,
            ..bmux_config::WebConfig::default()
        };
        let env = select_web_settings(
            configured,
            (None, None),
            Ok("8081".into()),
            Ok("none".into()),
        )
        .unwrap();
        assert_eq!(env.port, 8081);
        assert_eq!(env.auth, bmux_config::WebAuth::None);
        let cli = select_web_settings(
            configured,
            (Some(0), Some("token")),
            Ok("bad".into()),
            Ok("bad".into()),
        )
        .unwrap();
        assert_eq!(cli.port, 0);
        assert_eq!(cli.auth, bmux_config::WebAuth::Token);
        assert!(
            select_web_settings(configured, (None, None), Ok("65536".into()), absent()).is_err()
        );
        assert!(select_web_settings(configured, (None, None), absent(), Ok("bad".into())).is_err());
        let inherited = select_web_settings(configured, (None, None), absent(), absent()).unwrap();
        assert_eq!(inherited.port, 9090);
        assert_eq!(inherited.auth, bmux_config::WebAuth::Token);
    }
}
