#[cfg(test)]
use super::RecordStore;
use anyhow::{Context, Result, bail};
use std::ffi::{CStr, CString};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

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

    pub(crate) fn lock(&self, name: &CStr) -> Result<File> {
        // Open a fresh description per transaction to serialize threads as well
        // as processes. Publish once with O_EXCL, then open existing names without
        // O_CREAT: concurrent O_CREAT and flock can race with ENOENT on macOS.
        // Never unlink the shared lock name.
        let lock = match self.open_file(
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NONBLOCK,
        ) {
            Ok(lock) => lock,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.open_file(name, libc::O_RDWR | libc::O_NONBLOCK)?
            }
            Err(error) => return Err(error).context("create pairing store lock"),
        };
        check_private(&lock, false)?;
        loop {
            // SAFETY: lock owns a live descriptor, released on every return path.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(lock);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("lock pairing store");
            }
        }
    }

    /// Take an exclusive lock without waiting; `None` when another holder has it.
    pub(crate) fn try_lock(&self, name: &CStr) -> Result<Option<File>> {
        let lock = match self.open_file(
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NONBLOCK,
        ) {
            Ok(lock) => lock,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.open_file(name, libc::O_RDWR | libc::O_NONBLOCK)?
            }
            Err(error) => return Err(error).context("create private lock"),
        };
        check_private(&lock, false)?;
        loop {
            // SAFETY: lock owns a live descriptor, released on every return path.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(lock));
            }
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::Interrupted => {}
                std::io::ErrorKind::WouldBlock => return Ok(None),
                _ => return Err(error).context("lock private record"),
            }
        }
    }

    pub(crate) fn remove_record(&self, name: &CStr) -> Result<()> {
        // SAFETY: checked directory and static single-component name.
        if unsafe { libc::unlinkat(self.handle.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error()).context("remove pairing record");
        }
        self.handle.sync_all()?;
        Ok(())
    }

    pub(crate) fn read_record(&self, name: &CStr, limit: u64) -> Result<Option<Vec<u8>>> {
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

    /// Atomically create `name` unless it exists; returns the winning bytes.
    pub(crate) fn publish_record(&self, name: &CStr, bytes: &[u8]) -> Result<Vec<u8>> {
        let temporary = CString::new(format!(".publish-{}", uuid::Uuid::new_v4()))?;
        let mut file = self.open_file(&temporary, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        let result = self.publish_from(name, &temporary, &mut file, bytes);
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

    fn publish_from(
        &self,
        name: &CStr,
        temporary: &CStr,
        file: &mut File,
        bytes: &[u8],
    ) -> Result<Vec<u8>> {
        file.write_all(bytes)?;
        file.sync_all()?;
        // SAFETY: both names are valid C strings; the directory fd is live. linkat
        // atomically fails with EEXIST rather than replacing a concurrent winner.
        let result = unsafe {
            libc::linkat(
                self.handle.as_raw_fd(),
                temporary.as_ptr(),
                self.handle.as_raw_fd(),
                name.as_ptr(),
                0,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error).context("publish private record");
            }
        }
        self.read_record(name, 65_536)?
            .context("published record disappeared")
    }

    pub(crate) fn replace_record(&self, name: &CStr, bytes: &[u8]) -> Result<()> {
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

    fn increment(store: &RecordStore) {
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

    fn store(root: &Path) -> RecordStore {
        RecordStore::new(
            PrivateDirectory::open(&root.join("private")).unwrap(),
            "count.json",
        )
        .unwrap()
    }

    #[test]
    fn transaction_subprocess() {
        let Some(root) = std::env::var_os("BMUX_PAIRING_TEST_ROOT") else {
            return;
        };
        let store = store(Path::new(&root));
        for _ in 0..12 {
            increment(&store);
        }
    }

    #[test]
    fn transactions_serialize_across_processes_and_threads() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let mut children: Vec<_> = (0..4)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "storage::unix::tests::transaction_subprocess"])
                    .env("BMUX_PAIRING_TEST_ROOT", root.path())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
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
        assert_eq!(
            serde_json::from_slice::<u64>(&store.read().unwrap().unwrap()).unwrap(),
            96
        );
    }

    #[test]
    fn errors_and_oversize_leave_state_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
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
        assert_eq!(store.read().unwrap().as_deref(), Some(b"1".as_slice()));
        let path = root.path().join("private/count.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.transaction(|_| Ok(())).is_err());
    }

    #[test]
    fn publish_keeps_the_first_winner_and_try_lock_excludes() {
        let root = tempfile::tempdir().unwrap();
        let directory = PrivateDirectory::open(&root.path().join("private")).unwrap();
        assert_eq!(directory.publish_record(c"key", b"one").unwrap(), b"one");
        assert_eq!(directory.publish_record(c"key", b"two").unwrap(), b"one");
        let held = directory
            .try_lock(c"host.lock")
            .unwrap()
            .expect("first lock");
        assert!(directory.try_lock(c"host.lock").unwrap().is_none());
        drop(held);
        assert!(directory.try_lock(c"host.lock").unwrap().is_some());
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
    }

    #[test]
    fn symlink_records_and_directories_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let path = root.path().join("private");
        symlink(&target, &path).unwrap();
        assert!(PrivateDirectory::open(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        let store = store(root.path());
        symlink(&target, path.join("count.json")).unwrap();
        assert!(store.transaction(|_| Ok(())).is_err());
    }
}
