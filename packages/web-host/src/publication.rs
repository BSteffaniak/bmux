//! Opaque encoded representations and bounded publication resource manifests.
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;

use crate::{Snapshot, SnapshotPublisher};

const MAX_RESOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RETAINED_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESOURCES: usize = 1024;

/// Version-one encoded full snapshot and optional exact-predecessor delta.
#[derive(Clone, Debug)]
pub struct BinaryPresentation {
    pub snapshot: Arc<[u8]>,
    pub delta: Option<(u64, Arc<[u8]>)>,
}

/// Immutable opaque resource; length is derived from bytes, never caller supplied.
/// IDs must never be reused for different content, including after eviction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    pub bytes: Arc<[u8]>,
    pub mime_type: String,
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl SnapshotPublisher {
    /// Atomically replaces JSON, binary and the complete resource manifest.
    /// Retains resources of the latest and previous publication, not a frame queue.
    /// IDs are lowercase SHA-256 hex of resource bytes, verified at publication.
    /// Retained IDs may be reused only with unchanged MIME and bytes.
    /// Absent diagnostic JSON is served as HTTP 406 without blocking binary delivery.
    /// Resource limits: 8 MiB each, 1024 per manifest, 32 MiB across retained manifests.
    ///
    /// # Errors
    /// Rejects invalid sizes, IDs/MIME, changed retained resources, invalid delta bases,
    /// non-increasing revisions, closed feeds, and poisoned locks without publication.
    pub fn publish_representations(
        &self,
        revision: u64,
        json: Option<Arc<[u8]>>,
        binary: BinaryPresentation,
        resources: BTreeMap<String, Resource>,
    ) -> io::Result<()> {
        self.publish_inner(revision, json, Some(binary), resources)
    }
}

pub(crate) fn validate(
    max_bytes: usize,
    revision: u64,
    binary: Option<&BinaryPresentation>,
    resources: &BTreeMap<String, Resource>,
    current: Option<&Snapshot>,
) -> io::Result<()> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid publication representations",
        )
    };
    if let Some(binary) = binary {
        let valid_bytes = |bytes: &Arc<[u8]>| !bytes.is_empty() && bytes.len() <= max_bytes;
        if !valid_bytes(&binary.snapshot) {
            return Err(invalid());
        }
        if let Some((base, bytes)) = &binary.delta
            && (bytes.is_empty()
                || bytes.len() > max_bytes
                || *base >= revision
                || !current.is_some_and(|value| value.revision == *base && value.binary.is_some()))
        {
            return Err(invalid());
        }
    }
    if resources.len() > MAX_RESOURCES {
        return Err(invalid());
    }
    let mut total = current.map_or(0, |value| {
        value.resources.values().map(|r| r.bytes.len()).sum()
    });
    for (id, resource) in resources {
        let mime = &resource.mime_type;
        if !valid_id(id)
            || resource.bytes.len() > MAX_RESOURCE_BYTES
            || mime.is_empty()
            || mime.len() > 128
            || !mime.contains('/')
            || !mime.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
        {
            return Err(invalid());
        }
        let existing = current.and_then(|value| {
            value
                .resources
                .get(id)
                .or_else(|| value.previous_resources.get(id))
        });
        let digest = Sha256::digest(&resource.bytes);
        let expected: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        if id != &expected || existing.is_some_and(|value| value != resource) {
            return Err(invalid());
        }
        total += resource.bytes.len();
        if total > MAX_RETAINED_BYTES {
            return Err(invalid());
        }
    }
    Ok(())
}
