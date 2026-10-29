use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

pub(super) fn create(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}
pub(super) fn publish(temporary: &Path, path: &Path, parent: &Path) -> std::io::Result<()> {
    std::fs::rename(temporary, path)?;
    std::fs::File::open(parent)?.sync_all()
}
