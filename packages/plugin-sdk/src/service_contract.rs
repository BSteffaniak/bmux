//! Invocation-bound exact BPDL compatibility.
//!
//! Checked frames are explicitly
//! negotiated by the transport; legacy calls keep their existing representation.
//! Provider validation precedes deserialization/dispatch and response evidence
//! prevents treating a legacy provider's output as a checked response.

use crate::{PluginError, Result};

const MAGIC: &[u8] = b"BMUX-BPDL-CHECKED-1\0";
/// Maximum exact contract size accepted on one invocation.
pub const MAX_SERVICE_CONTRACT_BYTES: usize = 1024 * 1024;

/// Whether bytes select the checked service representation.
#[must_use]
pub fn is_checked(payload: &[u8]) -> bool {
    payload.starts_with(MAGIC)
}

/// Bind encoded payload bytes to the generated exact BPDL contract.
///
/// # Errors
/// Rejects missing or oversized generated evidence.
pub fn encode(contract: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
    if contract.is_empty() || contract.len() > MAX_SERVICE_CONTRACT_BYTES {
        return Err(incompatible("missing or oversized generated BPDL contract"));
    }
    let mut out = Vec::with_capacity(MAGIC.len() + 4 + contract.len() + payload.len());
    out.extend_from_slice(MAGIC);
    let length =
        u32::try_from(contract.len()).map_err(|_| incompatible("contract length overflow"))?;
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(contract);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Validate checked bytes against the actual provider's generated contract.
///
/// Validation precedes invocation. Also used by the consumer to validate checked response evidence.
///
/// # Errors
/// Rejects unmarked, truncated, unknown, missing or unequal evidence.
pub fn decode<'a>(expected: &[u8], bytes: &'a [u8]) -> Result<&'a [u8]> {
    if expected.is_empty() || expected.len() > MAX_SERVICE_CONTRACT_BYTES {
        return Err(incompatible(
            "provider/consumer has no supported generated contract",
        ));
    }
    let bytes = bytes
        .strip_prefix(MAGIC)
        .ok_or_else(|| incompatible("checked response/request evidence missing"))?;
    let size_bytes: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| incompatible("truncated checked frame"))?
        .try_into()
        .map_err(|_| incompatible("invalid contract length"))?;
    let size = usize::try_from(u32::from_le_bytes(size_bytes))
        .map_err(|_| incompatible("contract length overflow"))?;
    if size == 0 || size > MAX_SERVICE_CONTRACT_BYTES {
        return Err(incompatible("invalid checked contract size"));
    }
    let contract = bytes
        .get(4..4 + size)
        .ok_or_else(|| incompatible("truncated contract evidence"))?;
    if contract != expected {
        return Err(incompatible("BPDL exact contract mismatch"));
    }
    Ok(&bytes[4 + size..])
}

const RESPONSE_MAGIC: &[u8] = b"BMUX-BPDL-ACCEPTED-1\0";

/// Mark output only after the provider has accepted this invocation's contract.
///
/// The full contract is not echoed: doing so could force output-buffer retries
/// (and repeated command execution) solely because of schema size.
#[must_use]
pub fn encode_response(payload: &[u8]) -> Vec<u8> {
    let mut out = RESPONSE_MAGIC.to_vec();
    out.extend_from_slice(payload);
    out
}

/// Remove the provider's checked acceptance marker before typed response decoding.
///
/// # Errors
/// Rejects legacy responses without checked-provider evidence.
pub fn decode_response(payload: &[u8]) -> Result<&[u8]> {
    payload
        .strip_prefix(RESPONSE_MAGIC)
        .ok_or_else(|| incompatible("checked provider acceptance missing"))
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
    fn evidence_is_checked_before_payload_is_available() {
        let bytes = encode(b"provider-v1", b"request").unwrap();
        assert_eq!(decode(b"provider-v1", &bytes).unwrap(), b"request");
        assert!(decode(b"provider-v2", &bytes).is_err());
        assert!(decode(b"provider-v1", b"legacy response").is_err());
        assert!(decode(b"provider-v1", &bytes[..MAGIC.len() + 3]).is_err());
        assert!(encode(&[], b"request").is_err());
    }
}
