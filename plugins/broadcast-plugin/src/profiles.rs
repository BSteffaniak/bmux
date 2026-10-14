//! Plugin-owned durable profiles. Only local credential file references are persisted.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
};
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub endpoint: String,
    pub channel: String,
    pub credential_file: PathBuf,
    #[serde(default)]
    pub terminal: TerminalDefaults,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalDefaults {
    pub cols: u32,
    pub rows: u32,
    pub executable: Option<PathBuf>,
}
impl Default for TerminalDefaults {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            executable: None,
        }
    }
}
impl Profile {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        crate::relay::validate_endpoint(&self.endpoint, &self.channel)?;
        if self.terminal.cols == 0
            || self.terminal.rows == 0
            || self.terminal.cols > 65535
            || self.terminal.rows > 65535
            || u64::from(self.terminal.cols) * u64::from(self.terminal.rows) > 262144
            || self
                .terminal
                .executable
                .as_ref()
                .is_some_and(|p| !p.is_absolute())
        {
            return Err("invalid_terminal_defaults");
        }
        if !self.credential_file.is_absolute() {
            return Err("absolute_credential_path_required");
        }
        Ok(())
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profiles {
    pub version: u32,
    pub default: Option<String>,
    pub profiles: BTreeMap<String, Profile>,
}
impl Default for Profiles {
    fn default() -> Self {
        Self {
            version: 1,
            default: None,
            profiles: BTreeMap::new(),
        }
    }
}
pub(crate) fn name(value: &str) -> Result<(), &'static str> {
    if value.is_empty()
        || value.len() > 48
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("invalid_profile_name");
    }
    Ok(())
}
#[cfg(unix)]
pub(crate) fn private_dir(path: &Path, create: bool) -> Result<(), &'static str> {
    use std::os::unix::fs::MetadataExt;
    if create {
        let parent = path.parent().ok_or("private_directory_required")?;
        let metadata =
            std::fs::symlink_metadata(parent).map_err(|_| "private_directory_required")?;
        // SAFETY: geteuid only observes this process's effective identity.
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err("private_directory_required");
        }
        let mut builder = std::fs::DirBuilder::new();
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
        match builder.create(path) {
            Ok(()) => {
                std::fs::File::open(parent)
                    .and_then(|file| file.sync_all())
                    .map_err(|_| "private_directory_required")?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err("private_directory_required"),
        }
    }
    let m = std::fs::symlink_metadata(path).map_err(|_| "private_directory_required")?;
    // SAFETY: geteuid only reads the effective process identity.
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
    {
        return Err("private_directory_required");
    }
    Ok(())
}
#[cfg(not(unix))]
pub(crate) fn private_dir(_path: &Path, _create: bool) -> Result<(), &'static str> {
    Err("private_storage_unsupported")
}
fn open_private(path: &Path) -> Result<std::fs::File, &'static str> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(|_| "profiles_unavailable")?;
    let m = file.metadata().map_err(|_| "profiles_unavailable")?;
    if !m.is_file() || m.len() > 65536 {
        return Err("profiles_bound");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid only reads the effective process identity.
        if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
            return Err("private_profiles_required");
        }
    }
    Ok(file)
}
pub(crate) fn load(root: &Path) -> Result<Profiles, &'static str> {
    let dir = root.join("broadcast");
    match std::fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Profiles::default()),
        Err(_) => return Err("profiles_unavailable"),
        Ok(_) => private_dir(&dir, false)?,
    }
    let path = dir.join("profiles.json");
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Profiles::default()),
        Err(_) => return Err("profiles_unavailable"),
        Ok(_) => {}
    }
    let file = open_private(&path)?;
    let mut bytes = Vec::new();
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| "profiles_unavailable")?;
    if bytes.len() > 65536 {
        return Err("profiles_bound");
    }
    let profiles: Profiles = serde_json::from_slice(&bytes).map_err(|_| "invalid_profiles")?;
    if profiles.version != 1 || profiles.profiles.len() > 32 {
        return Err("invalid_profiles");
    }
    for (key, profile) in &profiles.profiles {
        name(key)?;
        profile.validate()?;
    }
    if profiles
        .default
        .as_ref()
        .is_some_and(|key| !profiles.profiles.contains_key(key))
    {
        return Err("invalid_default_profile");
    }
    Ok(profiles)
}
pub(crate) fn selected(root: &Path, selected: Option<&str>) -> Result<Profile, &'static str> {
    let profiles = load(root)?;
    let key = selected
        .or(profiles.default.as_deref())
        .ok_or("run_broadcast_setup")?;
    name(key)?;
    profiles
        .profiles
        .get(key)
        .cloned()
        .ok_or("profile_not_found")
}
/// Lock the entire read-modify-write transaction. Never used by inspection.
pub(crate) fn update(
    root: &Path,
    key: &str,
    profile: Profile,
    default: bool,
) -> Result<(), &'static str> {
    name(key)?;
    profile.validate()?;
    let dir = root.join("broadcast");
    private_dir(&dir, true)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let lock = options
        .open(dir.join("profiles.lock"))
        .map_err(|_| "profiles_locked")?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let m = lock.metadata().map_err(|_| "profiles_locked")?;
        // SAFETY: geteuid reads identity; flock operates on our owned valid descriptor.
        if !m.is_file()
            || m.mode() & 0o077 != 0
            || m.uid() != unsafe { libc::geteuid() }
            || unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
        {
            return Err("profiles_locked_retry");
        }
    }
    let mut profiles = load(root)?;
    if !profiles.profiles.contains_key(key) && profiles.profiles.len() >= 32 {
        return Err("profile_limit");
    }
    profiles.profiles.insert(key.into(), profile);
    if default {
        profiles.default = Some(key.into());
    }
    save(&dir, &profiles)
}
fn save(dir: &Path, profiles: &Profiles) -> Result<(), &'static str> {
    let bytes = serde_json::to_vec_pretty(profiles).map_err(|_| "invalid_profiles")?;
    if bytes.len() > 65536 {
        return Err("profiles_bound");
    }
    let temp = dir.join(format!("profiles-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(|_| "profiles_unwritable")?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, dir.join("profiles.json"))?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result.map_err(|_| "profiles_unwritable")
}
pub(crate) fn import(root: &Path, profile: &str, source: &Path) -> Result<PathBuf, &'static str> {
    name(profile)?;
    let key = crate::relay::credential_key(source)?;
    let parent = root.join("broadcast");
    private_dir(&parent, true)?;
    let dir = parent.join("credentials");
    private_dir(&dir, true)?;
    let path = dir.join(format!("{profile}-{}", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&path).map_err(|_| "credential_unwritable")?;
    file.write_all(key.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::File::open(&dir)?.sync_all())
        .map_err(|_| "credential_unwritable")?;
    Ok(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn profile(channel: &str) -> Profile {
        Profile {
            endpoint: "https://example.com".into(),
            channel: channel.into(),
            credential_file: "/private/key".into(),
            terminal: TerminalDefaults::default(),
        }
    }
    #[test]
    fn inspection_does_not_create_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().profiles.is_empty());
        assert!(!dir.path().join("broadcast").exists());
    }
    #[test]
    fn explicit_precedes_default() {
        let dir = tempfile::tempdir().unwrap();
        update(dir.path(), "a", profile("a"), true).unwrap();
        update(dir.path(), "b", profile("b"), false).unwrap();
        assert_eq!(selected(dir.path(), None).unwrap().channel, "a");
        assert_eq!(selected(dir.path(), Some("b")).unwrap().channel, "b");
        assert!(selected(dir.path(), Some("missing")).is_err());
    }
    #[test]
    fn corrupt_is_not_reset() {
        let dir = tempfile::tempdir().unwrap();
        update(dir.path(), "a", profile("a"), true).unwrap();
        std::fs::write(dir.path().join("broadcast/profiles.json"), b"{").unwrap();
        assert!(update(dir.path(), "b", profile("b"), false).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("broadcast/profiles.json")).unwrap(),
            b"{"
        );
    }
    #[cfg(unix)]
    #[test]
    fn credentials_symlink_is_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        private_dir(&dir.path().join("broadcast"), true).unwrap();
        symlink(target.path(), dir.path().join("broadcast/credentials")).unwrap();
        let key = dir.path().join("key");
        std::fs::write(&key, "x".repeat(32)).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(import(dir.path(), "a", &key).is_err());
    }
    #[test]
    fn concurrent_transactions_preserve_updates_or_reject_lock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for key in ["a", "b"] {
            let root = root.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                (key, update(&root, key, profile(key), false))
            }));
        }
        barrier.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        let profiles = load(&root).unwrap();
        for (key, result) in results {
            match result {
                Ok(()) => assert!(profiles.profiles.contains_key(key)),
                Err(code) => assert_eq!(code, "profiles_locked_retry"),
            }
        }
    }
    #[test]
    fn names_are_single_components() {
        for key in ["", "../a", "a/b", "a\n"] {
            assert!(name(key).is_err());
        }
    }
}
