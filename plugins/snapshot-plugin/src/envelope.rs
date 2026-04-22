//! Combined snapshot envelope schema (version 1).
//!
//! The envelope is the on-disk format the snapshot plugin writes when
//! it performs a save: a monotonic version, an integrity checksum, and
//! a `Vec<SectionV1>` where each section is one participant's opaque
//! payload plus its schema metadata.

use bmux_snapshot_runtime::SnapshotOrchestratorError;
use serde::{Deserialize, Serialize};

/// Current envelope schema version. Bump when the envelope wrapper
/// itself changes (not the participant sections, which track their
/// own versions).
pub const COMBINED_SNAPSHOT_VERSION: u32 = 1;

/// Combined snapshot envelope — the outer format written to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CombinedSnapshotEnvelope {
    pub version: u32,
    pub checksum: u64,
    pub sections: Vec<SectionV1>,
}

impl CombinedSnapshotEnvelope {
    /// Build an envelope from participant sections, computing a fresh
    /// checksum over the serialized sections.
    pub(crate) fn build(sections: Vec<SectionV1>) -> Result<Self, SnapshotOrchestratorError> {
        let checksum = sections_checksum(&sections)?;
        Ok(Self {
            version: COMBINED_SNAPSHOT_VERSION,
            checksum,
            sections,
        })
    }

    /// Validate the envelope's version + recompute its checksum.
    pub(crate) fn validate(&self) -> Result<(), SnapshotOrchestratorError> {
        if self.version != COMBINED_SNAPSHOT_VERSION {
            return Err(SnapshotOrchestratorError::Codec(format!(
                "unsupported envelope version {} (expected {})",
                self.version, COMBINED_SNAPSHOT_VERSION
            )));
        }
        let expected = sections_checksum(&self.sections)?;
        if expected != self.checksum {
            return Err(SnapshotOrchestratorError::Codec(
                "envelope checksum mismatch".to_string(),
            ));
        }
        Ok(())
    }
}

/// One per-participant section of the combined envelope.
///
/// `id` identifies which `StatefulPlugin` produced the payload (so we
/// can route it back on restore), `version` is the participant's own
/// schema version, and `bytes` is the opaque payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SectionV1 {
    pub id: String,
    pub version: u32,
    pub bytes: Vec<u8>,
}

/// FNV-1a 64 checksum over the JSON-serialized sections. Cheap and
/// deterministic; matches the helper the legacy `SnapshotV4` file used.
fn sections_checksum(sections: &[SectionV1]) -> Result<u64, SnapshotOrchestratorError> {
    let bytes = serde_json::to_vec(sections).map_err(|e| {
        SnapshotOrchestratorError::Codec(format!("encoding sections for checksum: {e}"))
    })?;
    Ok(fnv1a64(&bytes))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{COMBINED_SNAPSHOT_VERSION, CombinedSnapshotEnvelope, SectionV1};

    fn sample_sections() -> Vec<SectionV1> {
        vec![
            SectionV1 {
                id: "bmux.clients/follow-state".into(),
                version: 1,
                bytes: b"{\"connected_clients\":[]}".to_vec(),
            },
            SectionV1 {
                id: "bmux.sessions/session-manager".into(),
                version: 1,
                bytes: b"[]".to_vec(),
            },
        ]
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let envelope = CombinedSnapshotEnvelope::build(sample_sections()).expect("build envelope");
        assert_eq!(envelope.version, COMBINED_SNAPSHOT_VERSION);
        let json = serde_json::to_vec(&envelope).expect("encode");
        let decoded: CombinedSnapshotEnvelope = serde_json::from_slice(&json).expect("decode");
        assert_eq!(decoded, envelope);
        decoded.validate().expect("validate");
    }

    #[test]
    fn tampered_checksum_fails_validation() {
        let mut envelope =
            CombinedSnapshotEnvelope::build(sample_sections()).expect("build envelope");
        envelope.checksum = envelope.checksum.wrapping_add(1);
        let err = envelope
            .validate()
            .expect_err("should reject tampered checksum");
        assert!(err.to_string().contains("checksum"));
    }

    #[test]
    fn wrong_version_fails_validation() {
        let envelope = CombinedSnapshotEnvelope {
            version: 99,
            checksum: 0,
            sections: vec![],
        };
        let err = envelope.validate().expect_err("should reject version");
        assert!(err.to_string().contains("version"));
    }
}
