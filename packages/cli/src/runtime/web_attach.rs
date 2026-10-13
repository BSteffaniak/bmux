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

fn encode(snapshot: &AttachSnapshot) -> Result<Arc<[u8]>> {
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
    Ok(bmux_web_renderer::encode_json(&frame)?.into_bytes().into())
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
    let (mut publisher, mut observer) = attach_observer_channel();
    let epoch = observer.epoch();
    let mut revision = 0_u64;
    publisher.set_delivery(move |snapshot| {
        anyhow::ensure!(
            snapshot.epoch == epoch,
            "browser observer epoch changed unexpectedly"
        );
        let bytes = encode(snapshot).context("browser presentation rejected")?;
        revision = revision
            .checked_add(1)
            .context("browser transport revision exhausted")?;
        feed_publisher.publish(revision, bytes)?;
        control.set_generation(&generation(snapshot))?;
        Ok(())
    });
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
    let mut stopping = false;
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
            signal = tokio::signal::ctrl_c(), if web_only && !stopping => {
                signal.context("browser attachment shutdown signal")?;
                host.invalidate_view();
                viewport.close();
                stopping = true;
            }
        }
    };
    host.invalidate_view();
    host.shutdown().await;
    outcome
}
