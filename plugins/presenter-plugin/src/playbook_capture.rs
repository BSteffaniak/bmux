//! Parent-owned bounded reader for the confirmed private retained-attach wire.
//! No pane reconstruction, stdout capture, terminal input, or ANSI decoding.
use bmux_tui::embedded_surface::{RetainedSurface, SurfaceIdentity, SurfaceLimits};
use std::sync::Arc;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::watch,
};

const MAGIC: &[u8; 5] = b"BMAO\x01";
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct CaptureSnapshot {
    pub attempt_generation: u64,
    pub epoch: uuid::Uuid,
    pub session: uuid::Uuid,
    pub frame: Arc<bmux_web_renderer::BrowserFrame>,
    pub surface: Arc<RetainedSurface>,
}

/// Reads only an explicitly admitted owned connection. A complete valid header
/// establishes this observer's epoch; subsequent frames cannot change it.
/// EOF/rejection leaves the receiver's last complete accepted snapshot intact.
pub async fn read_capture<R: AsyncRead + Unpin>(
    mut reader: R,
    owner: String,
    attempt_generation: u64,
    expected_session: uuid::Uuid,
    sender: watch::Sender<Option<Arc<CaptureSnapshot>>>,
) -> Result<(), String> {
    let mut header = [0u8; 37];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_exact(&mut header),
    )
    .await
    .map_err(|_| "observer header timeout".to_owned())?
    .map_err(|error| error.to_string())?;
    if &header[..5] != MAGIC {
        return Err("unsupported retained observer protocol".into());
    }
    let epoch = uuid::Uuid::from_slice(&header[5..21]).map_err(|error| error.to_string())?;
    let session = uuid::Uuid::from_slice(&header[21..37]).map_err(|error| error.to_string())?;
    if epoch.is_nil() || session != expected_session || session.is_nil() {
        return Err("observer identity does not match owned attempt".into());
    }
    let generation = format!("{epoch}:{session}");
    let mut revision = 0;
    loop {
        let length = reader.read_u32().await.map_err(|error| error.to_string())? as usize;
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err("retained frame exceeds bounds".into());
        }
        let mut bytes = vec![0; length];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_exact(&mut bytes),
        )
        .await
        .map_err(|_| "partial retained frame timed out".to_owned())?
        .map_err(|error| error.to_string())?;
        let frame =
            bmux_web_renderer::decode_binary(&bytes, None).map_err(|error| format!("{error:?}"))?;
        if frame.generation() != generation || frame.revision() <= revision {
            return Err("stale or foreign retained observer frame".into());
        }
        let surface = RetainedSurface::new(
            SurfaceIdentity {
                owner: owner.clone(),
                generation: attempt_generation,
                revision: frame.revision(),
            },
            Arc::new(frame.buffer().clone()),
            Arc::new(frame.image_scene().clone()),
            SurfaceLimits {
                max_cells: 262_144,
                max_bytes: MAX_FRAME_BYTES,
            },
        )
        .map_err(|error| format!("{error:?}"))?;
        revision = frame.revision();
        // One in-flight bounded repair plus one latest complete snapshot. Readers
        // clone Arcs only; no repeated JSON/grid copies in paint or polling.
        sender.send_replace(Some(Arc::new(CaptureSnapshot {
            attempt_generation,
            epoch,
            session,
            frame: Arc::new(frame),
            surface: Arc::new(surface),
        })));
    }
}
