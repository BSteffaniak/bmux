//! Owner-only private records under `state_dir/connections`.
//!
//! Every directory is opened without following links and must be owned by the
//! current user with no group/other access. Records are replaced atomically.

use anyhow::{Context, Result, bail};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
pub use unix::PrivateDirectory;
#[cfg(windows)]
pub use windows::PrivateDirectory;

/// Largest private record accepted.
pub const RECORD_LIMIT: u64 = 1_048_576;

/// `state_dir/connections`, created owner-only.
pub fn connections_dir(state_dir: &Path) -> Result<PathBuf> {
    if !state_dir.is_absolute() {
        bail!("connections storage requires an absolute state directory");
    }
    std::fs::create_dir_all(state_dir).context("create state directory")?;
    let root = state_dir
        .canonicalize()
        .context("resolve state directory")?
        .join("connections");
    PrivateDirectory::open(&root)?;
    Ok(root)
}

/// Open (creating) the private subdirectory `name` of `state_dir/connections`.
pub fn private_subdirectory(state_dir: &Path, name: &str) -> Result<(PathBuf, PrivateDirectory)> {
    let path = connections_dir(state_dir)?.join(name);
    let directory = PrivateDirectory::open(&path)?;
    Ok((path, directory))
}

/// A profile, alias or device name usable as one file name component.
pub fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !name.starts_with('-')
}

/// One record replaced atomically under its own lock.
pub struct RecordStore {
    directory: PrivateDirectory,
    record: CString,
    lock: CString,
}

impl RecordStore {
    /// `record` in an already opened private directory.
    pub fn new(directory: PrivateDirectory, record: &str) -> Result<Self> {
        Ok(Self {
            directory,
            record: CString::new(record)?,
            lock: CString::new(format!("{record}.lock"))?,
        })
    }

    /// Read the record without taking the lock (atomic replace keeps it whole).
    pub fn read(&self) -> Result<Option<Vec<u8>>> {
        self.directory.read_record(&self.record, RECORD_LIMIT)
    }

    /// Serialized read/modify/write. A record set to `None` is removed.
    pub fn transaction<T>(&self, f: impl FnOnce(&mut Option<Vec<u8>>) -> Result<T>) -> Result<T> {
        let _lock = self.directory.lock(&self.lock)?;
        let original = self.directory.read_record(&self.record, RECORD_LIMIT)?;
        let mut value = original.clone();
        let result = f(&mut value)?;
        if value
            .as_ref()
            .is_some_and(|bytes| bytes.len() as u64 > RECORD_LIMIT)
        {
            bail!("private record exceeds 1 MiB limit");
        }
        if value != original {
            if let Some(bytes) = value {
                self.directory.replace_record(&self.record, &bytes)?;
            } else if original.is_some() {
                self.directory.remove_record(&self.record)?;
            }
        }
        Ok(result)
    }
}

/// A held exclusive lock file; released on drop.
pub struct HeldLock {
    _file: std::fs::File,
}

/// Take `name` in `directory` without waiting.
pub fn try_lock(directory: &PrivateDirectory, name: &CStr) -> Result<Option<HeldLock>> {
    Ok(directory
        .try_lock(name)?
        .map(|file| HeldLock { _file: file }))
}
