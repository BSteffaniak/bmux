//! Opaque full documents. Interpretation and local composition belong to the adapter.
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use serde::Serialize;

use crate::{Resource, Snapshot, SnapshotPublisher};

/// Product-neutral semantic content, paired atomically with its complete resource manifest.
/// Generations identify replaced sources; revisions increase within a generation.
#[derive(Clone, Debug)]
pub struct DocumentPublication {
    pub generation: String,
    pub revision: u64,
    /// Explicit versioned adapter capability, not a product dispatch selector in the host.
    pub capability: String,
    pub content: Arc<[u8]>,
}

#[derive(Clone, Debug)]
pub(crate) struct EncodedDocument {
    pub generation: String,
    pub revision: u64,
    pub capability: String,
    pub bytes: Arc<[u8]>,
}

#[derive(Serialize)]
struct ResourceDescriptor<'a> {
    id: &'a str,
    mime_type: &'a str,
    length: usize,
}

#[derive(Serialize)]
struct Envelope<'a> {
    version: u8,
    publication_revision: String,
    generation: &'a str,
    revision: String,
    capability: &'a str,
    content: &'a [u8],
    resources: Vec<ResourceDescriptor<'a>>,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

impl SnapshotPublisher {
    /// Publishes a bounded full document and SHA-256 resources as one replacement.
    /// The encoded JSON envelope (including byte-array expansion) must fit the feed
    /// budget and the browser's 16 MiB ceiling. IDs/capabilities are 1–128 ASCII
    /// alphanumeric characters or `._:-`. Revisions are decimal strings on the wire.
    /// Fixed-frame and document publications are explicitly distinct representations.
    ///
    /// # Errors
    /// Rejects oversized content/envelopes, invalid identities/resources, non-increasing
    /// transport or same-generation document revisions, and closed/poisoned feeds.
    pub fn publish_document(
        &self,
        publication_revision: u64,
        document: DocumentPublication,
        resources: BTreeMap<String, Resource>,
    ) -> io::Result<()> {
        let limit = self.max_bytes.min(16 * 1024 * 1024);
        if publication_revision == 0
            || document.revision == 0
            || !identifier(&document.generation)
            || !identifier(&document.capability)
            || document.content.is_empty()
            || document.content.len() > limit / 4
            || resources.len() > 1024
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid document identity or bounds",
            ));
        }
        // Validate resources before serializing attacker-sized descriptor strings.
        crate::publication::validate(self.max_bytes, publication_revision, None, &resources, None)?;
        let envelope = Envelope {
            version: 1,
            publication_revision: publication_revision.to_string(),
            generation: &document.generation,
            revision: document.revision.to_string(),
            capability: &document.capability,
            content: &document.content,
            resources: resources
                .iter()
                .map(|(id, resource)| ResourceDescriptor {
                    id,
                    mime_type: &resource.mime_type,
                    length: resource.bytes.len(),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec(&envelope).map_err(io::Error::other)?;
        if bytes.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "document envelope exceeds bounds",
            ));
        }
        let encoded = EncodedDocument {
            generation: document.generation,
            revision: document.revision,
            capability: document.capability,
            bytes: bytes.into(),
        };
        self.publish_inner(publication_revision, None, None, resources, Some(encoded))
    }
}

pub(crate) fn validate(
    document: Option<&EncodedDocument>,
    current: Option<&Snapshot>,
) -> io::Result<()> {
    if let (Some(next), Some(previous)) = (document, current.and_then(|s| s.document.as_ref()))
        && next.generation == previous.generation
        && (next.revision <= previous.revision || next.capability != previous.capability)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "document revision or capability changed without a new generation",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(generation: &str, revision: u64) -> DocumentPublication {
        DocumentPublication {
            generation: generation.into(),
            revision,
            capability: "test.document-v1".into(),
            content: Arc::from(&b"opaque content"[..]),
        }
    }

    #[test]
    fn document_identity_and_resources_commit_atomically() {
        let (publisher, feed) = crate::snapshot_feed(4096).unwrap();
        publisher
            .publish_document(1, document("a", 7), BTreeMap::new())
            .unwrap();
        assert!(
            publisher
                .publish_document(2, document("a", 7), BTreeMap::new())
                .is_err()
        );
        assert!(
            publisher
                .publish_document(2, document("a", 6), BTreeMap::new())
                .is_err()
        );
        let mut changed = document("a", 8);
        changed.capability = "test.other-v1".into();
        assert!(
            publisher
                .publish_document(2, changed, BTreeMap::new())
                .is_err()
        );
        let bad_resources = BTreeMap::from([(
            "0".repeat(64),
            Resource {
                bytes: Arc::from(&b"not this digest"[..]),
                mime_type: "image/png".into(),
            },
        )]);
        assert!(
            publisher
                .publish_document(2, document("a", 8), bad_resources)
                .is_err()
        );
        assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 1);
        publisher
            .publish_document(2, document("b", 1), BTreeMap::new())
            .unwrap();
        let current = feed.receiver.borrow();
        let snapshot = current.as_ref().unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&snapshot.document.as_ref().unwrap().bytes).unwrap();
        assert_eq!(value["revision"], "1");
        assert_eq!(value["publication_revision"], "2");
        assert_eq!(value["generation"], "b");
        assert!(snapshot.binary.is_none());
        assert!(!snapshot.json_available);
    }

    #[test]
    fn document_bounds_and_exact_u64_wire_identity() {
        let (publisher, feed) = crate::snapshot_feed(4096).unwrap();
        let mut invalid = document("a", 1);
        invalid.content = Arc::from(vec![255; 4096]);
        assert!(
            publisher
                .publish_document(1, invalid, BTreeMap::new())
                .is_err()
        );
        assert!(
            publisher
                .publish_document(0, document("a", 1), BTreeMap::new())
                .is_err()
        );
        assert!(
            publisher
                .publish_document(1, document("a\r\nb", 1), BTreeMap::new())
                .is_err()
        );
        publisher
            .publish_document(u64::MAX, document("a", u64::MAX), BTreeMap::new())
            .unwrap();
        let current = feed.receiver.borrow();
        let value: serde_json::Value =
            serde_json::from_slice(&current.as_ref().unwrap().document.as_ref().unwrap().bytes)
                .unwrap();
        assert_eq!(value["revision"], u64::MAX.to_string());
        assert_eq!(value["publication_revision"], u64::MAX.to_string());
    }
}
