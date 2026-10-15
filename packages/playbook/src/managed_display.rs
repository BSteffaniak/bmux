//! Private latest-complete output from an owned managed runner, not an input API.
//! This is deliberately distinct from BMAO1's fixed-target observer protocol.
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use uuid::Uuid;

pub const FILE_NAME: &str = "managed-display-v1";
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAGIC: &[u8; 5] = b"BMSD\x01";
const HEADER_BYTES: usize = 49;

pub struct DisplayPacket {
    pub generation: u64,
    pub epoch: Uuid,
    pub session: Uuid,
    pub frame: Vec<u8>,
}

impl DisplayPacket {
    fn header(&self) -> Result<Vec<u8>> {
        ensure!(self.generation != 0, "zero managed display generation");
        ensure!(
            !self.epoch.is_nil() && !self.session.is_nil(),
            "nil managed display identity"
        );
        ensure!(
            !self.frame.is_empty() && self.frame.len() <= MAX_FRAME_BYTES,
            "managed display exceeds bounds"
        );
        let mut header = Vec::with_capacity(HEADER_BYTES);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&self.generation.to_be_bytes());
        header.extend_from_slice(self.epoch.as_bytes());
        header.extend_from_slice(self.session.as_bytes());
        header.extend_from_slice(&u32::try_from(self.frame.len())?.to_be_bytes());
        Ok(header)
    }

    /// Atomically replace one repair in the service-owned private attempt root.
    pub fn publish(&self, root: &Path) -> Result<()> {
        let header = self.header()?;
        ensure!(root.is_absolute(), "managed display root must be absolute");
        let pending = root.join(format!(".managed-display-{}", Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let result = (|| {
            let mut file = options.open(&pending)?;
            file.write_all(&header)?;
            file.write_all(&self.frame)?;
            std::fs::rename(&pending, root.join(FILE_NAME))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(pending);
        }
        result
    }

    /// Read one bounded no-follow complete repair. Missing means not ready yet.
    pub fn read(root: &Path, generation: u64) -> Result<Option<Self>> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = match options.open(root.join(FILE_NAME)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "managed display is not a regular file");
        ensure!(
            metadata.len() <= u64::try_from(HEADER_BYTES + MAX_FRAME_BYTES)?,
            "oversized managed display"
        );
        let mut header = [0; HEADER_BYTES];
        file.read_exact(&mut header)
            .context("partial managed display header")?;
        ensure!(&header[..5] == MAGIC, "unsupported managed display version");
        let packet_generation = u64::from_be_bytes(header[5..13].try_into()?);
        ensure!(
            packet_generation == generation && generation != 0,
            "stale managed display generation"
        );
        let epoch = Uuid::from_slice(&header[13..29])?;
        let session = Uuid::from_slice(&header[29..45])?;
        let length = usize::try_from(u32::from_be_bytes(header[45..49].try_into()?))?;
        ensure!(
            length > 0 && length <= MAX_FRAME_BYTES,
            "managed display exceeds bounds"
        );
        ensure!(
            metadata.len() == u64::try_from(HEADER_BYTES + length)?,
            "partial managed display frame"
        );
        ensure!(
            !epoch.is_nil() && !session.is_nil(),
            "nil managed display identity"
        );
        let mut frame = vec![0; length];
        file.read_exact(&mut frame)?;
        Ok(Some(Self {
            generation,
            epoch,
            session,
            frame,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_is_generation_fenced_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        assert!(DisplayPacket::read(root.path(), 7).unwrap().is_none());
        let mut packet = DisplayPacket {
            generation: 7,
            epoch: Uuid::new_v4(),
            session: Uuid::new_v4(),
            frame: vec![1, 2, 3],
        };
        packet.publish(root.path()).unwrap();
        assert_eq!(
            DisplayPacket::read(root.path(), 7).unwrap().unwrap().frame,
            packet.frame
        );
        assert!(DisplayPacket::read(root.path(), 8).is_err());
        packet.session = Uuid::new_v4();
        packet.frame = vec![4];
        packet.publish(root.path()).unwrap();
        assert_eq!(
            DisplayPacket::read(root.path(), 7)
                .unwrap()
                .unwrap()
                .session,
            packet.session
        );
        packet.frame.clear();
        assert!(packet.publish(root.path()).is_err());
        std::fs::write(root.path().join(FILE_NAME), MAGIC).unwrap();
        assert!(DisplayPacket::read(root.path(), 7).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_display_sources() {
        let root = tempfile::tempdir().unwrap();
        let other = root.path().join("other");
        std::fs::write(&other, b"not a frame").unwrap();
        std::os::unix::fs::symlink(other, root.path().join(FILE_NAME)).unwrap();
        assert!(DisplayPacket::read(root.path(), 1).is_err());
    }
}
