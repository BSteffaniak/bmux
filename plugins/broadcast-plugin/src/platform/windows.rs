use super::windows_security::{Handle, Security, wide};
use std::{
    fs::File,
    io,
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Path, PathBuf},
    ptr::null_mut,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE},
    Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        MoveFileExW,
    },
};

fn checked(path: &Path, directory: bool) -> io::Result<File> {
    checked_with_policy(path, directory, false)
}

fn checked_with_policy(path: &Path, directory: bool, parent: bool) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options
        .access_mode(GENERIC_READ | 0x0002_0000 /* READ_CONTROL */)
        // A checked directory cannot be renamed/replaced while its child is
        // created. Files retain their normal sharing policy for publication.
        .share_mode(if directory { 3 } else { 7 })
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    use std::os::windows::fs::MetadataExt;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || metadata.is_dir() != directory
        || (!directory && !metadata.is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reparse point or object type mismatch",
        ));
    }
    let security = Security::current_user()?;
    if parent {
        security.check_parent(file.as_raw_handle())?;
    } else {
        security.check_file(file.as_raw_handle())?;
    }
    Ok(file)
}
pub(crate) fn open_private(path: &Path) -> io::Result<File> {
    checked(path, false)
}
pub(crate) fn create_new(path: &Path) -> io::Result<File> {
    let security = Security::current_user()?;
    let attributes = security.attributes();
    let name = wide(path.as_os_str())?;
    // SAFETY: security descriptor and UTF-16 path live through CreateFileW; handle is exclusively owned.
    let handle = Handle::new(unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE | GENERIC_READ | 0x0002_0000,
            0,
            &attributes,
            CREATE_NEW,
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    })?;
    security.check_file(handle.0)?;
    let raw = handle.0;
    std::mem::forget(handle);
    Ok(unsafe { File::from_raw_handle(raw) })
}
pub(crate) fn private_dir(path: &Path, create: bool) -> Result<(), &'static str> {
    private_dir_checked(path, create).map_err(|error| {
        // Preserve the public error contract, but retain the exact failing
        // security stage in diagnostics (paths only, never credential contents).
        eprintln!("broadcast private directory {}: {error:#}", path.display());
        "private_directory_required"
    })
}

