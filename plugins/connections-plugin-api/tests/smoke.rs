use bmux_connections_plugin_api::{
    connection_types::{ConnectionError, ConnectionTransport, InvocationOptions, ResolvedEndpoint},
    connections_commands, connections_state,
};

#[test]
fn invitation_pairing_contracts_are_generated_and_separate() {
    use bmux_connections_plugin_api::{
        pairing_peer_commands, pairing_session_commands, pairing_session_state,
        pairing_types::{PairingRequest, SessionPhase},
    };
    let request = pairing_peer_commands::client::SubmitPairingRequest {
        device_name: None,
        session_id: "session".into(),
        nonce: vec![1; 32],
        signature: vec![2; 64],
    };
    let json = serde_json::to_string(&request).unwrap();
    assert!(!json.contains("secret"));
    assert_ne!(
        pairing_peer_commands::INTERFACE_ID.as_str(),
        pairing_session_commands::INTERFACE_ID.as_str()
    );
    assert_ne!(
        pairing_peer_commands::INTERFACE_ID.as_str(),
        pairing_session_state::INTERFACE_ID.as_str()
    );
    let pending = PairingRequest {
        device_name: None,
        session_id: "session".into(),
        phase: SessionPhase::PendingGrant,
        peer_key: None,
        verification_phrase: None,
        device_id: None,
        expires_at: 123,
    };
    assert_eq!(
        serde_json::from_str::<PairingRequest>(&serde_json::to_string(&pending).unwrap()).unwrap(),
        pending
    );
}

#[test]
fn endpoint_round_trips() {
    let endpoint = ResolvedEndpoint {
        endpoint_id: "prod".to_string(),
        reference: "prod".to_string(),
        label: "prod".to_string(),
        transport: ConnectionTransport::Ssh,
        address: "ops@example.com:22".to_string(),
        server_name: None,
        connect_timeout_ms: 8_000,
    };
    let json = serde_json::to_string(&endpoint).expect("serialize endpoint");
    assert_eq!(
        serde_json::from_str::<ResolvedEndpoint>(&json).expect("deserialize endpoint"),
        endpoint
    );
}

#[test]
fn errors_are_structured() {
    let error = ConnectionError::TargetNotFound {
        target: "missing".to_string(),
    };
    let json = serde_json::to_string(&error).expect("serialize error");
    assert!(json.contains("target_not_found"));
}

#[test]
fn invocation_policy_and_errors_round_trip() {
    let options = InvocationOptions {
        timeout_ms: 5_000,
        max_attempts: 3,
        retry_backoff_ms: 25,
    };
    let json = serde_json::to_string(&options).expect("serialize invocation options");
    assert_eq!(
        serde_json::from_str::<InvocationOptions>(&json).expect("deserialize invocation options"),
        options
    );

    for error in [
        ConnectionError::Cancelled {
            target: "prod".to_string(),
        },
        ConnectionError::TimedOut {
            target: "prod".to_string(),
            phase: "acquire".to_string(),
            timeout_ms: 5_000,
        },
        ConnectionError::RetryExhausted {
            target: "prod".to_string(),
            attempts: 3,
            reason: "offline".to_string(),
        },
    ] {
        let json = serde_json::to_string(&error).expect("serialize connection error");
        assert_eq!(
            serde_json::from_str::<ConnectionError>(&json).expect("deserialize connection error"),
            error
        );
    }
}

#[test]
fn interface_ids_are_stable() {
    assert_eq!(connections_state::INTERFACE_ID, "connections-state");
    assert_eq!(connections_commands::INTERFACE_ID, "connections-commands");
    assert_eq!(connections_state::OP_RESOLVE, "resolve");
    assert_eq!(connections_commands::OP_INVOKE_SERVICE, "invoke-service");
}

#[test]
fn contract_declares_both_services() {
    assert_eq!(
        bmux_connections_plugin_api::service_declarations()
            .expect("valid declarations")
            .len(),
        9
    );
}

#[test]
fn pairing_contract_is_generated_and_explicit() {
    use bmux_connections_plugin_api::{pairing_commands, pairing_state, pairing_types};
    let request = pairing_commands::client::CreateInvitationRequest {
        device_key: "device-public-key".into(),
        ttl_seconds: 300,
        access: pairing_types::DeviceAccess::FullLocalRuntime,
    };
    let json = serde_json::to_string(&request).expect("serialize generated request");
    let decoded: pairing_commands::client::CreateInvitationRequest =
        serde_json::from_str(&json).expect("deserialize generated request");
    assert_eq!(
        decoded.access,
        pairing_types::DeviceAccess::FullLocalRuntime
    );
    assert_eq!(decoded.ttl_seconds, 300);
    assert_eq!(pairing_commands::INTERFACE_ID, "pairing-commands");
    assert_eq!(pairing_commands::OP_REDEEM_INVITATION, "redeem-invitation");
    assert_eq!(pairing_commands::OP_FINISH_ADMISSION, "finish-admission");
    assert_eq!(pairing_state::OP_LIST_DEVICES, "list-devices");
    assert_eq!(pairing_state::OP_CHECK_DEVICE_ACCESS, "check-device-access");
    assert_eq!(
        bmux_connections_plugin_api::admission_provider_state::INTERFACE_ID,
        "admission-provider-state"
    );
    assert_eq!(
        bmux_connections_plugin_api::admission_provider_commands::INTERFACE_ID,
        "admission-provider-commands"
    );
    assert_eq!(
        bmux_connections_plugin_api::admission_provider_commands::OP_CONSUME,
        "consume"
    );
    let check = pairing_types::DeviceAccessCheck {
        access: pairing_types::DeviceAccess::Denied,
        revision: 9,
    };
    let json = serde_json::to_string(&check).expect("serialize access check");
    assert_eq!(
        serde_json::from_str::<pairing_types::DeviceAccessCheck>(&json).unwrap(),
        check
    );
    let device = pairing_types::Device {
        device_id: "device-id".into(),
        device_key: request.device_key,
        access: pairing_types::DeviceAccess::Denied,
        revision: 2,
    };
    let json = serde_json::to_string(&device).expect("serialize device");
    assert_eq!(
        serde_json::from_str::<pairing_types::Device>(&json).unwrap(),
        device
    );
}

