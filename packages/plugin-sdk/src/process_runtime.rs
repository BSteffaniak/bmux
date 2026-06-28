use crate::{
    NativeCommandContext, NativeLifecycleContext, NativeServiceContext, PluginCancellationToken,
    PluginCommandOutcome, PluginError, PluginEvent, PluginInvocationId, Result, ServiceResponse,
    decode_service_message, encode_service_message,
};
use serde::{Deserialize, Serialize};

pub const PROCESS_RUNTIME_PROTOCOL_V1: u16 = 1;
pub const PROCESS_RUNTIME_MAGIC_V1: &[u8] = b"BMUXPRC1";
pub const PROCESS_RUNTIME_ENV_PROTOCOL: &str = "BMUX_PLUGIN_RUNTIME_PROTOCOL";
pub const PROCESS_RUNTIME_ENV_PLUGIN_ID: &str = "BMUX_PLUGIN_ID";
pub const PROCESS_RUNTIME_ENV_PERSISTENT_WORKER: &str = "BMUX_PLUGIN_RUNTIME_PERSISTENT_WORKER";
pub const PROCESS_RUNTIME_TRANSPORT_STDIO_V1: &str = "stdio-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProcessInvocationRequest {
    Command {
        protocol_version: u16,
        invocation_id: PluginInvocationId,
        plugin_id: String,
        command_name: String,
        arguments: Vec<String>,
        context: Option<NativeCommandContext>,
    },
    Lifecycle {
        protocol_version: u16,
        invocation_id: PluginInvocationId,
        plugin_id: String,
        symbol: String,
        context: NativeLifecycleContext,
    },
    Event {
        protocol_version: u16,
        invocation_id: PluginInvocationId,
        plugin_id: String,
        event: PluginEvent,
    },
    Service {
        protocol_version: u16,
        invocation_id: PluginInvocationId,
        cancellation: PluginCancellationToken,
        plugin_id: String,
        context: NativeServiceContext,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProcessInvocationResponse {
    Command {
        protocol_version: u16,
        invocation_id: PluginInvocationId,
        status: i32,
        outcome: Option<PluginCommandOutcome>,
    },
    Lifecycle {
        protocol_version: u16,
        invocation_id: PluginInvocationId,
        status: i32,
    },
    Event {
        protocol_version: u16,
        invocation_id: PluginInvocationId,
        status: Option<i32>,
    },
    Service {
        protocol_version: u16,
        invocation_id: PluginInvocationId,
        response: ServiceResponse,
    },
    Error {
        protocol_version: u16,
        invocation_id: Option<PluginInvocationId>,
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
    if bytes.len() > header_len + payload_len {
        return Err(PluginError::ServiceProtocol {
            details: "process runtime output has trailing bytes after payload".to_string(),
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
/// Returns an error when framing or encoding fails.
pub fn encode_process_invocation_response(response: &ProcessInvocationResponse) -> Result<Vec<u8>> {
    let payload = encode_service_message(response)?;
    encode_process_runtime_frame(&payload)
}

/// # Errors
///
/// Returns an error when framing or decoding fails.
pub fn decode_process_invocation_response(bytes: &[u8]) -> Result<ProcessInvocationResponse> {
    let payload = decode_process_runtime_frame(bytes)?;
    decode_service_message(payload).or_else(|current_error| {
        #[derive(Deserialize)]
        enum LegacyProcessInvocationResponse {
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

        let legacy: LegacyProcessInvocationResponse = decode_service_message(payload).map_err(|legacy_error| {
            PluginError::ServiceProtocol {
                details: format!(
                    "current process response decode failed: {current_error}; legacy process response decode failed: {legacy_error}",
                ),
            }
        })?;
        Ok(match legacy {
            LegacyProcessInvocationResponse::Command {
                protocol_version,
                status,
                outcome,
            } => ProcessInvocationResponse::Command {
                protocol_version,
                invocation_id: PluginInvocationId::new(),
                status,
                outcome,
            },
            LegacyProcessInvocationResponse::Lifecycle {
                protocol_version,
                status,
            } => ProcessInvocationResponse::Lifecycle {
                protocol_version,
                invocation_id: PluginInvocationId::new(),
                status,
            },
            LegacyProcessInvocationResponse::Event {
                protocol_version,
                status,
            } => ProcessInvocationResponse::Event {
                protocol_version,
                invocation_id: PluginInvocationId::new(),
                status,
            },
            LegacyProcessInvocationResponse::Service {
                protocol_version,
                response,
            } => ProcessInvocationResponse::Service {
                protocol_version,
                invocation_id: PluginInvocationId::new(),
                response,
            },
            LegacyProcessInvocationResponse::Error {
                protocol_version,
                details,
                status,
            } => ProcessInvocationResponse::Error {
                protocol_version,
                invocation_id: None,
                details,
                status,
            },
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ProcessInvocationRequest, ProcessInvocationResponse, decode_process_invocation_response,
        decode_process_runtime_frame, encode_process_runtime_frame,
    };

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

    #[test]
    fn process_frame_rejects_truncated_header() {
        let frame = b"BMUXPRC1\0\0";
        let error = decode_process_runtime_frame(frame).expect_err("truncated header must fail");
        assert!(error.to_string().contains("truncated frame header"));
    }

    #[test]
    fn process_frame_rejects_trailing_bytes() {
        let mut frame = encode_process_runtime_frame(b"ok").expect("frame should encode");
        frame.extend_from_slice(b"noise");
        let error = decode_process_runtime_frame(&frame).expect_err("trailing data must fail");
        assert!(error.to_string().contains("trailing bytes"));
    }

    #[test]
    fn process_invocation_response_rejects_non_protocol_payload() {
        let frame =
            encode_process_runtime_frame(b"not-bmux-codec").expect("frame encoding should succeed");
        let error =
            decode_process_invocation_response(&frame).expect_err("invalid payload must fail");
        assert!(error.to_string().contains("decode") || error.to_string().contains("invalid"));
    }

    #[test]
    fn process_invocation_request_service_carries_invocation_metadata() {
        let cancellation =
            crate::PluginCancellationToken::with_deadline(std::time::Duration::from_millis(5));
        let request = ProcessInvocationRequest::Service {
            protocol_version: 1,
            invocation_id: cancellation.invocation_id.clone(),
            cancellation: cancellation.clone(),
            plugin_id: "test.plugin".to_string(),
            context: crate::NativeServiceContext {
                plugin_id: "test.plugin".to_string(),
                request: crate::ServiceRequest {
                    caller_plugin_id: "caller.plugin".to_string(),
                    service: crate::RegisteredService {
                        capability: crate::HostScope::new("bmux.test")
                            .expect("capability should parse"),
                        kind: crate::ServiceKind::Query,
                        interface_id: "test/v1".to_string(),
                        provider: crate::ProviderId::Plugin("test.plugin".to_string()),
                    },
                    operation: "ping".to_string(),
                    payload: Vec::new(),
                },
                required_capabilities: Vec::new(),
                provided_capabilities: Vec::new(),
                services: Vec::new(),
                available_capabilities: Vec::new(),
                enabled_plugins: Vec::new(),
                plugin_search_roots: Vec::new(),
                host: crate::HostMetadata {
                    product_name: "bmux".to_string(),
                    product_version: "0.1.0".to_string(),
                    plugin_api_version: crate::ApiVersion::new(1, 0),
                    plugin_abi_version: crate::ApiVersion::new(1, 0),
                },
                connection: crate::HostConnectionInfo {
                    config_dir: String::new(),
                    config_dir_candidates: Vec::new(),
                    runtime_dir: String::new(),
                    data_dir: String::new(),
                    state_dir: String::new(),
                },
                settings: None,
                plugin_settings_map: std::collections::BTreeMap::new(),
                caller_client_id: None,
                cancellation: cancellation.clone(),
                host_kernel_bridge: None,
            },
        };
        let bytes = crate::encode_process_invocation_request(&request)
            .expect("process request should encode");
        let payload = decode_process_runtime_frame(&bytes).expect("frame should decode");
        let decoded: ProcessInvocationRequest =
            crate::decode_service_message(payload).expect("request should decode");
        let ProcessInvocationRequest::Service {
            invocation_id,
            cancellation: decoded_cancellation,
            context,
            ..
        } = decoded
        else {
            panic!("expected service request");
        };
        assert_eq!(invocation_id, cancellation.invocation_id);
        assert_eq!(decoded_cancellation, cancellation);
        assert_eq!(context.cancellation, cancellation);
    }

    #[test]
    fn process_invocation_response_decodes_valid_message() {
        let payload = crate::encode_service_message(&ProcessInvocationResponse::Event {
            protocol_version: 1,
            invocation_id: crate::PluginInvocationId::new(),
            status: Some(0),
        })
        .expect("encoding should succeed");
        let frame = encode_process_runtime_frame(&payload).expect("frame encoding should succeed");
        let response = decode_process_invocation_response(&frame)
            .expect("valid framed invocation response must decode");
        assert!(matches!(
            response,
            ProcessInvocationResponse::Event {
                protocol_version: 1,
                status: Some(0),
                ..
            }
        ));
    }
}
