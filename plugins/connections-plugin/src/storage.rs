//! Immutable identity publication and serialized pairing transactions.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::ffi::{CStr, CString};
use std::fmt::Write as _;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
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
        // Open a fresh lock fd per transaction: flock locks open descriptions,
        // so sharing a single fd would not serialize concurrent threads.
        let lock = self
            .directory
            .open_file(self.lock, libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK)?;
        check_private(&lock, false)?;
        loop {
            // SAFETY: lock owns a live fd. Closing it releases the lock on every return path.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("lock pairing store");
            }
        }
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
                // SAFETY: checked directory fd and static NUL-terminated name.
                if unsafe {
                    libc::unlinkat(self.directory.handle.as_raw_fd(), self.record.as_ptr(), 0)
                } != 0
                {
                    return Err(std::io::Error::last_os_error()).context("remove pairing record");
                }
                self.directory.handle.sync_all()?;
            }
        }
        Ok(result)
    }
}

pub struct PrivateDirectory {
    handle: File,
}

impl PrivateDirectory {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create private identity directory"),
        }
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(path)?;
        check_private(&handle, true)?;
        // Persist the directory entry even when another initializer created it.
        File::open(path.parent().context("identity directory needs a parent")?)?.sync_all()?;
        Ok(Self { handle })
    }

    pub(crate) fn read(&self) -> Result<Option<Vec<u8>>> {
        self.read_record(c"identity.json", 16_384)
    }

    fn read_record(&self, name: &CStr, limit: u64) -> Result<Option<Vec<u8>>> {
        let mut file = match self.open_file(name, libc::O_RDONLY | libc::O_NONBLOCK) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("open persistent endpoint identity"),
        };
        check_private(&file, false)?;
        let mut bytes = Vec::new();
        (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len())? > limit {
            bail!("private record exceeds size limit");
        }
        file.sync_all()?;
        self.handle.sync_all()?;
        Ok(Some(bytes))
    }

    pub(crate) fn publish(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        let temporary = CString::new(format!(".identity-{}", uuid::Uuid::new_v4()))?;
        let mut file = self.open_file(&temporary, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        let result = self.publish_from(&temporary, &mut file, bytes);
        // SAFETY: directory fd and the NUL-terminated name remain valid for this call.
        // Only remove a temporary entry we successfully created, never the destination.
        let cleanup = unsafe { libc::unlinkat(self.handle.as_raw_fd(), temporary.as_ptr(), 0) };
        let cleanup_error = (cleanup != 0).then(std::io::Error::last_os_error);
        let winner = result?;
        if let Some(error) = cleanup_error {
            return Err(error).context("remove temporary identity record");
        }
        self.handle.sync_all()?;
        Ok(winner)
    }

    fn publish_from(&self, temporary: &CStr, file: &mut File, bytes: &[u8]) -> Result<Vec<u8>> {
        file.write_all(bytes)?;
        file.sync_all()?;
        // SAFETY: both names are valid C strings; the directory fd is live. linkat
        // atomically fails with EEXIST rather than replacing a concurrent winner.
        let result = unsafe {
            libc::linkat(
                self.handle.as_raw_fd(),
                temporary.as_ptr(),
                self.handle.as_raw_fd(),
                c"identity.json".as_ptr(),
                0,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error).context("publish persistent endpoint identity");
            }
        }
        self.read()?.context("published identity disappeared")
    }

    fn replace_record(&self, name: &CStr, bytes: &[u8]) -> Result<()> {
        let temporary = CString::new(format!(".pairing-{}", uuid::Uuid::new_v4()))?;
        let mut file = self.open_file(&temporary, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        let result = (|| -> Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            // SAFETY: live directory descriptor and valid NUL-terminated names.
            if unsafe {
                libc::renameat(
                    self.handle.as_raw_fd(),
                    temporary.as_ptr(),
                    self.handle.as_raw_fd(),
                    name.as_ptr(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error()).context("replace pairing record");
            }
            self.handle.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            // SAFETY: remove only our own temporary name, never the destination.
            unsafe {
                libc::unlinkat(self.handle.as_raw_fd(), temporary.as_ptr(), 0);
            }
        }
        result
    }

    fn open_file(&self, name: &CStr, flags: libc::c_int) -> std::io::Result<File> {
        // SAFETY: name is NUL-terminated, the parent fd is live, mode is supplied
        // for O_CREAT. Directory-relative IO pins the checked private directory.
        // C variadic arguments require integer promotion: mode_t is u16 on macOS,
        // so pass c_uint explicitly rather than the platform-dependent mode_t.
        let fd = unsafe {
            libc::openat(
                self.handle.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned a fresh owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn check_private(file: &File, directory: bool) -> Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions and does not access Rust memory.
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        bail!("endpoint identity storage must be owner-only and owned by the current user");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn roots(root: &Path) -> (PathBuf, PathBuf) {
        let state = root.join("state");
        let runtime = root.join("runtime");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        (state, runtime)
    }

    fn increment(store: &PairingStore) {
        store
            .transaction(|bytes| {
                let value = bytes
                    .as_ref()
                    .map_or(0, |bytes| serde_json::from_slice::<u64>(bytes).unwrap());
                *bytes = Some(serde_json::to_vec(&(value + 1))?);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn transaction_subprocess() {
        let Some(root) = std::env::var_os("BMUX_PAIRING_TEST_ROOT") else {
            return;
        };
        let (state, runtime) = roots(Path::new(&root));
        let store = PairingStore::open(&state, &runtime).unwrap();
        for _ in 0..12 {
            increment(&store);
        }
    }

    #[test]
    fn transactions_serialize_across_processes_and_threads() {
        let root = tempfile::tempdir().unwrap();
        let (state, runtime) = roots(root.path());
        let mut children: Vec<_> = (0..4)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "storage::tests::transaction_subprocess"])
                    .env("BMUX_PAIRING_TEST_ROOT", root.path())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let store = PairingStore::open(&state, &runtime).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let store = &store;
                scope.spawn(move || {
                    for _ in 0..12 {
                        increment(store);
                    }
                });
            }
        });
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        store
            .transaction(|bytes| {
                assert_eq!(serde_json::from_slice::<u64>(bytes.as_ref().unwrap())?, 96);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn errors_and_oversize_leave_state_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let (state, runtime) = roots(root.path());
        let store = PairingStore::open(&state, &runtime).unwrap();
        increment(&store);
        assert!(
            store
                .transaction::<()>(|bytes| {
                    *bytes = Some(b"changed".to_vec());
                    bail!("abort")
                })
                .is_err()
        );
        assert!(
            store
                .transaction(|bytes| {
                    *bytes = Some(vec![0; 1_048_577]);
                    Ok(())
                })
                .is_err()
        );
        store
            .transaction(|bytes| {
                assert_eq!(bytes.as_deref(), Some(b"1".as_slice()));
                Ok(())
            })
            .unwrap();
        let path = runtime_directory(&state, &runtime)
            .unwrap()
            .join("pairing.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.transaction(|_| Ok(())).is_err());
    }

    #[test]
    fn directory_relative_creation_preserves_private_mode() {
        let root = tempfile::tempdir().unwrap();
        let directory = PrivateDirectory::open(&root.path().join("private")).unwrap();
        let file = directory
            .open_file(c"created", libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)
            .unwrap();
        check_private(&file, false).unwrap();
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        drop(file);
        let reopened = directory.open_file(c"created", libc::O_RDONLY).unwrap();
        check_private(&reopened, false).unwrap();
        assert_eq!(reopened.metadata().unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn symlink_records_and_directories_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let (state, runtime) = roots(root.path());
        let path = runtime_directory(&state, &runtime).unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(PairingStore::open(&state, &runtime).is_err());
        std::fs::remove_file(&path).unwrap();
        let store = PairingStore::open(&state, &runtime).unwrap();
        symlink(&target, path.join("pairing.json")).unwrap();
        assert!(store.transaction(|_| Ok(())).is_err());
    }
}
