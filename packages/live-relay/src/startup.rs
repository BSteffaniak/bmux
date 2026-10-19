//! Pre-runtime startup policy. Only Linux may initialize volumes and drop root.
use std::io;

pub const AUTHORITY_PATH: &str = "/data/relay/authority.json";

#[cfg(target_os = "linux")]
mod linux;

/// Must run in single-threaded main, before credentials, Tokio, or listeners.
pub fn prepare() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::prepare()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // SAFETY: identity queries have no preconditions and do not change IDs.
        let (uid, euid, gid, egid) = unsafe {
            (
                libc::getuid(),
                libc::geteuid(),
                libc::getgid(),
                libc::getegid(),
            )
        };
        unprivileged_unix(uid, euid, gid, egid)
    }
    #[cfg(not(unix))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "relay startup is unsupported on this platform",
        ))
    }
}

// Pure policy also tested on Linux without changing the test process's IDs.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
fn unprivileged_unix(uid: u32, euid: u32, gid: u32, egid: u32) -> io::Result<()> {
    if uid == 0 || euid == 0 || gid == 0 || egid == 0 || uid != euid || gid != egid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "non-Linux relay startup requires non-root, matching real/effective user and group IDs",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_unix_identity_is_allowed() {
        unprivileged_unix(501, 501, 20, 20).unwrap();
    }

    #[test]
    fn privileged_and_transitioned_unix_identities_are_refused() {
        for ids in [
            (0, 0, 20, 20),
            (0, 501, 20, 20),
            (501, 0, 20, 20),
            (501, 502, 20, 20),
            (501, 501, 0, 20),
            (501, 501, 20, 0),
            (501, 501, 0, 0),
            (501, 501, 20, 21),
        ] {
            assert_eq!(
                unprivileged_unix(ids.0, ids.1, ids.2, ids.3)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied,
            );
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn native_startup_matches_identity_policy() {
        // SAFETY: read-only process identity queries; no privilege mutation.
        let ids = unsafe {
            (
                libc::getuid(),
                libc::geteuid(),
                libc::getgid(),
                libc::getegid(),
            )
        };
        assert_eq!(
            prepare().is_ok(),
            unprivileged_unix(ids.0, ids.1, ids.2, ids.3).is_ok()
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn unsupported_platform_fails_closed() {
        assert_eq!(prepare().unwrap_err().kind(), io::ErrorKind::Unsupported);
    }
}