#[test]
fn generated_secret_debug_redacts_without_changing_serialization() {
    use bmux_connections_plugin_api::{
        admission_provider_commands, pairing_commands, pairing_peer_commands,
        pairing_types::{DeviceAccess, Invitation, PairingSession},
    };
    const SENTINEL: &str = "REUSABLE-CREDENTIAL-SENTINEL";
    fn check<T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug>(value: &T) {
        let debug = format!("{value:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(SENTINEL));
        assert!(!debug.contains("82, 69, 85, 83"));
        let encoded = bmux_codec::to_typed_vec(value).unwrap();
        let decoded: T = bmux_codec::from_typed_bytes(&encoded).unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::to_value(&decoded).unwrap()
        );
        let serialized = serde_json::to_string(value).unwrap();
        // Bytes serialize as integer arrays; strings retain their original contents.
        assert!(serialized.contains(SENTINEL) || serialized.contains("82,69,85,83"));
    }
    let invitation = Invitation {
        invitation_id: "invitation".into(),
        endpoint_id: "endpoint".into(),
        device_key: "device".into(),
        secret: SENTINEL.as_bytes().to_vec(),
        expires_at: 123,
        access: DeviceAccess::Denied,
        possession_message: SENTINEL.as_bytes().to_vec(),
    };
    check(&invitation);
    let decoded: Invitation =
        serde_json::from_str(&serde_json::to_string(&invitation).unwrap()).unwrap();
    assert_eq!(decoded, invitation);
    assert_eq!(decoded.secret, SENTINEL.as_bytes());
    assert_eq!(decoded.possession_message, SENTINEL.as_bytes());
    check(&PairingSession {
        session_id: "session".into(),
        invitation_link: SENTINEL.into(),
        expires_at: 123,
    });
    check(&pairing_commands::client::PairEndpointRequest {
        target: "host".into(),
        invitation,
    });
    check(&pairing_commands::client::RedeemInvitationRequest {
        invitation_id: "id".into(),
        secret: SENTINEL.as_bytes().to_vec(),
        signature: SENTINEL.as_bytes().to_vec(),
    });
    check(&admission_provider_commands::client::ConsumeRequest {
        ticket: SENTINEL.as_bytes().to_vec(),
    });
    check(&pairing_peer_commands::client::SubmitPairingRequest {
        session_id: "session".into(),
        nonce: SENTINEL.as_bytes().to_vec(),
        signature: SENTINEL.as_bytes().to_vec(),
        device_name: None,
    });
}

#[test]
fn secret_metadata_keeps_existing_typed_wire_layout() {
    use bmux_connections_plugin_api::pairing_types::{DeviceAccess, Invitation, PairingSession};
    // Mirrors the pre-annotation serde layout, without any diagnostic metadata.
    #[derive(serde::Serialize)]
    struct PlainInvitation {
        invitation_id: String,
        endpoint_id: String,
        device_key: String,
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        secret: Vec<u8>,
        expires_at: u64,
        access: DeviceAccess,
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        possession_message: Vec<u8>,
    }
    let plain = PlainInvitation {
        invitation_id: "id".into(),
        endpoint_id: "host".into(),
        device_key: "device".into(),
        secret: vec![1, 2, 3],
        expires_at: 42,
        access: DeviceAccess::Denied,
        possession_message: vec![4, 5, 6],
    };
    let generated: Invitation =
        serde_json::from_value(serde_json::to_value(&plain).unwrap()).unwrap();
    assert_eq!(
        bmux_codec::to_typed_vec(&plain).unwrap(),
        bmux_codec::to_typed_vec(&generated).unwrap()
    );
    #[derive(serde::Serialize)]
    struct PlainSession {
        session_id: String,
        invitation_link: String,
        expires_at: u64,
    }
    let plain = PlainSession {
        session_id: "id".into(),
        invitation_link: "secret link".into(),
        expires_at: 42,
    };
    let generated: PairingSession =
        serde_json::from_value(serde_json::to_value(&plain).unwrap()).unwrap();
    assert_eq!(
        bmux_codec::to_typed_vec(&plain).unwrap(),
        bmux_codec::to_typed_vec(&generated).unwrap()
    );
}
