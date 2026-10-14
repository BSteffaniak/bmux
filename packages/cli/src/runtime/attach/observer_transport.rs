//! Private exact-version, output-only attachment transport.
//!
//! The parent binds a private Unix socket and launches its explicitly selected
//! native executable with isolated `ConfigPaths`. The worker only connects; there
//! is no public listener or input endpoint. Consumers need no CLI dependency:
//! read `BMAO\x01`, epoch UUID bytes, target UUID bytes, then repeated big-endian
//! u32 lengths and self-contained `bmux_web_renderer::decode_binary(bytes, None)`
//! payloads. Reject unknown magic, lengths outside `1..=MAX_FRAME_BYTES`, generation
//! other than `<epoch>:<target>`, and non-increasing revisions. EOF, a partial
//! packet, or rejection preserves the consumer's last accepted complete frame.
//! New worker epochs must be explicitly admitted by the owning attempt, never
//! inferred from an unsolicited frame. No delta base or external images exist.
#[cfg(unix)]
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bmux_web_renderer::{BrowserFrame, SurfaceProvenance};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use uuid::Uuid;

use super::{AttachObserverPublisher, AttachSnapshot, attach_observer_channel};

/// Private framing version; the embedded output retains its own wire version.
pub const STREAM_MAGIC: &[u8; 5] = b"BMAO\x01";
/// Bounded full repair, including inline image resources.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(2);

/// Identity of the existing retained attach output, not a new compositor epoch.
#[must_use]
pub fn generation(snapshot: &AttachSnapshot) -> String {
    format!("{}:{}", snapshot.epoch, snapshot.attached_id)
}

/// Project the exact repaired presentation, including retained surface provenance.
///
/// # Errors
/// Rejects output outside the shared wire representation's admission bounds.
pub fn project_frame(snapshot: &AttachSnapshot) -> Result<BrowserFrame> {
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

// Admission runs synchronously in the existing presentation commit hook. An
// encoding failure must not advance the watch cell or native presentation.
fn encoded_channel(
    target: Uuid,
) -> (
    AttachObserverPublisher,
    Uuid,
    watch::Receiver<Option<Arc<[u8]>>>,
) {
    let (mut publisher, observer) = attach_observer_channel();
    let epoch = observer.epoch();
    let (sender, receiver) = watch::channel(None);
    let mut revision = 0;
    publisher.set_delivery(move |snapshot| {
        anyhow::ensure!(snapshot.epoch == epoch, "observer epoch changed");
        anyhow::ensure!(snapshot.attached_id == target, "observer target changed");
        anyhow::ensure!(snapshot.revision > revision, "stale observer revision");
        let bytes = bmux_web_renderer::encode_binary(&project_frame(snapshot)?)?;
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_FRAME_BYTES,
            "observer frame exceeds transport bound"
        );
        sender.send_replace(Some(Arc::from(bytes)));
        revision = snapshot.revision;
        Ok(())
    });
    (publisher, epoch, receiver)
}

async fn write_packet<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_FRAME_BYTES,
        "observer frame exceeds transport bound"
    );
    let length = u32::try_from(bytes.len()).context("observer frame length")?;
    tokio::time::timeout(IO_DEADLINE, async {
        writer.write_all(&length.to_be_bytes()).await?;
        writer.write_all(bytes).await?;
        writer.flush().await
    })
    .await
    .context("observer consumer stalled")?
    .context("observer frame delivery")
}

async fn pump<W: AsyncWrite + Unpin>(
    writer: &mut W,
    receiver: &mut watch::Receiver<Option<Arc<[u8]>>>,
) -> Result<()> {
    loop {
        receiver.changed().await.context("observer source closed")?;
        // Drop the watch borrow before awaiting output. During slow delivery
        // only the one in-flight repair and the latest replacement are retained.
        let bytes = receiver.borrow_and_update().clone();
        if let Some(bytes) = bytes {
            write_packet(writer, &bytes).await?;
        }
    }
}

