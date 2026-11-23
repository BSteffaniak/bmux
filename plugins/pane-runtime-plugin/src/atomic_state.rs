//! Atomic state publication shared by runtime and helper; native durability stays local.
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::platform::files::atomic;

pub(crate) fn commit(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("receipt parent missing"))?;
    // Unique to this write: concurrent writers (in other processes too) never share a staging
    // file, and a simulated run names it the same whichever process runs it.
    let temporary: PathBuf = path.with_extension(format!("tmp-{}", crate::ids::new_uuid()));
    let mut file = atomic::create(&temporary)
        .map_err(|error| stage_error("create state staging file", &error))?;
    file.write_all(&bytes)
        .map_err(|error| stage_error("write state staging file", &error))?;
    file.sync_all()
        .map_err(|error| stage_error("flush state staging file", &error))?;
    drop(file);
    atomic::publish(&temporary, path, parent)
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
        assert_eq!(
            crate::dir_order::read_dir_sorted(directory.path())
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn publication_replaces_state_that_readers_hold_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipt.json");
        super::commit(&path, &vec![1]).unwrap();
        let reader = std::fs::File::open(&path).unwrap();
        super::commit(&path, &vec![2]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[2]");
        drop(reader);
        super::commit(&path, &vec![3]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[3]");
    }
}
