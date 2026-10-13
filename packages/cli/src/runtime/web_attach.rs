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

pub(super) async fn run(
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
    assets: &Path,
    web_only: bool,
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
    let mut config = bmux_web_host::HostConfig::new(assets);
    config.viewport_mode = if web_only {
        bmux_web_host::ViewportMode::Resizable
    } else {
        bmux_web_host::ViewportMode::Observer
    };
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
