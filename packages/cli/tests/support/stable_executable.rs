//! A Cargo-built executable at a path whose file identity is stable.
//!
//! `CARGO_BIN_EXE_<name>` names the executable Cargo uplifts into the profile
//! directory, and Cargo replaces that file (a new copy, not a hard link) on every
//! invocation, even when nothing was rebuilt. macOS assesses an executable the first
//! time it runs as a new file, which for a debug bmux or bterm (over a gigabyte) takes
//! seconds: every test run paid it again on its first launch. Tests instead run a hard
//! link of the build, published once per build under the same file name (bmux derives
//! its slot from it) in a directory that identifies the build. Shared by the bmux and
//! bterm integration tests.
use std::path::{Path, PathBuf};

/// The executable tests run for `uplifted` (a `CARGO_BIN_EXE_<name>` path): a hard
/// link of the same build that keeps its identity until the executable is rebuilt, or
/// `uplifted` itself when no link can be made.
#[must_use]
pub fn cargo_bin(uplifted: &str) -> PathBuf {
    // Resolved once per test process and executable.
    static RESOLVED: std::sync::Mutex<Vec<(String, PathBuf)>> = std::sync::Mutex::new(Vec::new());
    let mut resolved = RESOLVED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, path)) = resolved.iter().find(|(name, _)| name == uplifted) {
        return path.clone();
    }
    let path = stable_link(Path::new(uplifted)).unwrap_or_else(|| PathBuf::from(uplifted));
    resolved.push((uplifted.to_owned(), path.clone()));
    path
}

fn stable_link(uplifted: &Path) -> Option<PathBuf> {
    let metadata = std::fs::metadata(uplifted).ok()?;
    // Length and modification time identify the build: Cargo's replacement copies
    // keep both, a rebuild changes them.
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let name = uplifted.file_name()?.to_str()?;
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("stable-executables");
    std::fs::create_dir_all(&root).ok()?;
    let build = format!("{name}-{}-{modified}", metadata.len());
    let published = root.join(&build).join(name);
    if published.exists() {
        return Some(published);
    }
    // Links to earlier builds pin those builds' bytes on disk.
    let earlier = format!("{name}-");
    for entry in std::fs::read_dir(&root).ok()?.flatten() {
        let other = entry.file_name();
        let other = other.to_string_lossy();
        if other.starts_with(&earlier) && other != build && !other.starts_with('.') {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let staging = root.join(format!(".{build}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir(&staging).ok()?;
    std::fs::hard_link(uplifted, staging.join(name)).ok()?;
    // Concurrent test processes may publish the same build; the first one wins.
    if std::fs::rename(&staging, root.join(&build)).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    published.exists().then_some(published)
}
