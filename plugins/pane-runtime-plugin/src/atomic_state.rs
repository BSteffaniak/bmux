//! Atomic state publication shared by runtime and helper; native durability stays local.
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
#[path = "atomic_state/unix.rs"]
mod native;
#[cfg(windows)]
#[path = "atomic_state/windows.rs"]
mod native;

pub(crate) fn commit(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("receipt parent missing"))?;
    let temporary: PathBuf = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = native::create(&temporary)
        .map_err(|error| stage_error("create state staging file", &error))?;
    file.write_all(&bytes)
        .map_err(|error| stage_error("write state staging file", &error))?;
    file.sync_all()
        .map_err(|error| stage_error("flush state staging file", &error))?;
    drop(file);
    native::publish(&temporary, path, parent)
        .map_err(|error| stage_error("publish durable state replacement", &error))
}

fn stage_error(stage: &'static str, error: &std::io::Error) -> std::io::Error {
    // Preserve the error kind and OS code in the message without exposing state
    // paths or serialized contents. Never turn publication failure into success.
    std::io::Error::new(error.kind(), format!("{stage}: {error}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn publication_replaces_prior_complete_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipt.json");
        super::commit(&path, &vec![1, 2]).unwrap();
        super::commit(&path, &vec![3, 4]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[3,4]");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
