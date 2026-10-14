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
        self.publish_inner(revision, json, Some(binary), resources, None)
    }
}

pub(crate) fn validate(
    max_bytes: usize,
    revision: u64,
    binary: Option<&BinaryPresentation>,
    resources: &BTreeMap<String, Resource>,
    current: Option<&Snapshot>,
) -> io::Result<()> {
    let invalid = |message| io::Error::new(io::ErrorKind::InvalidInput, message);
    if let Some(binary) = binary {
        let valid_bytes = |bytes: &Arc<[u8]>| !bytes.is_empty() && bytes.len() <= max_bytes;
        if !valid_bytes(&binary.snapshot) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "browser binary snapshot bytes rejected: used={}, limit={max_bytes}",
                    binary.snapshot.len()
                ),
            ));
        }
        if let Some((base, bytes)) = &binary.delta
            && (bytes.is_empty()
                || bytes.len() > max_bytes
                || *base >= revision
                || !current.is_some_and(|value| value.revision == *base && value.binary.is_some()))
        {
            return Err(invalid(
                "browser delta rejected: invalid size or predecessor revision",
            ));
        }
    }
    if resources.len() > MAX_RESOURCES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "browser resource count budget exceeded: used={}, limit={MAX_RESOURCES}",
                resources.len()
            ),
        ));
    }
    let mut total = current.map_or(0, |value| {
        value.resources.values().map(|r| r.bytes.len()).sum()
    });
    for (id, resource) in resources {
        let mime = &resource.mime_type;
        if resource.bytes.len() > MAX_RESOURCE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "browser image resource bytes exceeded: used={}, limit={MAX_RESOURCE_BYTES}",
                    resource.bytes.len()
                ),
            ));
        }
        if !valid_id(id)
            || mime.is_empty()
            || mime.len() > 128
            || !mime.contains('/')
            || !mime.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
        {
            return Err(invalid(
                "browser resource manifest rejected: invalid SHA-256 ID or MIME",
            ));
        }
        let existing = current.and_then(|value| {
            value
                .resources
                .get(id)
                .or_else(|| value.previous_resources.get(id))
        });
        // Retained entries were digest-checked at their original admission. Exact
        // MIME/content equality preserves that proof; an ID alone never does.
        // Shared immutable allocations avoid both comparison and rehashing.
        let unchanged = existing.is_some_and(|value| {
            value.mime_type == resource.mime_type
                && (Arc::ptr_eq(&value.bytes, &resource.bytes) || value.bytes == resource.bytes)
        });
        let valid_content = if existing.is_some() {
            unchanged
        } else {
            let digest = Sha256::digest(&resource.bytes);
            let expected: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            id == &expected
        };
        if !valid_content {
            return Err(invalid(
                "browser resource rejected: digest mismatch or changed retained content",
            ));
        }
        total += resource.bytes.len();
        if total > MAX_RETAINED_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "retained browser image resource budget exceeded: used={total}, limit={MAX_RETAINED_BYTES}, resources={}",
                    resources.len()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod retained_resource_tests {
    use super::*;
    use crate::snapshot_feed;

    fn manifest(bytes: Arc<[u8]>) -> BTreeMap<String, Resource> {
        let id = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        BTreeMap::from([(
            id,
            Resource {
                bytes,
                mime_type: "application/octet-stream".into(),
            },
        )])
    }

    fn full() -> BinaryPresentation {
        BinaryPresentation {
            snapshot: Arc::from([1_u8]),
            delta: None,
        }
    }

    #[test]
    fn reuse_requires_exact_content_and_mime_and_preserves_atomicity() {
        let (publisher, feed) = snapshot_feed(128).unwrap();
        let resources = manifest(Arc::from([2_u8, 3]));
        let id = resources.keys().next().unwrap().clone();
        publisher
            .publish_representations(1, None, full(), resources.clone())
            .unwrap();
        let mut copied = resources.clone();
        copied.get_mut(&id).unwrap().bytes = Arc::from([2_u8, 3]);
        assert!(!Arc::ptr_eq(&resources[&id].bytes, &copied[&id].bytes));
        publisher
            .publish_representations(2, None, full(), copied)
            .unwrap();
        for changed in [
            Resource {
                bytes: Arc::from([2_u8, 4]),
                mime_type: "application/octet-stream".into(),
            },
            Resource {
                bytes: Arc::clone(&resources[&id].bytes),
                mime_type: "image/png".into(),
            },
        ] {
            assert!(
                publisher
                    .publish_representations(
                        3,
                        None,
                        full(),
                        BTreeMap::from([(id.clone(), changed)]),
                    )
                    .is_err()
            );
            assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 2);
        }
        publisher
            .publish_representations(3, None, full(), resources)
            .unwrap();
        assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 3);
    }

    #[test]
    fn previous_manifest_reuse_and_eviction_keep_digest_validation() {
        let (publisher, feed) = snapshot_feed(128).unwrap();
        let resources = manifest(Arc::from([2_u8, 3]));
        publisher
            .publish_representations(1, None, full(), resources.clone())
            .unwrap();
        publisher
            .publish_representations(2, None, full(), BTreeMap::new())
            .unwrap();
        publisher
            .publish_representations(3, None, full(), resources.clone())
            .unwrap();
        for revision in [4, 5] {
            publisher
                .publish_representations(revision, None, full(), BTreeMap::new())
                .unwrap();
        }
        let mut corrupt = resources.clone();
        corrupt.values_mut().next().unwrap().bytes = Arc::from([9_u8]);
        assert!(
            publisher
                .publish_representations(6, None, full(), corrupt)
                .is_err()
        );
        assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 5);
        publisher
            .publish_representations(6, None, full(), resources)
            .unwrap();
    }

    #[test]
    fn shared_reuse_does_not_relax_retained_byte_budget() {
        let (publisher, feed) = snapshot_feed(128).unwrap();
        let mut resources = BTreeMap::new();
        for value in 0..3_u8 {
            resources.extend(manifest(Arc::from(vec![value; MAX_RESOURCE_BYTES])));
        }
        publisher
            .publish_representations(1, None, full(), resources.clone())
            .unwrap();
        assert!(
            publisher
                .publish_representations(2, None, full(), resources)
                .is_err()
        );
        assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 1);
    }

    /// Admission-only timing harness: no renderer, network, or end-to-end claims.
    #[test]
    #[ignore = "manual resource admission measurement; coordinator runs explicitly"]
    fn resource_admission_measurement() {
        use std::hint::black_box;
        use std::time::Instant;

        let (publisher, feed) = snapshot_feed(128).unwrap();
        let resources = manifest(Arc::from(vec![7_u8; 1024 * 1024]));
        publisher
            .publish_representations(1, None, full(), resources.clone())
            .unwrap();
        let current = feed.receiver.borrow();
        let mut copied = resources.clone();
        copied.values_mut().next().unwrap().bytes = Arc::from(vec![7_u8; 1024 * 1024]);
        for (label, candidate, retained) in [
            ("unseen/hash", &resources, None),
            ("retained/shared", &resources, current.as_deref()),
            ("retained/copied", &copied, current.as_deref()),
        ] {
            let started = Instant::now();
            for _ in 0..100 {
                validate(128, 2, None, black_box(candidate), black_box(retained)).unwrap();
            }
            eprintln!(
                "{label}: 100 admissions, 1 MiB payload, {:?}",
                started.elapsed()
            );
        }
    }
}
