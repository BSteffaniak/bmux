//! Snapshot-owned durable catalog. Entries belong to exact runtime targets;
//! retaining another entry never authorizes restoring it into this runtime.
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bmux_snapshot_plugin_api::envelope::CombinedSnapshotEnvelope;
use bmux_snapshot_runtime::{
    SnapshotOrchestratorError as Error, SnapshotOrchestratorResult as Result,
};
use serde_json::{Value, json};

pub(crate) struct ManagedCatalog {
    path: PathBuf,
    legacy: PathBuf,
    target: String,
    identity: Value,
}

impl ManagedCatalog {
    pub(crate) fn capture(legacy: &Path) -> Result<Option<Self>> {
        match (
            std::env::var("BMUX_MANAGED_TARGET_ID"),
            std::env::var("BMUX_MANAGED_TARGET_IDENTITY"),
        ) {
            (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => Ok(None),
            (Ok(target), Ok(identity)) => Self::new(legacy, target, &identity).map(Some),
            _ => Err(Error::Other(
                "snapshot managed scope requires target ID and identity together".into(),
            )),
        }
    }

    pub(crate) fn new(legacy: &Path, target: String, identity: &str) -> Result<Self> {
        validate_target(&target)?;
        let identity: Value =
            serde_json::from_str(identity).map_err(|error| Error::Codec(error.to_string()))?;
        if !identity.is_object() {
            return Err(Error::Codec(
                "snapshot managed identity must be an object".into(),
            ));
        }
        Ok(Self {
            path: legacy.with_file_name("bmux-managed-snapshots-v1.json"),
            legacy: legacy.to_path_buf(),
            target,
            identity,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> Result<File> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::Other("snapshot catalog has no parent".into()))?;
        std::fs::create_dir_all(parent).map_err(|error| io(&error))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))
            .map_err(|error| io(&error))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    return Err(Error::Other(format!(
                        "snapshot catalog authority unavailable: {error}"
                    )));
                }
            }
        }
    }

    fn read(&self) -> Result<Value> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({"version":1,"targets":{}}));
            }
            Err(error) => return Err(io(&error)),
        };
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|error| Error::Codec(error.to_string()))?;
        if value.as_object().is_none_or(|object| object.len() != 2)
            || value["version"].as_u64() != Some(1)
            || !value["targets"].is_object()
        {
            return Err(Error::Codec(
                "unsupported or corrupt managed snapshot catalog".into(),
            ));
        }
        for (target, entry) in value["targets"].as_object().expect("validated object") {
            validate_target(target)?;
            if entry.as_object().is_none_or(|object| object.len() != 2)
                || !entry["identity"].is_object()
            {
                return Err(Error::Codec(
                    "corrupt managed snapshot ownership entry".into(),
                ));
            }
            let envelope: CombinedSnapshotEnvelope =
                serde_json::from_value(entry["envelope"].clone())
                    .map_err(|error| Error::Codec(error.to_string()))?;
            envelope.validate()?;
        }
        Ok(value)
    }

    pub(crate) fn load(&self) -> Result<Option<CombinedSnapshotEnvelope>> {
        let _transaction = self.lock()?;
        let catalog = self.read()?;
        let Some(entry) = catalog["targets"].get(&self.target) else {
            return Ok(None);
        };
        self.verify_entry(entry)?;
        serde_json::from_value(entry["envelope"].clone())
            .map(Some)
            .map_err(|error| Error::Codec(error.to_string()))
    }

    fn verify_entry(&self, entry: &Value) -> Result<()> {
        if entry["identity"] != self.identity {
            return Err(Error::Other(
                "managed snapshot exact target identity mismatch; restore and replacement refused"
                    .into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn save(&self, envelope: &CombinedSnapshotEnvelope) -> Result<()> {
        envelope.validate()?;
        let _transaction = self.lock()?;
        let mut catalog = self.read()?;
        if let Some(entry) = catalog["targets"].get(&self.target) {
            self.verify_entry(entry)?;
        }
        catalog["targets"][self.target.as_str()] =
            json!({"identity":self.identity,"envelope":envelope});
        let bytes =
            serde_json::to_vec_pretty(&catalog).map_err(|error| Error::Codec(error.to_string()))?;
        super::orchestrator::write_bytes_atomic(&self.path, &bytes)
    }

    /// Discovery is explicit even for a new target. Never choose a "latest"
    /// layout: another target's live processes and IDs are not ours to restore.
    pub(crate) fn discovery(&self) -> Result<String> {
        let _transaction = self.lock()?;
        let catalog = self.read()?;
        let targets = catalog["targets"]
            .as_object()
            .expect("validated object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let legacy = match std::fs::symlink_metadata(&self.legacy) {
            Ok(_) => format!(
                "legacy snapshot retained at {} (explicit recovery only)",
                self.legacy.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                "no legacy snapshot".into()
            }
            Err(error) => return Err(io(&error)),
        };
        Ok(format!(
            "selected target {}; retained layout targets [{}] in {}; {legacy}; no cross-target restore",
            self.target,
            targets.join(", "),
            self.path.display()
        ))
    }
}

fn validate_target(target: &str) -> Result<()> {
    if target.len() != 64
        || !target
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Codec(
            "managed snapshot target must be a lowercase SHA-256 identity".into(),
        ));
    }
    Ok(())
}
fn io(error: &std::io::Error) -> Error {
    Error::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entries_merge_without_cross_target_restore_or_legacy_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("bmux-snapshot-v1.json");
        std::fs::write(&legacy, b"retained legacy bytes").unwrap();
        let first = ManagedCatalog::new(&legacy, "a".repeat(64), r#"{"build":"one"}"#).unwrap();
        let second = ManagedCatalog::new(&legacy, "b".repeat(64), r#"{"build":"two"}"#).unwrap();
        let envelope = CombinedSnapshotEnvelope::build(Vec::new()).unwrap();
        first.save(&envelope).unwrap();
        assert!(second.load().unwrap().is_none());
        assert!(second.discovery().unwrap().contains(&"a".repeat(64)));
        second.save(&envelope).unwrap();
        assert!(first.load().unwrap().is_some());
        assert!(second.load().unwrap().is_some());
        assert_eq!(std::fs::read(&legacy).unwrap(), b"retained legacy bytes");
        let wrong = ManagedCatalog::new(&legacy, "a".repeat(64), r#"{"build":"other"}"#).unwrap();
        assert!(wrong.load().is_err());
        assert!(wrong.save(&envelope).is_err());
    }
    #[test]
    fn independent_target_writers_merge_all_layouts() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("legacy.json");
        let workers: Vec<_> = (0..4)
            .map(|index| {
                let legacy = legacy.clone();
                std::thread::spawn(move || {
                    let target = format!("{index:064x}");
                    let catalog = ManagedCatalog::new(&legacy, target, "{}").unwrap();
                    for _ in 0..10 {
                        catalog
                            .save(&CombinedSnapshotEnvelope::build(Vec::new()).unwrap())
                            .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let catalog = ManagedCatalog::new(&legacy, "0".repeat(64), "{}").unwrap();
        assert_eq!(
            catalog.read().unwrap()["targets"]
                .as_object()
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn unknown_catalog_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let catalog =
            ManagedCatalog::new(&temp.path().join("legacy"), "a".repeat(64), "{}").unwrap();
        std::fs::write(catalog.path(), br#"{"version":2,"targets":{}}"#).unwrap();
        let before = std::fs::read(catalog.path()).unwrap();
        assert!(
            catalog
                .save(&CombinedSnapshotEnvelope::build(Vec::new()).unwrap())
                .is_err()
        );
        assert_eq!(std::fs::read(catalog.path()).unwrap(), before);
    }
}
