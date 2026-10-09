//! Atomic owner-private receipt storage. Corrupt or unknown state is an error.
use crate::atomic_state::commit;
use serde::{Deserialize, Serialize};
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
            if record.version != 1 || record.fingerprint != fingerprint {
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
            if count >= 128
                || bytes + u64::try_from(fingerprint.len()).map_err(std::io::Error::other)?
                    > 1_048_576
            {
                return Err(std::io::Error::other("mutation receipt capacity exhausted"));
            }
            commit(
                &path,
                &MutationRecord {
                    version: 1,
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
            version: 1,
            fingerprint,
            result: Some(result),
        },
    )
}
