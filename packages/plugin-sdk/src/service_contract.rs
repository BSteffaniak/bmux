//! Invocation-bound interface compatibility, checked frame version 2.
//!
//! This is distinct from legacy/v1 negotiation. Compatibility evidence does not
//! authorize dispatch: providers still enforce authoritative permissions.

use crate::{PluginError, Result};

const MAGIC: &[u8] = b"BMUX-BPDL-CHECKED-2\0";
const RESPONSE_MAGIC: &[u8] = b"BMUX-BPDL-ACCEPTED-2\0";
/// SHA-256 interface identity size; schemas are never sent with invocations.
pub const MAX_SERVICE_CONTRACT_BYTES: usize = 32;
/// Bound the only variable-length metadata in a checked request.
pub const MAX_INTERFACE_ID_BYTES: usize = 1024;

/// Whether bytes select the checked v2 service representation.
#[must_use]
pub fn is_checked(payload: &[u8]) -> bool {
    payload.starts_with(MAGIC)
}

/// Bind a request to its generated interface and SHA-256 identity.
///
/// # Errors
/// Rejects empty/oversized interface IDs and non-SHA-256 evidence.
pub fn encode(interface_id: &str, contract: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
    if interface_id.is_empty() || interface_id.len() > MAX_INTERFACE_ID_BYTES {
        return Err(incompatible("invalid checked interface id"));
    }
    if contract.len() != MAX_SERVICE_CONTRACT_BYTES {
        return Err(incompatible("expected SHA-256 interface identity"));
    }
    let length = u16::try_from(interface_id.len())
        .map_err(|_| incompatible("interface id length overflow"))?;
    let mut out = Vec::with_capacity(MAGIC.len() + 2 + interface_id.len() + 32 + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(interface_id.as_bytes());
    out.extend_from_slice(contract);
    out.extend_from_slice(payload);
    Ok(out)
}

fn header(bytes: &[u8]) -> Result<(&str, &[u8])> {
    let bytes = bytes
        .strip_prefix(MAGIC)
        .ok_or_else(|| incompatible("checked v2 evidence missing"))?;
    let size = bytes
        .get(..2)
        .ok_or_else(|| incompatible("truncated checked frame"))?;
    let size = usize::from(u16::from_le_bytes([size[0], size[1]]));
    if size == 0 || size > MAX_INTERFACE_ID_BYTES {
        return Err(incompatible("invalid checked interface id length"));
    }
    let name = bytes
        .get(2..2 + size)
        .ok_or_else(|| incompatible("truncated interface id"))?;
    let name = std::str::from_utf8(name).map_err(|_| incompatible("non-UTF-8 interface id"))?;
    let remainder = bytes
        .get(2 + size..)
        .filter(|rest| rest.len() >= 32)
        .ok_or_else(|| incompatible("truncated SHA-256 identity"))?;
    Ok((name, remainder))
}

/// Identify the generated interface before looking up the actual provider.
/// This is untrusted routing metadata, not authorization or acceptance evidence.
///
/// # Errors
/// Rejects unsupported versions, invalid IDs and truncated identities.
pub fn interface_id(bytes: &[u8]) -> Result<&str> {
    header(bytes).map(|(name, _)| name)
}

/// Validate that checked metadata names the interface selected by authoritative routing.
///
/// # Errors
/// Rejects malformed frames and IDs that differ from the invocation context.
pub fn interface_id_for<'a>(expected_interface_id: &str, bytes: &'a [u8]) -> Result<&'a str> {
    let actual = interface_id(bytes)?;
    if actual != expected_interface_id {
        return Err(incompatible(
            "checked interface does not match routed interface",
        ));
    }
    Ok(actual)
}

/// Check both the routed interface and actual provider identity before dispatch.
///
/// # Errors
/// Rejects malformed frames, routed interface mismatch and contract mismatch.
pub fn decode_for<'a>(
    expected_interface_id: &str,
    expected: &[u8],
    bytes: &'a [u8],
) -> Result<&'a [u8]> {
    interface_id_for(expected_interface_id, bytes)?;
    decode(expected, bytes)
}

/// Validate against the actual provider's identity before decoding or dispatch.
///
/// The caller must obtain `expected` via `PluginContract::service_contract_for`
/// for this frame's interface ID and verify the routed interface agrees.
///
/// # Errors
/// Rejects unsupported frames, missing evidence and unequal SHA-256 identities.
pub fn decode<'a>(expected: &[u8], bytes: &'a [u8]) -> Result<&'a [u8]> {
    let (_, remainder) = header(bytes)?;
    if expected.len() != 32 || remainder[..32] != *expected {
        return Err(incompatible("BPDL interface contract mismatch"));
    }
    Ok(&remainder[32..])
}

/// Mark output only after this invocation's provider contract was accepted.
#[must_use]
pub fn encode_response(payload: &[u8]) -> Vec<u8> {
    let mut out = RESPONSE_MAGIC.to_vec();
    out.extend_from_slice(payload);
    out
}

/// Remove checked v2 provider acceptance evidence before response decoding.
///
/// # Errors
/// Rejects legacy/v1 responses and missing acceptance markers.
pub fn decode_response(payload: &[u8]) -> Result<&[u8]> {
    payload
        .strip_prefix(RESPONSE_MAGIC)
        .ok_or_else(|| incompatible("checked v2 provider acceptance missing"))
}

fn incompatible(details: &str) -> PluginError {
    PluginError::ServiceProtocol {
        details: format!("service compatibility: {details}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_is_compact_and_checked_before_payload_is_available() {
        let bytes = encode("state/v1", &[1; 32], b"request").unwrap();
        assert_eq!(interface_id_for("state/v1", &bytes).unwrap(), "state/v1");
        assert!(interface_id_for("other/v1", &bytes).is_err());
        assert!(decode_for("other/v1", &[1; 32], &bytes).is_err());
        assert_eq!(
            decode_for("state/v1", &[1; 32], &bytes).unwrap(),
            b"request"
        );
        assert_eq!(bytes.len(), MAGIC.len() + 2 + 8 + 32 + 7);
        assert_eq!(decode(&[1; 32], &bytes).unwrap(), b"request");
        assert!(decode(&[2; 32], &bytes).is_err());
        assert!(decode(&[], &bytes).is_err());
        for end in 0..bytes.len() - 7 {
            assert!(decode(&[1; 32], &bytes[..end]).is_err());
        }
        assert!(encode("state/v1", b"whole schema", b"request").is_err());
        assert!(encode("", &[1; 32], b"request").is_err());
        assert!(encode(&"x".repeat(MAX_INTERFACE_ID_BYTES + 1), &[1; 32], b"").is_err());
    }

    #[test]
    fn versions_are_not_guessed() {
        assert!(decode(&[1; 32], b"BMUX-BPDL-CHECKED-1\0old").is_err());
        assert!(decode_response(b"BMUX-BPDL-ACCEPTED-1\0old").is_err());
        assert_eq!(decode_response(&encode_response(b"ok")).unwrap(), b"ok");
    }
}
