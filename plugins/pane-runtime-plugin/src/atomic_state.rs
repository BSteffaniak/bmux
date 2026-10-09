//! Atomic owner-private state publication shared by runtime and helper.
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn commit(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let temporary: PathBuf = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(
        path.parent()
            .ok_or_else(|| std::io::Error::other("receipt parent missing"))?,
    )?
    .sync_all()
}
