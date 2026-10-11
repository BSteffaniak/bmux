//! Execution-owned durable raw capture. EOF commits independently of projection.
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
#[derive(Serialize, Deserialize)]
pub(super) struct Boundary {
    pub version: u32,
    pub bytes: u64,
    pub eof: bool,
    pub error: Option<String>,
}
pub(super) struct DurableCapture {
    root: PathBuf,
    file: std::fs::File,
    boundary: Boundary,
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
        let file = options.open(root.join("raw.bin"))?;
        let capture = Self {
            root: root.to_path_buf(),
            file,
            boundary: Boundary {
                version: 4,
                bytes: 0,
                eof: false,
                error: None,
            },
        };
        capture.commit()?;
        Ok(capture)
    }
    fn commit(&self) -> std::io::Result<()> {
        crate::atomic_state::commit(&self.root.join("capture.json"), &self.boundary)
    }
    pub(super) fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        const LIMIT: u64 = 64 * 1024 * 1024;
        let next = self
            .boundary
            .bytes
            .checked_add(u64::try_from(bytes.len()).map_err(std::io::Error::other)?)
            .ok_or_else(|| std::io::Error::other("capture length overflow"))?;
        if next > LIMIT {
            return Err(std::io::Error::other(
                "durable capture capacity exhausted; capture incomplete",
            ));
        }
        self.file.write_all(bytes)?;
        self.file.sync_data()?;
        self.boundary.bytes = next;
        self.commit()
    }
    pub(super) fn end(&mut self, error: Option<String>) -> std::io::Result<()> {
        self.file.sync_all()?;
        self.boundary.eof = error.is_none();
        self.boundary.error = error;
        self.commit()
    }
    pub(super) fn read(root: &Path, offset: u64, max: u32) -> std::io::Result<(Boundary, Vec<u8>)> {
        let boundary: Boundary = serde_json::from_slice(&std::fs::read(root.join("capture.json"))?)
            .map_err(std::io::Error::other)?;
        if boundary.version != 4 {
            return Err(std::io::Error::other(
                "unsupported capture boundary version",
            ));
        }
        let mut file = std::fs::File::open(root.join("raw.bin"))?;
        if file.metadata()?.len() < boundary.bytes {
            return Err(std::io::Error::other(
                "capture shorter than durable boundary",
            ));
        }
        if offset > boundary.bytes || max == 0 || max > 262_144 {
            return Err(std::io::Error::other(
                "invalid capture cursor or read limit",
            ));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![
            0;
            usize::try_from((boundary.bytes - offset).min(u64::from(max)))
                .map_err(std::io::Error::other)?
        ];
        file.read_exact(&mut bytes)?;
        Ok((boundary, bytes))
    }
}

#[cfg(test)]
include!("durable_capture_tests.rs");
