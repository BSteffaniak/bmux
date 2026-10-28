//! Closed, content-free ingest outcomes. Never format decoder errors into telemetry.
use bmux_web_renderer::DecodeError;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestReason {
    Oversize,
    PoisonedAuthority,
    Resources,
    WireVersion,
    DecodeLimit,
    MissingDeltaBase,
    DeltaBaseMismatch,
    Malformed,
    Unsupported,
    ZeroRevision,
    StaleRevision,
}
impl IngestReason {
    pub const ALL: [Self; 11] = [
        Self::Oversize,
        Self::PoisonedAuthority,
        Self::Resources,
        Self::WireVersion,
        Self::DecodeLimit,
        Self::MissingDeltaBase,
        Self::DeltaBaseMismatch,
        Self::Malformed,
        Self::Unsupported,
        Self::ZeroRevision,
        Self::StaleRevision,
    ];
    pub const fn label(self) -> &'static str {
        match self {
            Self::Oversize => "oversize",
            Self::PoisonedAuthority => "poisoned_authority",
            Self::Resources => "resources",
            Self::WireVersion => "wire_version",
            Self::DecodeLimit => "decode_limit",
            Self::MissingDeltaBase => "missing_delta_base",
            Self::DeltaBaseMismatch => "delta_base_mismatch",
            Self::Malformed => "malformed",
            Self::Unsupported => "unsupported",
            Self::ZeroRevision => "zero_revision",
            Self::StaleRevision => "stale_revision",
        }
    }
    pub(crate) fn decode(error: DecodeError) -> Self {
        match error {
            DecodeError::Version => Self::WireVersion,
            DecodeError::Limit(_) => Self::DecodeLimit,
            DecodeError::Invalid("missing delta base") => Self::MissingDeltaBase,
            DecodeError::Invalid("delta base") => Self::DeltaBaseMismatch,
            DecodeError::Unsupported(_) => Self::Unsupported,
            DecodeError::Invalid(_) | DecodeError::Json(_) => Self::Malformed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decoder_details_cannot_escape_closed_reason_labels() {
        for error in [
            DecodeError::Json("private-content-token".into()),
            DecodeError::Invalid("private-content-token"),
            DecodeError::Limit("private-content-token"),
            DecodeError::Unsupported("private-content-token"),
        ] {
            assert!(
                !IngestReason::decode(error)
                    .label()
                    .contains("private-content-token")
            );
        }
        assert_eq!(
            IngestReason::decode(DecodeError::Invalid("delta base")),
            IngestReason::DeltaBaseMismatch
        );
        assert_eq!(
            IngestReason::decode(DecodeError::Invalid("missing delta base")),
            IngestReason::MissingDeltaBase
        );
    }
}
