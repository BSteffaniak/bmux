//! Private negotiated, output-only attachment transport.
//!
//! Parent request (8 bytes): `BMAQ`, minimum version, maximum version, maximum
//! FPS (1..=60), flags (zero). Version 2 response (38 bytes): `BMAO\x02`,
//! epoch UUID, target UUID, accepted FPS. Both handshake and writes have a
//! two-second deadline. Unknown versions/settings are rejected, not guessed.
//!
//! Records are big-endian u32 lengths (`1..=MAX_FRAME_BYTES`) followed by BMWP
//! payloads. First/geometry-repair records are full; other records are deltas
//! against the last successfully written frame. Decode before latest-state
//! coalescing. Inline resources use the codec's immutable baseline reuse.
//! Records signify changes, never keepalives; EOF ends source liveness. Admit
//! epochs explicitly and validate `<epoch>:<target>` and increasing revisions.
//! A partial/rejected record must not replace the reader's last complete frame.
#[cfg(unix)]
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bmux_web_renderer::{BrowserFrame, SurfaceProvenance};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use uuid::Uuid;

use super::{AttachObserverPublisher, AttachSnapshot, attach_observer_channel};

/// Private framing version; the embedded output retains its own wire version.
pub const STREAM_MAGIC: &[u8; 5] = b"BMAO\x02";
const MAX_FPS: u8 = 60;

fn negotiate(request: [u8; 8]) -> Result<u8> {
    anyhow::ensure!(&request[..4] == b"BMAQ", "invalid observer request");
    anyhow::ensure!(
        request[4] <= 2 && request[5] >= 2,
        "unsupported observer version"
    );
    anyhow::ensure!(request[4] <= request[5], "invalid observer version range");
    anyhow::ensure!((1..=MAX_FPS).contains(&request[6]), "invalid observer FPS");
    anyhow::ensure!(request[7] == 0, "unsupported observer flags");
    Ok(request[6])
}
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

