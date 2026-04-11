use crate::{
    NativeCommandContext, NativeLifecycleContext, NativeServiceContext, PluginCommandOutcome,
    PluginError, PluginEvent, Result, ServiceResponse, decode_service_message,
    encode_service_message,
};
use serde::{Deserialize, Serialize};

pub const PROCESS_RUNTIME_PROTOCOL_V1: u16 = 1;
pub const PROCESS_RUNTIME_MAGIC_V1: &[u8] = b"BMUXPRC1";
pub const PROCESS_RUNTIME_ENV_PROTOCOL: &str = "BMUX_PLUGIN_RUNTIME_PROTOCOL";
pub const PROCESS_RUNTIME_ENV_PLUGIN_ID: &str = "BMUX_PLUGIN_ID";
pub const PROCESS_RUNTIME_TRANSPORT_STDIO_V1: &str = "stdio-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProcessInvocationRequest {
    Command {
        protocol_version: u16,
        plugin_id: String,
        command_name: String,
        arguments: Vec<String>,
        context: Option<NativeCommandContext>,
    },
    Lifecycle {
        protocol_version: u16,
        plugin_id: String,
        symbol: String,
        context: NativeLifecycleContext,
    },
    Event {
        protocol_version: u16,
        plugin_id: String,
        event: PluginEvent,
    },
    Service {
        protocol_version: u16,
        plugin_id: String,
        context: NativeServiceContext,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProcessInvocationResponse {
    Command {
        protocol_version: u16,
        status: i32,
        outcome: Option<PluginCommandOutcome>,
    },
    Lifecycle {
        protocol_version: u16,
        status: i32,
    },
    Event {
        protocol_version: u16,
        status: Option<i32>,
    },
    Service {
        protocol_version: u16,
        response: ServiceResponse,
    },
    Error {
        protocol_version: u16,
        details: String,
        status: Option<i32>,
    },
}

/// # Errors
///
/// Returns an error when the payload is larger than the frame format supports.
pub fn encode_process_runtime_frame(payload: &[u8]) -> Result<Vec<u8>> {
    let payload_len = u32::try_from(payload.len()).map_err(|_| PluginError::ServiceProtocol {
        details: "process runtime payload too large".to_string(),
    })?;
    let mut frame = Vec::with_capacity(PROCESS_RUNTIME_MAGIC_V1.len() + 4 + payload.len());
    frame.extend_from_slice(PROCESS_RUNTIME_MAGIC_V1);
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// # Errors
///
/// Returns an error when the input bytes are not a complete process-runtime frame.
pub fn decode_process_runtime_frame(bytes: &[u8]) -> Result<&[u8]> {
    if !bytes.starts_with(PROCESS_RUNTIME_MAGIC_V1) {
        return Err(PluginError::ServiceProtocol {
            details: "process runtime output missing BMUXPRC1 frame prefix".to_string(),
        });
    }
    let header_len = PROCESS_RUNTIME_MAGIC_V1.len() + 4;
    if bytes.len() < header_len {
        return Err(PluginError::ServiceProtocol {
            details: "process runtime output truncated frame header".to_string(),
        });
    }
    let mut len_buf = [0_u8; 4];
    len_buf.copy_from_slice(&bytes[PROCESS_RUNTIME_MAGIC_V1.len()..header_len]);
    let payload_len =
        usize::try_from(u32::from_be_bytes(len_buf)).map_err(|_| PluginError::ServiceProtocol {
            details: "process runtime payload length conversion failed".to_string(),
        })?;
    if bytes.len() < header_len + payload_len {
        return Err(PluginError::ServiceProtocol {
            details: "process runtime output truncated payload".to_string(),
        });
    }
    Ok(&bytes[header_len..header_len + payload_len])
}

/// # Errors
///
/// Returns an error when framing or encoding fails.
pub fn encode_process_invocation_request(request: &ProcessInvocationRequest) -> Result<Vec<u8>> {
    let payload = encode_service_message(request)?;
    encode_process_runtime_frame(&payload)
}

/// # Errors
///
/// Returns an error when framing or decoding fails.
pub fn decode_process_invocation_response(bytes: &[u8]) -> Result<ProcessInvocationResponse> {
    let payload = decode_process_runtime_frame(bytes)?;
    decode_service_message(payload)
}

#[cfg(test)]
mod tests {
    use super::{decode_process_runtime_frame, encode_process_runtime_frame};

    #[test]
    fn process_frame_round_trips_payload() {
        let payload = b"hello";
        let frame = encode_process_runtime_frame(payload).expect("frame should encode");
        let decoded = decode_process_runtime_frame(&frame).expect("frame should decode");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn process_frame_rejects_invalid_magic() {
        let frame = b"INVALID\0\0\0\x05hello";
        let error = decode_process_runtime_frame(frame).expect_err("invalid magic must fail");
        assert!(error.to_string().contains("BMUXPRC1"));
    }

    #[test]
    fn process_frame_rejects_truncated_payload() {
        let mut frame = b"BMUXPRC1".to_vec();
        frame.extend_from_slice(&5_u32.to_be_bytes());
        frame.extend_from_slice(b"hey");
        let error = decode_process_runtime_frame(&frame).expect_err("truncated payload must fail");
        assert!(error.to_string().contains("truncated payload"));
    }
}