fn private_dir_checked(path: &Path, create: bool) -> io::Result<()> {
    if !create {
        return checked(path, true).map(drop).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("validate existing private leaf: {error}"),
            )
        });
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing private directory parent"))?;
    let parent = checked_with_policy(parent, true, true).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("validate owner-controlled parent: {error}"),
        )
    })?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("missing private directory leaf"))?;
    super::windows_directory::open_or_create(&parent, name)
        .map(drop)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("create or validate owner-private leaf: {error}"),
            )
        })
}
pub(crate) fn lock(path: &Path) -> io::Result<File> {
    let file = match create_new(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => open_private(path)?,
        Err(e) => return Err(e),
    };
    file.try_lock()
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(file)
}
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    // Windows does not support flushing directory handles. Private directory
    // identity is checked; publication uses write-through MoveFileExW instead.
    checked(path, true).map(drop)
}
pub(crate) fn publish_new(source: &Path, target: &Path) -> io::Result<()> {
    let source = wide(source.as_os_str())?;
    let target = wide(target.as_os_str())?;
    // SAFETY: terminated names live through the call. Without REPLACE_EXISTING,
    // concurrent or tampered assets remain untouched. WRITE_THROUGH completes
    // namespace publication before acknowledging the extracted asset.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn publish(source: &Path, target: &Path) -> io::Result<()> {
    let source = wide(source.as_os_str())?;
    let target = wide(target.as_os_str())?;
    // SAFETY: both terminated path buffers remain live for the synchronous call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) struct ObserverListener {
    server: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    endpoint: PathBuf,
}
impl ObserverListener {
    pub(crate) fn bind() -> io::Result<Self> {
        let endpoint = PathBuf::from(format!(r"\\.\pipe\bmux-broadcast-{}", uuid::Uuid::new_v4()));
        let security = Security::current_user()?;
        let mut attributes = security.attributes();
        // SAFETY: the descriptor remains live through pipe creation and is copied by Windows.
        let server = unsafe {
            tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    &endpoint,
                    std::ptr::from_mut(&mut attributes).cast(),
                )
        }?;
        Ok(Self {
            server: Some(server),
            endpoint,
        })
    }
    pub(crate) fn endpoint(&self) -> &Path {
        &self.endpoint
    }
    pub(crate) async fn accept(
        &mut self,
        child: u32,
    ) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        let server = self
            .server
            .take()
            .ok_or_else(|| io::Error::other("observer already accepted"))?;
        server.connect().await?;
        let mut peer = 0;
        // SAFETY: connected pipe handle and output PID pointer are valid.
        if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut peer) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if peer != child {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "observer child identity mismatch",
            ));
        }
        Ok(server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn observer_accepts_only_the_owned_child_identity() {
        use tokio::net::windows::named_pipe::ClientOptions;
        let mut listener = ObserverListener::bind().unwrap();
        let client = ClientOptions::new().open(listener.endpoint()).unwrap();
        // The test process is the connecting peer; claiming any other PID fails.
        let wrong = std::process::id().wrapping_add(1);
        assert!(listener.accept(wrong).await.is_err());
        drop(client);

        let mut listener = ObserverListener::bind().unwrap();
        let client = ClientOptions::new().open(listener.endpoint()).unwrap();
        let server = listener.accept(std::process::id()).await.unwrap();
        drop(server);
        drop(client);
    }

    #[test]
    fn public_acl_is_rejected_without_repairing_the_credential() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("public");
        drop(create_new(&path).unwrap());
        set_dacl(&path, "D:P(A;;GA;;;WD)");
        assert!(open_private(&path).is_err());
        assert!(open_private(&path).is_err());
    }

    fn set_dacl(path: &Path, sddl: &str) {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SE_FILE_OBJECT,
            SetNamedSecurityInfoW,
        };
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
            PROTECTED_DACL_SECURITY_INFORMATION,
        };
        let sddl = wide(std::ffi::OsStr::new(sddl)).unwrap();
        let mut descriptor = null_mut();
        // SAFETY: valid terminated descriptor and writable output pointer.
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    null_mut(),
                )
            },
            0
        );
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = null_mut();
        // SAFETY: descriptor is allocated by Windows; all output pointers are valid.
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            },
            0
        );
        let name = wide(path.as_os_str()).unwrap();
        // SAFETY: path/ACL buffers remain valid through the synchronous call.
        let error = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        unsafe { LocalFree(descriptor) };
        assert_eq!(error, 0);
    }
    #[test]
    fn controlled_parent_and_private_leaf_have_distinct_acl_policies() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("controlled");
        private_dir_checked(&parent, true).expect("private parent creation");
        // SYSTEM ACE is safe on a parent but deliberately not private on a leaf.
        let security = Security::current_user().unwrap();
        let file = checked(&parent, true).unwrap();
        drop(file);
        set_dacl(&parent, &security.parent_test_descriptor().unwrap());
        let parent_handle =
            checked_with_policy(&parent, true, true).expect("controlled parent ACL");
        security
            .check_parent(parent_handle.as_raw_handle())
            .unwrap();
        assert!(checked(&parent, true).is_err());
        let leaf = parent.join("leaf");
        private_dir_checked(&leaf, true).expect("atomically protected leaf");
        assert!(checked(&leaf, true).is_ok());
        // Existing public leaf is never repaired, even with create=true.
        set_dacl(&leaf, "D:P(A;;GA;;;WD)");
        assert!(private_dir_checked(&leaf, true).is_err());
        assert!(private_dir_checked(&leaf, false).is_err());
        // A parent writable by everyone is not an acceptable creation authority.
        set_dacl(&parent, "D:P(A;;GA;;;WD)");
        assert!(private_dir_checked(&parent.join("forbidden"), true).is_err());
        assert!(!parent.join("forbidden").exists());
    }
}
