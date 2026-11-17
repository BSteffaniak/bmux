//! Wire encoding of asynchronous host service calls over plug-rt.
//!
//! A dynamic plugin's [`crate::PluginHostServices::call_service_async`]
//! starts the call as a host task through the plug-rt table
//! (`bmux_plugin_runtime::rt::services`) and awaits it with a real waker. The
//! request and outcome cross as these bytes; the invocation port the call is
//! made through supplies its authority on the host.
//!
//! Request: `kind u8`, then `capability`, `interface` and `operation` each as
//! a little-endian `u32` length plus UTF-8 bytes, then the payload to the
//! end. Outcome: a tag byte, then the success payload or a UTF-8 reason.
use crate::{HostServiceOwnedRequest, PluginError, ServiceKind};

const OUTCOME_OK: u8 = 0;
const OUTCOME_CANCELLED: u8 = 1;
const OUTCOME_REJECTED: u8 = 2;
const OUTCOME_FAILED: u8 = 3;

fn malformed(what: &str) -> PluginError {
    PluginError::ServiceProtocol {
        details: format!("malformed asynchronous host {what}"),
    }
}

fn put(out: &mut Vec<u8>, field: &str) -> crate::Result<()> {
    let length = u32::try_from(field.len()).map_err(|_| malformed("request field"))?;
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(field.as_bytes());
    Ok(())
}

fn take<'a>(bytes: &mut &'a [u8]) -> crate::Result<&'a str> {
    if bytes.len() < 4 {
        return Err(malformed("request"));
    }
    let (length, rest) = bytes.split_at(4);
    let length = u32::from_le_bytes([length[0], length[1], length[2], length[3]]) as usize;
    if rest.len() < length {
        return Err(malformed("request"));
    }
    let (field, rest) = rest.split_at(length);
    *bytes = rest;
    std::str::from_utf8(field).map_err(|_| malformed("request field"))
}

/// # Errors
/// A field exceeds the length prefix.
pub fn encode_request(request: &HostServiceOwnedRequest) -> crate::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(
        13 + request.capability.len()
            + request.interface.len()
            + request.operation.len()
            + request.payload.len(),
    );
    out.push(match request.kind {
        ServiceKind::Query => 0,
        ServiceKind::Command => 1,
        ServiceKind::Event => 2,
    });
    put(&mut out, &request.capability)?;
    put(&mut out, &request.interface)?;
    put(&mut out, &request.operation)?;
    out.extend_from_slice(&request.payload);
    Ok(out)
}

/// # Errors
/// The bytes are not an encoded request.
pub fn decode_request(bytes: &[u8]) -> crate::Result<HostServiceOwnedRequest> {
    let (&kind, mut rest) = bytes.split_first().ok_or_else(|| malformed("request"))?;
    let kind = match kind {
        0 => ServiceKind::Query,
        1 => ServiceKind::Command,
        2 => ServiceKind::Event,
        _ => return Err(malformed("request kind")),
    };
    let capability = take(&mut rest)?.to_owned();
    let interface = take(&mut rest)?.to_owned();
    let operation = take(&mut rest)?.to_owned();
    Ok(HostServiceOwnedRequest {
        capability,
        kind,
        interface,
        operation,
        payload: rest.to_vec(),
    })
}

/// Encode a host outcome. Pre-dispatch refusals keep their type so the
/// plugin can tell "nothing happened" from a failed call.
#[must_use]
pub fn encode_outcome(outcome: &crate::Result<Vec<u8>>) -> Vec<u8> {
    let (tag, body): (u8, Vec<u8>) = match outcome {
        Ok(payload) => (OUTCOME_OK, payload.clone()),
        Err(PluginError::InvocationCancelled) => (OUTCOME_CANCELLED, Vec::new()),
        Err(PluginError::RejectedBeforeDispatch { details }) => {
            (OUTCOME_REJECTED, details.clone().into_bytes())
        }
        Err(error) => (OUTCOME_FAILED, error.to_string().into_bytes()),
    };
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(tag);
    out.extend_from_slice(&body);
    out
}

/// # Errors
/// The host reported a refusal or failure, or the bytes are malformed.
pub fn decode_outcome(bytes: &[u8]) -> crate::Result<Vec<u8>> {
    let (&tag, body) = bytes.split_first().ok_or_else(|| malformed("outcome"))?;
    let reason = || String::from_utf8_lossy(body).into_owned();
    match tag {
        OUTCOME_OK => Ok(body.to_vec()),
        OUTCOME_CANCELLED => Err(PluginError::InvocationCancelled),
        OUTCOME_REJECTED => Err(PluginError::RejectedBeforeDispatch { details: reason() }),
        OUTCOME_FAILED => Err(PluginError::ServiceProtocol { details: reason() }),
        _ => Err(malformed("outcome tag")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_outcomes_round_trip_and_keep_refusal_types() {
        let request = HostServiceOwnedRequest {
            capability: "example.read".into(),
            kind: ServiceKind::Command,
            interface: "counter-commands/v1".into(),
            operation: "incr".into(),
            payload: vec![1, 2, 3],
        };
        let decoded = decode_request(&encode_request(&request).unwrap()).unwrap();
        assert_eq!(decoded.capability, request.capability);
        assert_eq!(decoded.interface, request.interface);
        assert_eq!(decoded.operation, request.operation);
        assert_eq!(decoded.payload, request.payload);
        assert!(matches!(decoded.kind, ServiceKind::Command));
        assert_eq!(
            decode_outcome(&encode_outcome(&Ok(vec![9]))).unwrap(),
            vec![9]
        );
        assert!(matches!(
            decode_outcome(&encode_outcome(&Err(PluginError::InvocationCancelled))),
            Err(PluginError::InvocationCancelled)
        ));
        assert!(matches!(
            decode_outcome(&encode_outcome(&Err(PluginError::RejectedBeforeDispatch {
                details: "no route".into()
            }))),
            Err(PluginError::RejectedBeforeDispatch { details }) if details == "no route"
        ));
        assert!(decode_request(&[0, 9, 0]).is_err());
    }
}
