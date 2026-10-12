//! Execution-owned single ordered capture. V4 raw reads are projections only.
use bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::{
    TerminalCaptureEvent, TerminalCaptureEventKind,
};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
#[derive(Serialize, Deserialize)]
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
        if self.boundary.eof || self.boundary.error.is_some() {
            return Err(std::io::Error::other("capture is closed or incomplete"));
        }
        let frame = serde_json::to_vec(&TerminalCaptureEvent {
            mono_ns: u64::try_from(self.origin.elapsed().as_nanos())
                .map_err(std::io::Error::other)?,
            event,
        })
        .map_err(std::io::Error::other)?;
        let next = self
            .boundary
            .stream_bytes
            .checked_add(u64::try_from(frame.len()).map_err(std::io::Error::other)? + 4)
            .ok_or_else(|| std::io::Error::other("capture length overflow"))?;
        if next > 256 * 1024 * 1024 {
            return Err(std::io::Error::other("ordered capture capacity exhausted"));
        }
        self.file.write_all(
            &u32::try_from(frame.len())
                .map_err(std::io::Error::other)?
                .to_le_bytes(),
        )?;
        self.file.write_all(&frame)?;
        self.file.sync_data()?;
        self.boundary.bytes = self
            .boundary
            .bytes
            .checked_add(raw_bytes)
            .ok_or_else(|| std::io::Error::other("raw length overflow"))?;
        self.boundary.stream_bytes = next;
        self.commit()
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
        self.file.sync_all()?;
        self.boundary.eof = error.is_none();
        self.boundary.error = error;
        self.commit()
    }
    fn open(root: &Path) -> std::io::Result<(Boundary, std::fs::File)> {
        let boundary: Boundary = serde_json::from_slice(&std::fs::read(root.join("capture.json"))?)
            .map_err(std::io::Error::other)?;
        if boundary.version != 1 {
            return Err(std::io::Error::other(
                "unsupported ordered capture schema; raw-only capture requires explicit migration",
            ));
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
include!("durable_capture_tests.rs");
