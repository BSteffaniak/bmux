//! Durable no-replay fences contain only random source operation identities, never secrets.
use std::{io::Write, path::Path};
pub(crate) fn claim(root: &Path, source: &str) -> Result<(), &'static str> {
    let id = uuid::Uuid::parse_str(source).map_err(|_| "invalid_source")?;
    let directory = root.join("broadcast-starts");
    crate::profiles::private_dir(&directory, true)?;
    if std::fs::read_dir(&directory)
        .map_err(|_| "start_identity_unavailable")?
        .take(1024)
        .count()
        >= 1024
    {
        return Err("start_identity_limit");
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(directory.join(id.to_string())).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            "start_identity_superseded"
        } else {
            "start_identity_unwritable"
        }
    })?;
    file.write_all(b"broadcast-start-v1\n")
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::File::open(&directory)?.sync_all())
        .map_err(|_| "start_identity_unwritable")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_cannot_replay_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        claim(dir.path(), &id).unwrap();
        assert_eq!(claim(dir.path(), &id), Err("start_identity_superseded"));
    }
}
