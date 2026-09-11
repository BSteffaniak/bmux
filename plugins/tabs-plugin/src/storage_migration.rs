//! One-time filesystem migration. Run only with the previous runtime stopped.
//! Renames preserve every file; interrupted runs resume from the new directory.
use std::path::Path;

pub fn migrate(data_dir: &Path) -> Result<(), String> {
    migrate_inner(data_dir).map_err(|error| format!("tab storage migration failed: {error}"))
}

fn migrate_inner(data_dir: &Path) -> std::io::Result<()> {
    let root = data_dir.join("plugin-storage");
    let old = root.join("bmux.windows");
    let new = root.join("bmux.tabs");
    if old.try_exists()? {
        if new.try_exists()? {
            return Err(std::io::Error::other(
                "both bmux.windows and bmux.tabs storage exist; stop all runtimes and explicitly reconcile them before restarting",
            ));
        }
        std::fs::rename(&old, &new)?;
        sync_directory(&root)?;
    }
    if !new.try_exists()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(&new)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix("windows.")) else {
            continue;
        };
        let destination = new.join(format!("tabs.{suffix}"));
        if destination.try_exists()? {
            return Err(std::io::Error::other(format!(
                "conflicting storage files: {} and {}",
                entry.path().display(),
                destination.display()
            )));
        }
        std::fs::rename(entry.path(), destination)?;
        sync_directory(&new)?;
    }
    // Also complete durability after an interruption between rename and sync.
    sync_directory(&new)?;
    sync_directory(&root)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "automatic tab storage migration requires directory synchronization; migrate offline on a supported filesystem",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_bytes_and_resumes_partial_migration() {
        let root =
            std::env::temp_dir().join(format!("bmux-tab-migration-{}", uuid::Uuid::new_v4()));
        let old = root.join("plugin-storage/bmux.windows");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("windows.order.bin"), b"exact original bytes").unwrap();
        std::fs::write(old.join("other.bin"), b"other state").unwrap();
        migrate(&root).unwrap();
        let new = root.join("plugin-storage/bmux.tabs");
        assert_eq!(
            std::fs::read(new.join("tabs.order.bin")).unwrap(),
            b"exact original bytes"
        );
        assert_eq!(
            std::fs::read(new.join("other.bin")).unwrap(),
            b"other state"
        );
        std::fs::write(new.join("windows.order.workspace.bin"), b"workspace bytes").unwrap();
        migrate(&root).unwrap();
        migrate(&root).unwrap();
        assert_eq!(
            std::fs::read(new.join("tabs.order.workspace.bin")).unwrap(),
            b"workspace bytes"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn conflicting_authorities_fail_without_overwrite() {
        let root =
            std::env::temp_dir().join(format!("bmux-tab-migration-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("plugin-storage/bmux.windows")).unwrap();
        std::fs::create_dir_all(root.join("plugin-storage/bmux.tabs")).unwrap();
        assert!(migrate(&root).unwrap_err().contains("both"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