/// Drive the real headless retained attach into an explicitly supplied channel.
///
/// The supplied client must already be connected using the caller's explicitly
/// selected isolated `ConfigPaths`; this function never connects to default state.
/// Only the requested session is observed. Shutdown detaches this view, not the
/// server-owned process, and bounds acknowledgement waiting to two seconds.
///
/// # Errors
/// Returns provider, authorization, geometry, presentation, or transport errors.
pub async fn run_headless_observer<W, S>(
    client: bmux_client::BmuxClient,
    session_id: Uuid,
    geometry: super::super::input::TerminalGeometry,
    mut writer: W,
    shutdown: S,
) -> Result<u8>
where
    W: AsyncWrite + Unpin,
    S: std::future::Future<Output = ()>,
{
    anyhow::ensure!(
        !session_id.is_nil(),
        "observer requires an explicit session UUID"
    );
    let (driver, viewport) = super::super::headless::headless_attach_channel(geometry)?;
    let (publisher, epoch, mut receiver) = encoded_channel(session_id);
    tokio::time::timeout(IO_DEADLINE, async {
        writer.write_all(STREAM_MAGIC).await?;
        writer.write_all(epoch.as_bytes()).await?;
        writer.write_all(session_id.as_bytes()).await?;
        writer.flush().await
    })
    .await
    .context("observer consumer stalled during handshake")?
    .context("observer handshake delivery")?;

    let target = session_id.to_string();
    let attach = super::super::runtime::run_session_attach_with_client_headless(
        client,
        Some(&target),
        None,
        false,
        None,
        driver,
        publisher,
    );
    let output = pump(&mut writer, &mut receiver);
    tokio::pin!(attach, output, shutdown);
    let deadline = tokio::time::sleep(IO_DEADLINE);
    tokio::pin!(deadline);
    let mut stopping = false;
    loop {
        tokio::select! {
            result = &mut attach => return result.map(|outcome| outcome.status_code),
            result = &mut output => return result.map(|()| 0),
            () = &mut shutdown, if !stopping => {
                viewport.close();
                stopping = true;
                deadline.as_mut().reset(tokio::time::Instant::now() + IO_DEADLINE);
            }
            () = &mut deadline, if stopping => return Ok(0),
        }
    }
}

/// CLI dispatch entry for a private parent-owned channel, with no HTTP listener.
///
/// # Errors
/// Rejects non-absolute socket paths and failed bounded connect/attach/delivery.
#[cfg(unix)]
pub async fn run_private_worker(
    client: bmux_client::BmuxClient,
    session_id: Uuid,
    socket_path: &Path,
    cols: u16,
    rows: u16,
) -> Result<u8> {
    anyhow::ensure!(
        socket_path.is_absolute(),
        "observer socket path must be absolute"
    );
    let stream = tokio::time::timeout(IO_DEADLINE, tokio::net::UnixStream::connect(socket_path))
        .await
        .context("observer socket connection timed out")?
        .context("connect private observer socket")?;
    let (mut reader, writer) = stream.into_split();
    run_headless_observer(
        client,
        session_id,
        super::super::input::TerminalGeometry { cols, rows },
        writer,
        async {
            use tokio::io::AsyncReadExt;
            // This channel is output-only: EOF, unexpected parent input, or
            // failed signal registration all close just this owned view.
            tokio::select! {
                _ = reader.read_u8() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::{geometry::Rect, presentation::PresentationState};
    use tokio::io::AsyncReadExt;

    fn snapshot(epoch: Uuid, target: Uuid, revision: u64) -> AttachSnapshot {
        let state = PresentationState::new(Rect::new(0, 0, 2, 1));
        let prepared = state
            .prepare(bmux_tui::damage::Damage::Full, |_| {})
            .unwrap();
        AttachSnapshot {
            epoch,
            revision,
            attached_id: target,
            prepared,
            surfaces: Vec::new(),
            selection_text: std::collections::BTreeMap::new(),
            unsupported_metadata: &["selection", "semantics"],
        }
    }

    #[tokio::test]
    async fn packet_is_bounded_self_contained_native_output() {
        let epoch = Uuid::new_v4();
        let target = Uuid::new_v4();
        let frame = project_frame(&snapshot(epoch, target, 1)).unwrap();
        let bytes = bmux_web_renderer::encode_binary(&frame).unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        write_packet(&mut writer, &bytes).await.unwrap();
        let length = reader.read_u32().await.unwrap();
        let mut received = vec![0; usize::try_from(length).unwrap()];
        reader.read_exact(&mut received).await.unwrap();
        let decoded = bmux_web_renderer::decode_binary(&received, None).unwrap();
        assert_eq!(decoded, frame);
        assert_eq!(decoded.generation(), format!("{epoch}:{target}"));
        assert_eq!(decoded.buffer().area(), Rect::new(0, 0, 2, 1));
    }

    #[test]
    fn failed_admission_preserves_latest_repair() {
        let target = Uuid::new_v4();
        let (mut publisher, epoch, receiver) = encoded_channel(target);
        let delivery = publisher.delivery.as_mut().unwrap();
        delivery(&snapshot(epoch, target, 1)).unwrap();
        let committed = receiver.borrow().clone().unwrap();
        assert!(delivery(&snapshot(epoch, Uuid::new_v4(), 2)).is_err());
        assert!(delivery(&snapshot(Uuid::new_v4(), target, 2)).is_err());
        assert!(delivery(&snapshot(epoch, target, 1)).is_err());
        assert!(Arc::ptr_eq(&committed, receiver.borrow().as_ref().unwrap()));
        delivery(&snapshot(epoch, target, 3)).unwrap();
        let latest = receiver.borrow().clone().unwrap();
        let decoded = bmux_web_renderer::decode_binary(&latest, None).unwrap();
        assert_eq!(decoded.revision(), 3);
    }

    #[tokio::test]
    async fn invalid_packet_writes_no_prefix() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        assert!(write_packet(&mut writer, &[]).await.is_err());
        drop(writer);
        assert_eq!(
            reader.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }
}
