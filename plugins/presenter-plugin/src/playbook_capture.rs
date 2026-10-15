//! Parent-owned bounded reader for the confirmed private retained-attach wire.
//! No pane reconstruction, stdout capture, terminal input, or ANSI decoding.
use bmux_tui::embedded_surface::{RetainedSurface, SurfaceIdentity, SurfaceLimits};
use std::sync::Arc;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::watch,
};

#[derive(Debug)]
pub enum CaptureError {
    Closed,
    Truncated(&'static str),
    Failure(String),
}
impl From<String> for CaptureError {
    fn from(message: String) -> Self {
        Self::Failure(message)
    }
}
impl From<&str> for CaptureError {
    fn from(message: &str) -> Self {
        Self::Failure(message.into())
    }
}
impl std::fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => formatter.write_str("observer connection closed between frames"),
            Self::Truncated(stage) => write!(formatter, "observer connection truncated {stage}"),
            Self::Failure(message) => formatter.write_str(message),
        }
    }
}

async fn read_part<R: AsyncRead + Unpin>(
    reader: &mut R,
    bytes: &mut [u8],
    stage: &'static str,
) -> Result<(), CaptureError> {
    tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_exact(bytes))
        .await
        .map_err(|_| CaptureError::Failure(format!("observer {stage} timed out")))?
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                CaptureError::Truncated(stage)
            } else {
                CaptureError::Failure(format!("observer {stage}: {error}"))
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn closed_stream(tail: &[u8]) -> CaptureError {
        let session = uuid::Uuid::new_v4();
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes.extend_from_slice(session.as_bytes());
        bytes.extend_from_slice(tail);
        let (sender, _) = watch::channel(None);
        read_capture(bytes.as_slice(), "owner".into(), 1, session, sender)
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn eof_distinguishes_boundaries_and_truncation() {
        assert!(matches!(closed_stream(&[]).await, CaptureError::Closed));
        assert!(matches!(
            closed_stream(&[0]).await,
            CaptureError::Truncated("frame length")
        ));
        assert!(matches!(
            closed_stream(&[0, 0, 0, 2, 1]).await,
            CaptureError::Truncated("frame payload")
        ));
        let (sender, _) = watch::channel(None);
        assert!(matches!(
            read_capture(&b"BMA"[..], "owner".into(), 1, uuid::Uuid::new_v4(), sender).await,
            Err(CaptureError::Truncated("header"))
        ));
    }

    #[tokio::test]
    async fn truncated_update_retains_last_complete_frame() {
        let session = uuid::Uuid::new_v4();
        let epoch = uuid::Uuid::new_v4();
        let prepared = bmux_tui::presentation::PresentationState::new(
            bmux_tui::geometry::Rect::new(0, 0, 2, 1),
        )
        .prepare(bmux_tui::damage::Damage::Full, |_| {})
        .unwrap();
        let frame = bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
            &format!("{epoch}:{session}"),
            1,
            &prepared,
        )
        .unwrap();
        let encoded = bmux_web_renderer::encode_binary(&frame).unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(epoch.as_bytes());
        bytes.extend_from_slice(session.as_bytes());
        bytes.extend_from_slice(&u32::try_from(encoded.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(&encoded);
        bytes.extend_from_slice(&[0, 0, 0, 2, 1]);
        let (sender, receiver) = watch::channel(None);
        assert!(matches!(
            read_capture(bytes.as_slice(), "owner".into(), 9, session, sender).await,
            Err(CaptureError::Truncated("frame payload"))
        ));
        let retained = receiver.borrow().clone().unwrap();
        assert_eq!(retained.attempt_generation, 9);
        assert_eq!(retained.frame.revision(), 1);
        assert_eq!(retained.session, session);
    }

    #[tokio::test(start_paused = true)]
    async fn partial_length_has_a_deadline() {
        let session = uuid::Uuid::new_v4();
        let (mut writer, reader) = tokio::io::duplex(128);
        writer.write_all(MAGIC).await.unwrap();
        writer
            .write_all(uuid::Uuid::new_v4().as_bytes())
            .await
            .unwrap();
        writer.write_all(session.as_bytes()).await.unwrap();
        writer.write_all(&[0]).await.unwrap();
        let (sender, _) = watch::channel(None);
        let task = tokio::spawn(read_capture(reader, "owner".into(), 1, session, sender));
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        assert!(
            matches!(task.await.unwrap(), Err(CaptureError::Failure(message)) if message.contains("frame length timed out"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn paused_connection_has_no_idle_deadline() {
        let session = uuid::Uuid::new_v4();
        let (mut writer, reader) = tokio::io::duplex(128);
        writer.write_all(MAGIC).await.unwrap();
        writer
            .write_all(uuid::Uuid::new_v4().as_bytes())
            .await
            .unwrap();
        writer.write_all(session.as_bytes()).await.unwrap();
        let (sender, _) = watch::channel(None);
        let task = tokio::spawn(read_capture(reader, "owner".into(), 1, session, sender));
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        drop(writer);
        assert!(matches!(task.await.unwrap(), Err(CaptureError::Closed)));
    }
}

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
) -> Result<(), CaptureError> {
    let mut header = [0u8; 37];
    read_part(&mut reader, &mut header, "header").await?;
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
        // Waiting for the first byte has no deadline: paused views may be idle.
        let mut prefix = [0u8; 4];
        if reader
            .read(&mut prefix[..1])
            .await
            .map_err(|error| error.to_string())?
            == 0
        {
            return Err(CaptureError::Closed);
        }
        read_part(&mut reader, &mut prefix[1..], "frame length").await?;
        let length = u32::from_be_bytes(prefix) as usize;
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err("retained frame exceeds bounds".into());
        }
        let mut bytes = vec![0; length];
        read_part(&mut reader, &mut bytes, "frame payload").await?;
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
