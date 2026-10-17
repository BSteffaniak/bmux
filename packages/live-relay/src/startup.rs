//! Linux-only, pre-runtime volume setup. No configurable path is ever chowned.
use std::{
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::Path,
};

const SERVICE_ID: u32 = 65532;
pub const AUTHORITY_PATH: &str = "/data/relay/authority.json";
fn check(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn open_dir(parent: libc::c_int, name: &std::ffi::CStr) -> io::Result<File> {
    // SAFETY: NUL-terminated constant name, valid descriptor or AT_FDCWD; returned
    // owned descriptor is adopted exactly once. O_NOFOLLOW refuses final symlinks.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn validate_private(directory: &Path, uid: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.gid() != SERVICE_ID
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(io::Error::other("unsafe private authority directory"));
    }
    // Only two bounded metadata files are permitted. Never adopt foreign entries.
    for (index, entry) in fs::read_dir(directory)?.enumerate() {
        if index >= 2 {
            return Err(io::Error::other("unexpected authority entries"));
        }
        let entry = entry?;
        if entry.file_name() != "authority.json" && entry.file_name() != "authority.tmp" {
            return Err(io::Error::other("foreign authority entry"));
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.gid() != SERVICE_ID
            || metadata.nlink() != 1
            || metadata.mode() & 0o7777 != 0o600
            || metadata.len() > 4096
        {
            return Err(io::Error::other("unsafe authority file"));
        }
    }
    Ok(())
}
fn initialize_volume(data: &File) -> io::Result<()> {
    let metadata = data.metadata()?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::other(
            "data volume must be root owned and not publicly writable",
        ));
    }
    let pinned_data = format!("/proc/self/fd/{}", data.as_raw_fd());
    for (index, entry) in fs::read_dir(&pinned_data)?.enumerate() {
        if index >= 2 {
            return Err(io::Error::other("foreign volume entries"));
        }
        let entry = entry?;
        if entry.file_name() == "relay" {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if entry.file_name() != "lost+found"
            || !metadata.is_dir()
            || metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
        {
            return Err(io::Error::other("foreign volume entry; refusing migration"));
        }
    }
    // SAFETY: valid open directory fd and fixed NUL-terminated child name.
    let created = unsafe { libc::mkdirat(data.as_raw_fd(), c"relay".as_ptr(), 0o700) };
    if created == -1 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
        return Err(io::Error::last_os_error());
    }
    let directory = open_dir(data.as_raw_fd(), c"relay")?;
    if created == 0 {
        // Only the directory created by this call is chowned, through its fd.
        // No recursion, environment input, user file, or volume-root ownership change.
        check(unsafe { libc::fchown(directory.as_raw_fd(), SERVICE_ID, SERVICE_ID) })?;
        check(unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) })?;
        directory.sync_all()?;
        data.sync_all()?;
    }
    let pinned_path = format!("/proc/self/fd/{}", directory.as_raw_fd());
    // Validate through pinned descriptor, not a replaceable pathname. The descriptor
    // is a procfs symlink, so inspect its target's metadata separately.
    let metadata = directory.metadata()?;
    if metadata.uid() != SERVICE_ID
        || metadata.gid() != SERVICE_ID
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(io::Error::other("foreign authority directory"));
    }
    validate_entries(Path::new(&pinned_path))?;
    Ok(())
}
fn validate_entries(pinned: &Path) -> io::Result<()> {
    for (index, entry) in fs::read_dir(pinned)?.enumerate() {
        if index >= 2 {
            return Err(io::Error::other("unexpected authority entries"));
        }
        let entry = entry?;
        if entry.file_name() != "authority.json" && entry.file_name() != "authority.tmp" {
            return Err(io::Error::other("foreign authority entry"));
        }
        let m = fs::symlink_metadata(entry.path())?;
        if !m.is_file()
            || m.uid() != SERVICE_ID
            || m.gid() != SERVICE_ID
            || m.nlink() != 1
            || m.mode() & 0o7777 != 0o600
            || m.len() > 4096
        {
            return Err(io::Error::other("unsafe authority file"));
        }
    }
    Ok(())
}
/// Must run in single-threaded main, before reading credentials or constructing Tokio.
pub fn prepare() -> io::Result<()> {
    // SAFETY: getuid/geteuid have no preconditions or memory arguments.
    let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    if uid != euid {
        return Err(io::Error::other("setuid execution unsupported"));
    }
    if euid != 0 {
        return Ok(());
    }
    // Root may only use the fixed production location. Read no credential env here.
    if let Some(path) = std::env::var_os("AUTHORITY_PATH")
        && path != AUTHORITY_PATH
    {
        return Err(io::Error::other(
            "root startup requires fixed AUTHORITY_PATH",
        ));
    }
    let data = open_dir(libc::AT_FDCWD, c"/data")?;
    initialize_volume(&data)?;
    // SAFETY: single-threaded privileged startup; no borrowed pointers. Drop all
    // supplementary groups, real/effective/saved GID then UID. Every failure aborts.
    check(unsafe { libc::setgroups(0, std::ptr::null()) })?;
    check(unsafe { libc::setresgid(SERVICE_ID, SERVICE_ID, SERVICE_ID) })?;
    check(unsafe { libc::setresuid(SERVICE_ID, SERVICE_ID, SERVICE_ID) })?;
    check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })?;
    let (uid, euid, gid, egid) = unsafe {
        (
            libc::getuid(),
            libc::geteuid(),
            libc::getgid(),
            libc::getegid(),
        )
    };
    if (uid, euid, gid, egid) != (SERVICE_ID, SERVICE_ID, SERVICE_ID, SERVICE_ID) {
        return Err(io::Error::other("privilege drop failed"));
    }
    validate_private(Path::new("/data/relay"), SERVICE_ID)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn unsafe_volume_and_foreign_symlink_are_refused() {
        let path = std::env::temp_dir().join(format!("relay-startup-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        let file = File::open(&path).unwrap();
        // Ordinary test user cannot accidentally initialize/chown arbitrary paths.
        if file.metadata().unwrap().uid() != 0 {
            assert!(initialize_volume(&file).is_err());
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(initialize_volume(&file).is_err());
        symlink("/tmp", path.join("relay")).unwrap();
        assert!(open_dir(file.as_raw_fd(), c"relay").is_err());
        fs::remove_file(path.join("relay")).unwrap();
        fs::remove_dir(path).unwrap();
    }
}
