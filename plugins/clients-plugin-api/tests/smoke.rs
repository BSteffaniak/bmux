//! Smoke test: the BPDL-generated clients-plugin-api bindings compile
//! and their types can be constructed, serialized, and the declared
//! constants match the schema.

use bmux_clients_plugin_api::{
    clients_commands::{ClientAck, SetCurrentSessionError, SetFollowingError},
    clients_events::{self, ClientEvent},
    clients_state::{self, ClientQueryError, ClientSummary},
};

#[test]
fn client_summary_round_trips() {
    let summary = ClientSummary {
        id: uuid::Uuid::nil(),
        selected_session_id: Some(uuid::Uuid::nil()),
        selected_context_id: None,
        following_client_id: None,
        following_global: true,
    };
    let json = serde_json::to_string(&summary).expect("serialize");
    let round: ClientSummary = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(summary, round);
}

#[test]
fn client_query_error_variants_serialize() {
    let err = ClientQueryError::NoCurrentClient;
    let json = serde_json::to_string(&err).expect("serialize");
    assert!(json.contains("no_current_client"));
}

#[test]
fn command_ack_and_error_variants_serialize() {
    let ack = ClientAck {
        client_id: uuid::Uuid::nil(),
    };
    assert!(
        serde_json::to_string(&ack)
            .expect("ack")
            .contains("client_id")
    );

    let err = SetCurrentSessionError::NotFound;
    assert!(
        serde_json::to_string(&err)
            .expect("err")
            .contains("not_found")
    );

    let follow_err = SetFollowingError::Denied {
        reason: "no".to_string(),
    };
    assert!(
        serde_json::to_string(&follow_err)
            .expect("follow err")
            .contains("denied")
    );
}

#[test]
fn client_event_variants_are_tagged() {
    let ev = ClientEvent::Attached {
        client_id: uuid::Uuid::nil(),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    assert!(json.contains("\"attached\""));
}

#[test]
fn current_stream_provenance_contract_has_no_caller_selected_identity() {
    use bmux_clients_plugin_api::clients_identity::{self, StreamProvenance};
    assert_eq!(clients_identity::INTERFACE_ID, "clients-identity/v1");
    let admitted = StreamProvenance::Admitted {
        subject_id: uuid::Uuid::from_u128(1),
        lease_id: uuid::Uuid::from_u128(2),
    };
    for value in [StreamProvenance::Local, admitted, StreamProvenance::Invalid] {
        let bytes = serde_json::to_vec(&value).unwrap();
        let decoded: StreamProvenance = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, value);
    }
    let source = include_str!("../bpdl/clients-plugin.bpdl");
    assert!(source.contains("query current-stream-provenance()"));
}

#[test]
fn interface_ids_match_bpdl_source() {
    assert_eq!(clients_state::INTERFACE_ID, "clients-state");
    assert_eq!(clients_events::INTERFACE_ID, "clients-events");
}

#[test]
fn event_kind_is_namespaced_by_plugin_id() {
    assert_eq!(clients_events::EVENT_KIND, "bmux.clients/clients-events");
}

#[test]
fn event_payload_alias_matches_declared_type() {
    let via_alias: clients_events::EventPayload = clients_events::EventPayload::Detached {
        client_id: uuid::Uuid::nil(),
    };
    let via_type: ClientEvent = ClientEvent::Detached {
        client_id: uuid::Uuid::nil(),
    };
    assert_eq!(
        serde_json::to_string(&via_alias).unwrap(),
        serde_json::to_string(&via_type).unwrap()
    );
}
