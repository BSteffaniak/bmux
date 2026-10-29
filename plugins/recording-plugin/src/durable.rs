//! Durable replacement shared by recording-owned metadata.
use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("durable file has no parent")?;
    let temporary = parent.join(format!(".recording-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = crate::platform::create_new(&temporary)
            .with_context(|| format!("creating durable temporary file {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing durable file {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing durable file {}", path.display()))?;
        drop(file);
        crate::platform::publish(&temporary, path)
            .with_context(|| format!("publishing durable file {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    crate::platform::private_directory(path).with_context(|| {
        format!(
            "creating or validating private recording directory {}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_is_complete_and_leaves_no_temporary_file() {
        let root = std::env::temp_dir().join(format!("bmux-durable-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("metadata.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    #[test]
    fn private_tree_atomic_replace_and_no_replace_publication() {
        let root = std::env::temp_dir().join(format!("bmux-private-{}", uuid::Uuid::new_v4()));
        private_directory(&root).unwrap();
        private_directory(&root.join("nested/child")).unwrap();
        let path = root.join("state");
        write_atomic(&path, b"old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let candidate = root.join("candidate");
        write_atomic(&candidate, b"candidate").unwrap();
        assert!(crate::platform::publish_noreplace(&candidate, &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read(&candidate).unwrap(), b"candidate");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn failed_atomic_write_reports_the_operation_and_path() {
        let root = std::env::temp_dir().join(format!("bmux-missing-{}", uuid::Uuid::new_v4()));
        let error = write_atomic(&root.join("state"), b"data").unwrap_err();
        assert!(format!("{error:#}").contains("creating durable temporary file"));
        assert!(format!("{error:#}").contains(&root.display().to_string()));
    }
}
