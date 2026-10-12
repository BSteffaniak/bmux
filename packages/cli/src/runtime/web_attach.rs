//! Local read-only browser observation of an explicitly selected retained attach.
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use bmux_web_renderer::{BrowserFrame, SurfaceProvenance};

use super::attach::observer::{AttachSnapshot, attach_observer_channel};
use super::{ConnectionContext, ConnectionPolicyScope, connect_with_context};

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
        })
        .collect();
    let generation = format!("{}:{}", snapshot.epoch, snapshot.attached_id);
    let frame = BrowserFrame::from_prepared_in_generation(
        &generation,
        snapshot.revision,
        &snapshot.prepared,
    )?
    .with_surfaces(surfaces)?;
    Ok(bmux_web_renderer::encode_json(&frame)?.into_bytes().into())
}

pub(super) async fn run(
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
    assets: &Path,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let provider = super::attach::provider::resolve(target)?;
    anyhow::ensure!(
        provider.requires_fallback_client(),
        "web observation currently requires a local retained attach"
    );
    let assets = std::fs::canonicalize(assets).context("resolve browser asset directory")?;
    let limits = bmux_web_host::Limits::default();
    let (feed_publisher, feed) = bmux_web_host::snapshot_feed(limits.max_snapshot_bytes)?;
    let host = bmux_web_host::start(bmux_web_host::HostConfig::new(assets), feed).await?;
    // This is an explicit local capability disclosure requested by --web-assets.
    // Never send this URL/token to tracing, recordings or durable configuration.
    eprintln!("Read-only browser view: {}", host.bootstrap_url());
    let (publisher, mut observer) = attach_observer_channel();
    let epoch = observer.epoch();
    let client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-web-observer",
        connection_context,
    )
    .await?;
    let attach = super::attach::runtime::run_session_attach_with_client_observer(
        client, target, follow, global, None, publisher,
    );
    tokio::pin!(attach);
    let mut feed_publisher = Some(feed_publisher);
    let mut revision = 0_u64;
    let mut observing = true;
    let outcome = loop {
        tokio::select! {
            result = &mut attach => break result,
            changed = observer.changed(), if observing => {
                let Ok(snapshot) = changed else {
                    observing = false;
                    // Close the feed immediately; never retain a healthy-looking stale view.
                    feed_publisher.take();
                    continue;
                };
                let snapshot = observer.latest().unwrap_or(snapshot);
                {
                    anyhow::ensure!(snapshot.epoch == epoch, "browser observer generation changed unexpectedly");
                    match encode(&snapshot) {
                        Ok(bytes) => {
                            revision = revision.checked_add(1).context("browser transport revision exhausted")?;
                            if let Some(publisher) = &feed_publisher {
                                publisher.publish(revision, bytes)?;
                            }
                        }
                        Err(error) => {
                            // Never publish a partial or apparently healthy truncated frame.
                            host.shutdown().await;
                            return Err(error.context("browser presentation rejected"));
                        }
                    }
                }
            }
        }
    };
    drop(feed_publisher);
    host.shutdown().await;
    outcome.map(|result| result.status_code)
}
