//! Atomic owner-private receipt storage. Corrupt or unknown state is an error.
use crate::atomic_state::commit;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize)]
pub(super) struct MutationRecord {
    pub version: u32,
    pub fingerprint: Vec<u8>,
    pub result: Option<Vec<u8>>,
}

pub(super) fn admit(
    root: &Path,
    mutation: &str,
    fingerprint: Vec<u8>,
) -> std::io::Result<(PathBuf, Option<Vec<u8>>)> {
    // Stable UUID identities prevent caller-controlled filesystem traversal.
    let mutation = uuid::Uuid::parse_str(mutation).map_err(std::io::Error::other)?;
    let path = root.join(format!("receipt-{mutation}.json"));
    match std::fs::read(&path) {
        Ok(bytes) => {
            let record: MutationRecord =
                serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
            let expected = match record.version {
                1 => fingerprint.clone(),
                2 => Sha256::digest(&fingerprint).to_vec(),
                _ => return Err(std::io::Error::other("unsupported mutation receipt")),
            };
            if record.fingerprint != expected {
                return Err(std::io::Error::other(
                    "conflicting or unsupported mutation receipt",
                ));
            }
            let result = record.result.ok_or_else(|| {
                std::io::Error::other("mutation outcome uncertain; explicit resolution required")
            })?;
            Ok((path, Some(result)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut count = 0;
            let mut bytes = 0;
            for entry in std::fs::read_dir(root)? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with("receipt-") {
                    count += 1;
                    bytes += entry.metadata()?.len();
                }
            }
            // Retain identity, not the complete environment encoded a second
            // time as a JSON byte array. Large valid launches must leave room
            // for their cleanup/retirement receipts.
            let fingerprint = Sha256::digest(&fingerprint).to_vec();
            if count >= 128
                || bytes + u64::try_from(fingerprint.len()).map_err(std::io::Error::other)?
                    > 1_048_576
            {
                return Err(std::io::Error::other("mutation receipt capacity exhausted"));
            }
            commit(
                &path,
                &MutationRecord {
                    version: 2,
                    fingerprint,
                    result: None,
                },
            )?;
            Ok((path, None))
        }
        Err(error) => Err(error),
    }
}

pub(super) fn complete(path: &Path, fingerprint: Vec<u8>, result: Vec<u8>) -> std::io::Result<()> {
    commit(
        path,
        &MutationRecord {
            version: 2,
            fingerprint: Sha256::digest(&fingerprint).to_vec(),
            result: Some(result),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_launch_leaves_room_for_cleanup_and_preserves_replay_identity() {
        let root = tempfile::tempdir().unwrap();
        let mutation = uuid::Uuid::new_v4().to_string();
        let fingerprint = vec![255; 4 * 1024 * 1024];
        let (path, previous) = admit(root.path(), &mutation, fingerprint.clone()).unwrap();
        assert!(previous.is_none());
        assert!(admit(root.path(), &mutation, fingerprint.clone()).is_err());
        complete(&path, fingerprint.clone(), b"started".to_vec()).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 1024);
        assert_eq!(
            admit(root.path(), &mutation, fingerprint).unwrap().1,
            Some(b"started".to_vec())
        );
        assert!(admit(root.path(), &mutation, b"different".to_vec()).is_err());
        admit(
            root.path(),
            &uuid::Uuid::new_v4().to_string(),
            b"retire".to_vec(),
        )
        .unwrap();
    }

    #[test]
    fn legacy_receipts_remain_replayable_but_conflicts_and_unknown_versions_fail() {
        let root = tempfile::tempdir().unwrap();
        let mutation = uuid::Uuid::new_v4().to_string();
        let path = root.path().join(format!("receipt-{mutation}.json"));
        let mut record = MutationRecord {
            version: 1,
            fingerprint: b"launch".to_vec(),
            result: Some(b"started".to_vec()),
        };
        commit(&path, &record).unwrap();
        assert_eq!(
            admit(root.path(), &mutation, b"launch".to_vec()).unwrap().1,
            record.result
        );
        assert!(admit(root.path(), &mutation, b"different".to_vec()).is_err());
        record.version = 3;
        commit(&path, &record).unwrap();
        assert!(admit(root.path(), &mutation, b"launch".to_vec()).is_err());
    }
}