// Admission projects a complete immutable state. Serialization belongs to the
// writer, after latest-state coalescing; queued states are never delta bases.
fn encoded_channel(
    target: Uuid,
) -> (
    AttachObserverPublisher,
    Uuid,
    watch::Receiver<Option<Arc<BrowserFrame>>>,
) {
    let (mut publisher, observer) = attach_observer_channel();
    let epoch = observer.epoch();
    let (sender, receiver) = watch::channel(None);
    let mut revision = 0;
    publisher.set_delivery(move |snapshot| {
        anyhow::ensure!(snapshot.epoch == epoch, "observer epoch changed");
        anyhow::ensure!(snapshot.attached_id == target, "observer target changed");
        anyhow::ensure!(snapshot.revision > revision, "stale observer revision");
        let frame = project_frame(snapshot)?;
        sender.send_replace(Some(Arc::new(frame)));
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

fn encode_update(frame: &BrowserFrame, baseline: Option<&BrowserFrame>) -> Result<Vec<u8>> {
    let bytes = match baseline {
        Some(base) if base.buffer().area() == frame.buffer().area() => {
            bmux_web_renderer::encode_delta(frame, base)?
        }
        _ => bmux_web_renderer::encode_binary(frame)?,
    };
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_FRAME_BYTES,
        "observer frame exceeds transport bound"
    );
    Ok(bytes)
}

async fn pump<W: AsyncWrite + Unpin>(
    writer: &mut W,
    receiver: &mut watch::Receiver<Option<Arc<BrowserFrame>>>,
    max_fps: u8,
) -> Result<()> {
    let mut baseline: Option<Arc<BrowserFrame>> = None;
    let interval = Duration::from_nanos(1_000_000_000_u64.div_ceil(u64::from(max_fps)));
    let mut next_write = tokio::time::Instant::now();
    loop {
        receiver.changed().await.context("observer source closed")?;
        // One-shot pacing after change, never an idle timer. Select the latest
        // state after waiting so intermediate frames never serialize.
        if next_write > tokio::time::Instant::now() {
            tokio::time::sleep_until(next_write).await;
        }
        // No borrow survives IO. One in-flight frame, one latest replacement,
        // and one successful baseline bound retention independently of rate.
        let frame = receiver.borrow_and_update().clone();
        if let Some(frame) = frame {
            let bytes = encode_update(&frame, baseline.as_deref())?;
            write_packet(writer, &bytes).await?;
            baseline = Some(frame);
            next_write = tokio::time::Instant::now() + interval;
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
    W: AsyncRead + AsyncWrite + Unpin,
    S: std::future::Future<Output = ()>,
{
    anyhow::ensure!(
        !session_id.is_nil(),
        "observer requires an explicit session UUID"
    );
    let mut request = [0; 8];
    tokio::time::timeout(IO_DEADLINE, writer.read_exact(&mut request))
        .await
        .context("observer request timed out")?
        .context("observer request read")?;
    let max_fps = negotiate(request)?;
    let (mut driver, viewport) = super::super::headless::headless_attach_channel(geometry)?;
    driver.observe_only = true;
    let (mut publisher, epoch, mut receiver) = encoded_channel(session_id);
    publisher.set_max_fps(max_fps);
    tokio::time::timeout(IO_DEADLINE, async {
        writer.write_all(STREAM_MAGIC).await?;
        writer.write_all(epoch.as_bytes()).await?;
        writer.write_all(session_id.as_bytes()).await?;
        writer.write_all(&[max_fps]).await?;
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
    let (mut reader, mut writer) = tokio::io::split(writer);
    let output = pump(&mut writer, &mut receiver, max_fps);
    let disconnected = async {
        let _ = reader.read_u8().await;
    };
    tokio::pin!(attach, output, shutdown, disconnected);
    let deadline = tokio::time::sleep(IO_DEADLINE);
    tokio::pin!(deadline);
    let mut stopping = false;
    loop {
        tokio::select! {
            result = &mut attach => return result.map(|outcome| outcome.status_code),
            () = &mut disconnected, if !stopping => {
                viewport.close();
                stopping = true;
                deadline.as_mut().reset(tokio::time::Instant::now() + IO_DEADLINE);
            }
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
    run_headless_observer(
        client,
        session_id,
        super::super::input::TerminalGeometry { cols, rows },
        stream,
        async {
            let _ = tokio::signal::ctrl_c().await;
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
        assert_eq!(latest.revision(), 3);
    }

    #[test]
    fn negotiation_rejects_unsupported_settings() {
        assert_eq!(negotiate(*b"BMAQ\x02\x02\x3c\x00").unwrap(), 60);
        assert_eq!(negotiate(*b"BMAQ\x01\x03\x01\x00").unwrap(), 1);
        for request in [
            *b"BMAQ\x01\x01\x3c\x00",
            *b"BMAQ\x03\x02\x3c\x00",
            *b"BMAQ\x02\x02\x00\x00",
            *b"BMAQ\x02\x02\x3d\x00",
            *b"BMAQ\x02\x02\x3c\x01",
            *b"XXXX\x02\x02\x3c\x00",
        ] {
            assert!(negotiate(request).is_err());
        }
    }

    #[test]
    fn coalesced_update_uses_written_not_discarded_baseline() {
        let target = Uuid::new_v4();
        let (mut publisher, epoch, receiver) = encoded_channel(target);
        let delivery = publisher.delivery.as_mut().unwrap();
        delivery(&snapshot(epoch, target, 1)).unwrap();
        let first = receiver.borrow().clone().unwrap();
        let full = encode_update(&first, None).unwrap();
        let base = bmux_web_renderer::decode_binary(&full, None).unwrap();
        delivery(&snapshot(epoch, target, 2)).unwrap();
        delivery(&snapshot(epoch, target, 3)).unwrap();
        let latest = receiver.borrow().clone().unwrap();
        let delta = encode_update(&latest, Some(&first)).unwrap();
        assert!(bmux_web_renderer::decode_binary(&delta, None).is_err());
        assert_eq!(
            bmux_web_renderer::decode_binary(&delta, Some(&base)).unwrap(),
            *latest
        );
        assert_eq!(latest.revision(), 3);
    }

    #[tokio::test]
    async fn pump_reconstructs_after_latest_state_replacement() {
        let target = Uuid::new_v4();
        let (mut publisher, epoch, mut receiver) = encoded_channel(target);
        let delivery = publisher.delivery.as_mut().unwrap();
        delivery(&snapshot(epoch, target, 1)).unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(1);
        // A one-byte pipe ensures the first write cannot complete until read.
        let output = pump(&mut writer, &mut receiver, 60);
        tokio::pin!(output);
        tokio::select! {
            biased;
            result = &mut output => panic!("premature output completion: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        delivery(&snapshot(epoch, target, 2)).unwrap();
        delivery(&snapshot(epoch, target, 3)).unwrap();
        let input = async {
            let mut base = None;
            for revision in [1, 3] {
                let length = reader.read_u32().await.unwrap();
                let mut bytes = vec![0; usize::try_from(length).unwrap()];
                reader.read_exact(&mut bytes).await.unwrap();
                let frame = bmux_web_renderer::decode_binary(&bytes, base.as_ref()).unwrap();
                assert_eq!(frame.revision(), revision);
                base = Some(frame);
            }
        };
        tokio::pin!(input);
        tokio::select! {
            result = &mut output => panic!("premature output completion: {result:?}"),
            () = &mut input => {}
            () = tokio::time::sleep(IO_DEADLINE) => panic!("coalesced delivery stalled"),
        }
    }

    #[tokio::test]
    async fn idle_pump_writes_nothing_and_disconnect_is_bounded() {
        let target = Uuid::new_v4();
        let (mut publisher, epoch, mut receiver) = encoded_channel(target);
        let (mut writer, reader) = tokio::io::duplex(64);
        drop(reader);
        let idle = tokio::time::timeout(
            Duration::from_millis(5),
            pump(&mut writer, &mut receiver, 60),
        )
        .await;
        assert!(idle.is_err());
        publisher.delivery.as_mut().unwrap()(&snapshot(epoch, target, 1)).unwrap();
        assert!(pump(&mut writer, &mut receiver, 60).await.is_err());
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
