//! Immutable identity publication and serialized pairing transactions.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Durable local-process namespace. Profiles are explicit and never inferred from
/// an ephemeral runtime path or the process environment.
pub fn client_directory(state_dir: &Path, profile: &str) -> Result<PathBuf> {
    if !state_dir.is_absolute()
        || profile.is_empty()
        || profile.len() > 128
        || !profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("client storage requires an absolute state root and an explicit valid profile");
    }
    std::fs::create_dir_all(state_dir).context("create client state root")?;
    Ok(state_dir
        .canonicalize()?
        .join(format!("connections-client-{profile}")))
}

/// Resolve only explicit host paths; named runtimes sharing state get distinct namespaces.
pub fn runtime_directory(state_dir: &Path, runtime_dir: &Path) -> Result<PathBuf> {
    if !state_dir.is_absolute() || !runtime_dir.is_absolute() {
        bail!("storage requires explicit absolute host state and runtime paths");
    }
    let state = state_dir
        .canonicalize()
        .context("resolve host state directory")?;
    let runtime = runtime_dir
        .canonicalize()
        .context("resolve host runtime directory")?;
    let runtime = runtime.to_str().context("runtime path must be UTF-8")?;
    let namespace = Sha256::digest(runtime.as_bytes()).iter().fold(
        String::with_capacity(64),
        |mut text, byte| {
            write!(text, "{byte:02x}").expect("writing to String is infallible");
            text
        },
    );
    Ok(state.join(format!("connections-{namespace}")))
}

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
pub use unix::PrivateDirectory;
#[cfg(windows)]
pub use windows::PrivateDirectory;

use std::ffi::CStr;

/// Serialized durable byte transactions. The caller owns record schema validation.
/// A post-rename sync error is an indeterminate commit, never permission to replay
/// a one-shot operation without first reading authoritative state.
pub struct PairingStore {
    directory: PrivateDirectory,
    record: &'static CStr,
    lock: &'static CStr,
}

impl PairingStore {
    pub(crate) fn client(state_dir: &Path, profile: &str, record: &'static CStr) -> Result<Self> {
        Ok(Self {
            directory: PrivateDirectory::open(&client_directory(state_dir, profile)?)?,
            record,
            // One client lock also serializes migration and related client writes.
            lock: c"client.lock",
        })
    }

    /// Read only known client portions of legacy runtime stores. Host grants and
    /// endpoint identity are never read or moved by client migration.
    pub(crate) fn legacy_records(
        state_dir: &Path,
        record: &'static CStr,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let mut records = Vec::new();
        for entry in std::fs::read_dir(state_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(suffix) = name.strip_prefix("connections-") else {
                continue;
            };
            if suffix.len() != 64 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let directory = PrivateDirectory::open(&entry.path())?;
            if let Some(bytes) = directory.read_record(record, 1_048_576)? {
                records.push((name.to_owned(), bytes));
            }
        }
        records.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(records)
    }

    pub(crate) fn open(state_dir: &Path, runtime_dir: &Path) -> Result<Self> {
        Ok(Self {
            directory: PrivateDirectory::open(&runtime_directory(state_dir, runtime_dir)?)?,
            record: c"pairing.json",
            lock: c"pairing.lock",
        })
    }

    pub(crate) fn remote(state_dir: &Path, runtime_dir: &Path) -> Result<Self> {
        Ok(Self {
            directory: PrivateDirectory::open(&runtime_directory(state_dir, runtime_dir)?)?,
            record: c"remote.json",
            lock: c"remote.lock",
        })
    }

    #[cfg(test)]
    pub(crate) fn outbound(state_dir: &Path, runtime_dir: &Path) -> Result<Self> {
        Ok(Self {
            directory: PrivateDirectory::open(&runtime_directory(state_dir, runtime_dir)?)?,
            record: c"outbound.json",
            lock: c"outbound.lock",
        })
    }

    pub(crate) fn transaction<T>(
        &self,
        f: impl FnOnce(&mut Option<Vec<u8>>) -> Result<T>,
    ) -> Result<T> {
        let _lock = self.directory.lock(self.lock)?;
        let original = self.directory.read_record(self.record, 1_048_576)?;
        let mut value = original.clone();
        let result = f(&mut value)?;
        if value.as_ref().is_some_and(|bytes| bytes.len() > 1_048_576) {
            bail!("pairing record exceeds 1 MiB limit");
        }
        if value != original {
            if let Some(bytes) = value {
                self.directory.replace_record(self.record, &bytes)?;
            } else {
                self.directory.remove_record(self.record)?;
            }
        }
        Ok(result)
    }
}
