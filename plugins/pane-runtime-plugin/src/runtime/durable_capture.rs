//! Execution-owned single ordered capture. V4 raw reads are projections only.
use bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::{
    TerminalCaptureEvent, TerminalCaptureEventKind,
};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Boundary {
    pub version: u32,
    pub bytes: u64,
    pub stream_bytes: u64,
    pub eof: bool,
    pub error: Option<String>,
}
pub(super) struct DurableCapture {
    root: PathBuf,
    file: std::fs::File,
    boundary: Boundary,
    origin: std::time::Instant,
    failed: bool,
}
impl DurableCapture {
    pub(super) fn create(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(root.join("events.bin"))?;
        let capture = Self {
            root: root.to_path_buf(),
            file,
            boundary: Boundary {
                version: 1,
                bytes: 0,
                stream_bytes: 0,
                eof: false,
                error: None,
            },
            origin: std::time::Instant::now(),
            failed: false,
        };
        capture.commit()?;
        Ok(capture)
    }
    fn commit(&self) -> std::io::Result<()> {
        crate::atomic_state::commit(&self.root.join("capture.json"), &self.boundary)
    }
    pub(super) fn raw_bytes(&self) -> u64 {
        self.boundary.bytes
    }
    fn append_event(
        &mut self,
        event: TerminalCaptureEventKind,
        raw_bytes: u64,
    ) -> std::io::Result<()> {
        let mono_ns =
            u64::try_from(self.origin.elapsed().as_nanos()).map_err(std::io::Error::other)?;
        self.append_admitted(event, raw_bytes, mono_ns)
    }
    // Called by the ordered writer with the timestamp assigned at admission.
    pub(super) fn append_admitted(
        &mut self,
        event: TerminalCaptureEventKind,
        raw_bytes: u64,
        mono_ns: u64,
    ) -> std::io::Result<()> {
        if self.failed || self.boundary.eof || self.boundary.error.is_some() {
            return Err(std::io::Error::other("capture is closed or incomplete"));
        }
        let valid = match &event {
            TerminalCaptureEventKind::Output { data } => {
                data.len() <= 8192 && u64::try_from(data.len()).ok() == Some(raw_bytes)
            }
            TerminalCaptureEventKind::Resize { cols, rows } => {
                raw_bytes == 0 && *cols > 0 && *rows > 0 && *cols <= 1000 && *rows <= 1000
            }
        };
        if !valid {
            self.failed = true;
            return Err(std::io::Error::other("invalid admitted capture event"));
        }
        let mut candidate = self.boundary.clone();
        candidate.bytes = candidate
            .bytes
            .checked_add(raw_bytes)
            .ok_or_else(|| std::io::Error::other("raw length overflow"))?;
        if candidate.bytes > 64 * 1024 * 1024 {
            self.failed = true;
            return Err(std::io::Error::other("durable capture capacity exhausted"));
        }
        let frame = serde_json::to_vec(&TerminalCaptureEvent { mono_ns, event })
            .map_err(std::io::Error::other)?;
        let next = self
            .boundary
            .stream_bytes
            .checked_add(u64::try_from(frame.len()).map_err(std::io::Error::other)? + 4)
            .ok_or_else(|| std::io::Error::other("capture length overflow"))?;
        if next > 256 * 1024 * 1024 {
            self.failed = true;
            return Err(std::io::Error::other("ordered capture capacity exhausted"));
        }
        candidate.stream_bytes = next;
        // A failed append may leave a tail, but must never advance acknowledged
        // progress or permit later appends behind that uncertain tail.
        let result = (|| {
            self.file.write_all(
                &u32::try_from(frame.len())
                    .map_err(std::io::Error::other)?
                    .to_le_bytes(),
            )?;
            self.file.write_all(&frame)?;
            self.file.sync_data()?;
            crate::atomic_state::commit(&self.root.join("capture.json"), &candidate)
        })();
        match result {
            Ok(()) => {
                self.boundary = candidate;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
    pub(super) fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // Keep the framed representation bounded even for test/private callers.
        if bytes.len() > 8192 {
            for chunk in bytes.chunks(8192) {
                self.append(chunk)?;
            }
            return Ok(());
        }
        let count = u64::try_from(bytes.len()).map_err(std::io::Error::other)?;
        if self.boundary.bytes.saturating_add(count) > 64 * 1024 * 1024 {
            self.failed = true;
            return Err(std::io::Error::other(
                "durable capture capacity exhausted; capture incomplete",
            ));
        }
        self.append_event(
            TerminalCaptureEventKind::Output {
                data: bytes.to_vec(),
            },
            count,
        )
    }
    pub(super) fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        self.append_event(TerminalCaptureEventKind::Resize { cols, rows }, 0)
    }
    pub(super) fn end(&mut self, error: Option<String>) -> std::io::Result<()> {
        if self.failed || self.boundary.eof || self.boundary.error.is_some() {
            return Err(std::io::Error::other("capture is closed or incomplete"));
        }
        let mut candidate = self.boundary.clone();
        candidate.eof = error.is_none();
        candidate.error = error;
        let result = self.file.sync_all().and_then(|()| {
            crate::atomic_state::commit(&self.root.join("capture.json"), &candidate)
        });
        match result {
            Ok(()) => {
                self.boundary = candidate;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
    pub(super) fn committed_boundary(&self) -> Boundary {
        self.boundary.clone()
    }
    /// Recovery admission validates the entire committed prefix once, before
    /// exposing an abandoned capture. Uncommitted tails are deliberately ignored.
    /// Not a per-keystroke read path and never reopens the spool for append.
    pub(super) fn validate_recovery(root: &Path) -> std::io::Result<Boundary> {
        let (boundary, mut file) = Self::open(root)?;
        let mut position = 0_u64;
        let mut raw = 0_u64;
        let mut previous_time = 0;
        while position < boundary.stream_bytes {
            if boundary.stream_bytes - position < 4 {
                return Err(std::io::Error::other("partial committed capture length"));
            }
            let mut length = [0; 4];
            file.read_exact(&mut length)?;
            let length = u32::from_le_bytes(length);
            position += 4 + u64::from(length);
            if length == 0 || length > 65536 || position > boundary.stream_bytes {
                return Err(std::io::Error::other("invalid committed capture frame"));
            }
            let mut bytes = vec![0; usize::try_from(length).map_err(std::io::Error::other)?];
            file.read_exact(&mut bytes)?;
            let event: TerminalCaptureEvent =
                serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
            if event.mono_ns < previous_time {
                return Err(std::io::Error::other("capture timestamps out of order"));
            }
            previous_time = event.mono_ns;
            match event.event {
                TerminalCaptureEventKind::Output { data } => {
                    if data.len() > 8192 {
                        return Err(std::io::Error::other("oversized committed output"));
                    }
                    raw = raw
                        .checked_add(u64::try_from(data.len()).map_err(std::io::Error::other)?)
                        .ok_or_else(|| std::io::Error::other("capture raw length overflow"))?;
                }
                TerminalCaptureEventKind::Resize { cols, rows } => {
                    if cols == 0 || rows == 0 || cols > 1000 || rows > 1000 {
                        return Err(std::io::Error::other("invalid committed geometry"));
                    }
                }
            }
        }
        if raw != boundary.bytes {
            return Err(std::io::Error::other("capture raw boundary mismatch"));
        }
        Ok(boundary)
    }
    fn open(root: &Path) -> std::io::Result<(Boundary, std::fs::File)> {
        let boundary: Boundary = serde_json::from_slice(&std::fs::read(root.join("capture.json"))?)
            .map_err(std::io::Error::other)?;
        if boundary.version != 1 {
            return Err(std::io::Error::other(
                "unsupported ordered capture schema; raw-only capture requires explicit migration",
            ));
        }
        if boundary.bytes > 64 * 1024 * 1024
            || boundary.stream_bytes > 256 * 1024 * 1024
            || (boundary.eof && boundary.error.is_some())
        {
            return Err(std::io::Error::other("invalid ordered capture boundary"));
        }
        let file = std::fs::File::open(root.join("events.bin"))?;
        if file.metadata()?.len() < boundary.stream_bytes {
            return Err(std::io::Error::other(
                "capture shorter than durable boundary",
            ));
        }
        Ok((boundary, file))
    }
    pub(super) fn read_events(
        root: &Path,
        offset: u64,
        max: u32,
    ) -> std::io::Result<(Boundary, Vec<u8>)> {
        let (boundary, mut file) = Self::open(root)?;
        if offset > boundary.stream_bytes || max == 0 || max > 262_144 {
            return Err(std::io::Error::other(
                "invalid ordered capture cursor or read limit",
            ));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![
            0;
            usize::try_from((boundary.stream_bytes - offset).min(u64::from(max)))
                .map_err(std::io::Error::other)?
        ];
        file.read_exact(&mut bytes)?;
        Ok((boundary, bytes))
    }
    pub(super) fn read(root: &Path, offset: u64, max: u32) -> std::io::Result<(Boundary, Vec<u8>)> {
        let (boundary, mut file) = Self::open(root)?;
        if offset > boundary.bytes || max == 0 || max > 262_144 {
            return Err(std::io::Error::other(
                "invalid capture cursor or read limit",
            ));
        }
        let mut position = 0_u64;
        let mut raw = 0_u64;
        let mut output = Vec::new();
        while position < boundary.stream_bytes
            && output.len() < usize::try_from(max).map_err(std::io::Error::other)?
        {
            let mut length = [0; 4];
            file.read_exact(&mut length)?;
            let length = u32::from_le_bytes(length);
            position += 4 + u64::from(length);
            if length > 65536 || position > boundary.stream_bytes {
                return Err(std::io::Error::other("invalid ordered capture frame"));
            }
            let mut bytes = vec![0; usize::try_from(length).map_err(std::io::Error::other)?];
            file.read_exact(&mut bytes)?;
            let event: TerminalCaptureEvent =
                serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
            if let TerminalCaptureEventKind::Output { data } = event.event {
                let end = raw + u64::try_from(data.len()).map_err(std::io::Error::other)?;
                if end > offset {
                    let start = usize::try_from(offset.saturating_sub(raw))
                        .map_err(std::io::Error::other)?;
                    let count = (data.len() - start)
                        .min(usize::try_from(max).map_err(std::io::Error::other)? - output.len());
                    output.extend_from_slice(&data[start..start + count]);
                }
                raw = end;
            }
        }
        Ok((boundary, output))
    }
}

#[cfg(test)]
mod server_loss_tests {
    use super::*;

    #[test]
    fn recovery_accepts_committed_v1_and_ignores_complete_uncommitted_frames() {
        let root = tempfile::tempdir().unwrap();
        let mut capture = DurableCapture::create(root.path()).unwrap();
        capture.append(b"committed").unwrap();
        let saved = std::fs::read(root.path().join("capture.json")).unwrap();
        capture.append(b"unacknowledged tail").unwrap();
        drop(capture);
        std::fs::write(root.path().join("capture.json"), saved).unwrap();
        let boundary = DurableCapture::validate_recovery(root.path()).unwrap();
        assert_eq!(boundary.bytes, 9);
        assert!(!boundary.eof);
        assert_eq!(
            DurableCapture::read(root.path(), 0, 64).unwrap().1,
            b"committed"
        );
    }

    #[test]
    fn recovery_rejects_corrupt_committed_frames_and_unknown_versions() {
        let root = tempfile::tempdir().unwrap();
        let mut capture = DurableCapture::create(root.path()).unwrap();
        capture.append(b"committed").unwrap();
        capture.file.seek(SeekFrom::Start(0)).unwrap();
        capture.file.write_all(&u32::MAX.to_le_bytes()).unwrap();
        assert!(DurableCapture::validate_recovery(root.path()).is_err());
        let mut boundary = capture.committed_boundary();
        boundary.version = 99;
        crate::atomic_state::commit(&root.path().join("capture.json"), &boundary).unwrap();
        assert!(DurableCapture::validate_recovery(root.path()).is_err());
    }

    #[test]
    fn failed_boundary_commit_never_advances_memory_or_allows_clean_end() {
        let root = tempfile::tempdir().unwrap();
        let mut capture = DurableCapture::create(root.path()).unwrap();
        capture.append(b"safe").unwrap();
        let boundary = root.path().join("capture.json");
        std::fs::remove_file(&boundary).unwrap();
        std::fs::create_dir(&boundary).unwrap();
        assert!(capture.append(b"uncertain").is_err());
        assert_eq!(capture.raw_bytes(), 4);
        assert!(capture.end(None).is_err());
        assert!(capture.append(b"retry").is_err());
    }

    #[test]
    fn abandoned_writer_preserves_committed_output_without_claiming_eof() {
        let root = tempfile::tempdir().unwrap();
        let mut capture = DurableCapture::create(root.path()).unwrap();
        capture.append(b"before server loss").unwrap();
        drop(capture); // No end acknowledgment, as with an interrupted owner.

        // A crash can leave an uncommitted partial frame after the boundary.
        std::fs::OpenOptions::new()
            .append(true)
            .open(root.path().join("events.bin"))
            .unwrap()
            .write_all(&[0xff, 0xff])
            .unwrap();
        let (boundary, bytes) = DurableCapture::read(root.path(), 0, 1024).unwrap();
        assert_eq!(bytes, b"before server loss");
        assert_eq!(boundary.bytes, u64::try_from(bytes.len()).unwrap());
        assert!(!boundary.eof, "owner loss is not a committed capture end");
        assert!(boundary.error.is_none());
        assert!(DurableCapture::create(root.path()).is_err());
        let (retained, bytes_again) = DurableCapture::read(root.path(), 0, 1024).unwrap();
        assert_eq!(bytes_again, bytes);
        assert!(!retained.eof);
    }
}

#[cfg(test)]
include!("durable_capture_tests.rs");
