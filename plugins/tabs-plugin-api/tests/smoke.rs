//! Smoke test: the BPDL-generated bindings compile and their types can
//! be constructed, serialized, and used via the trait signature.

use bmux_tabs_plugin_api::{
    tabs_events::{self, PaneEvent},
    tabs_state::{self, PaneState, PaneStatus},
};

#[test]
fn pane_state_record_round_trips() {
    let state = PaneState {
        id: uuid::Uuid::nil(),
        session_id: uuid::Uuid::nil(),
        focused: true,
        zoomed: false,
        name: Some("main".to_string()),
        status: PaneStatus::Running,
    };
    let json = serde_json::to_string(&state).expect("serialize");
    let round: PaneState = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(state, round);
}

#[test]
fn pane_status_variant_payload_serializes_case() {
    let exited = PaneStatus::Exited { exit_code: 42 };
    let json = serde_json::to_string(&exited).expect("serialize");
    assert!(json.contains("exited"));
    assert!(json.contains("42"));
}

#[test]
fn pane_event_variant_is_tagged() {
    // External-tagged enums serialize to `{"variant_name": <payload>}`
    // in JSON. Variant names use snake_case via
    // `#[serde(rename_all = "snake_case")]`.
    let ev = PaneEvent::Focused {
        pane_id: uuid::Uuid::nil(),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    assert!(json.contains("\"focused\""));
}

#[test]
fn interface_ids_match_bpdl_source() {
    assert_eq!(tabs_state::INTERFACE_ID, "tabs-state");
    assert_eq!(tabs_events::INTERFACE_ID, "tabs-events");
    assert_eq!(
        bmux_tabs_plugin_api::tabs_personal_views::INTERFACE_ID,
        "tabs-personal-views/v1"
    );
    assert_eq!(
        bmux_tabs_plugin_api::tabs_personal_commands::INTERFACE_ID,
        "tabs-personal-commands/v1"
    );
    assert_eq!(
        bmux_tabs_plugin_api::tabs_personal_attachment::INTERFACE_ID,
        "tabs-personal-attachment/v1"
    );
    assert_eq!(
        bmux_tabs_plugin_api::tabs_personal_actions::INTERFACE_ID,
        "tabs-personal-actions/v1"
    );
    assert_eq!(
        bmux_tabs_plugin_api::personal_view::STATE_KIND,
        "bmux.tabs/personal-view"
    );
}

#[test]
fn personal_attachment_contracts_take_no_client_and_round_trip() {
    use bmux_tabs_plugin_api::personal_view::{
        PersonalViewMember, PersonalViewProjection, PersonalViewWorkspace,
    };
    use bmux_tabs_plugin_api::tabs_personal_attachment::PersonalAttachmentError;
    use bmux_tabs_plugin_api::tabs_personal_attachment::client::{
        BindViewRequest, SelectMemberRequest,
    };
    // Requests name a view or member and a binding revision, never a client
    // or an owner: the host resolves the invoking attachment.
    let bind = serde_json::to_value(BindViewRequest {
        view_id: uuid::Uuid::from_u128(1),
        expected_binding_revision: 2,
    })
    .unwrap();
    let select = serde_json::to_value(SelectMemberRequest {
        member_id: uuid::Uuid::from_u128(3),
        expected_binding_revision: 2,
    })
    .unwrap();
    for request in [&bind, &select] {
        let fields = request.as_object().unwrap();
        assert!(
            fields
                .keys()
                .all(|key| !key.contains("client") && !key.contains("owner")),
            "{request}"
        );
    }
    let projection = PersonalViewProjection {
        view_id: uuid::Uuid::from_u128(1),
        name: "default".into(),
        view_revision: 4,
        binding_revision: 2,
        active_workspace_id: uuid::Uuid::nil(),
        workspaces: vec![PersonalViewWorkspace {
            workspace_id: uuid::Uuid::nil(),
            members: vec![PersonalViewMember {
                member_id: uuid::Uuid::from_u128(5),
                context_id: uuid::Uuid::from_u128(6),
                name: "mine".into(),
                active: true,
            }],
        }],
    };
    let round: PersonalViewProjection =
        serde_json::from_slice(&serde_json::to_vec(&projection).unwrap()).unwrap();
    assert_eq!(round, projection);
    assert!(
        serde_json::to_string(&PersonalAttachmentError::RevisionConflict {
            current_revision: 3
        })
        .unwrap()
        .contains("revision_conflict")
    );
}

#[test]
fn personal_view_contracts_take_no_owner_and_round_trip() {
    use bmux_tabs_plugin_api::tabs_personal_commands::client::{
        AddTabRequest, EnsureDefaultRequest,
    };
    use bmux_tabs_plugin_api::tabs_personal_views::{
        PersonalMember, PersonalView, PersonalViewError, PersonalWorkspace,
    };
    // Requests identify the mutation and the view, never an owner.
    let add = AddTabRequest {
        mutation_id: uuid::Uuid::from_u128(1),
        view_id: uuid::Uuid::from_u128(2),
        expected_revision: 3,
        context_id: uuid::Uuid::from_u128(4),
        before: None,
    };
    let json = serde_json::to_value(&add).unwrap();
    assert!(json.get("owner").is_none());
    let _ = EnsureDefaultRequest {
        mutation_id: uuid::Uuid::nil(),
    };
    let view = PersonalView {
        view_id: uuid::Uuid::from_u128(2),
        owner: uuid::Uuid::from_u128(9),
        name: "work".into(),
        schema_version: 1,
        revision: 3,
        is_default: true,
        workspaces: vec![PersonalWorkspace {
            workspace_id: uuid::Uuid::nil(),
            initialized: true,
            members: vec![PersonalMember {
                member_id: uuid::Uuid::from_u128(5),
                context_id: uuid::Uuid::from_u128(4),
                label: Some("mine".into()),
            }],
        }],
    };
    let round: PersonalView = serde_json::from_str(&serde_json::to_string(&view).unwrap()).unwrap();
    assert_eq!(round, view);
    let conflict = PersonalViewError::RevisionConflict {
        current_revision: 4,
    };
    assert!(
        serde_json::to_string(&conflict)
            .unwrap()
            .contains("revision_conflict")
    );
}

#[test]
fn event_kind_constant_is_namespaced_by_plugin_id() {
    // `plugin bmux.tabs` in the BPDL source means every interface's
    // event stream is namespaced under `bmux.tabs/<interface-name>`.
    assert_eq!(tabs_events::EVENT_KIND, "bmux.tabs/tabs-events");
}

#[test]
fn event_payload_alias_matches_declared_type() {
    // The generated `EventPayload` alias must resolve to the same
    // `PaneEvent` variant the BPDL declared. Constructing one via the
    // alias round-trips through JSON identically to the direct type.
    let via_alias: tabs_events::EventPayload = tabs_events::EventPayload::Focused {
        pane_id: uuid::Uuid::nil(),
    };
    let via_type: PaneEvent = PaneEvent::Focused {
        pane_id: uuid::Uuid::nil(),
    };
    assert_eq!(
        serde_json::to_string(&via_alias).unwrap(),
        serde_json::to_string(&via_type).unwrap()
    );
}

#[test]
fn personal_actions_address_tabs_without_client_owner_or_revision() {
    use bmux_tabs_plugin_api::tabs_personal_actions::client::{
        AddTabRequest, MoveTabRequest, OpenTabRequest, RemoveTabRequest,
    };
    let tab = uuid::Uuid::from_u128(1);
    let requests = [
        serde_json::to_value(AddTabRequest { context_id: tab }).unwrap(),
        serde_json::to_value(OpenTabRequest { context_id: tab }).unwrap(),
        serde_json::to_value(RemoveTabRequest { context_id: tab }).unwrap(),
        serde_json::to_value(MoveTabRequest {
            context_id: tab,
            before: Some(uuid::Uuid::from_u128(2)),
        })
        .unwrap(),
    ];
    // The tabs plugin resolves the attachment, its view, the members and the
    // revision itself.
    for request in &requests {
        assert!(
            request.as_object().unwrap().keys().all(|key| {
                !key.contains("client")
                    && !key.contains("owner")
                    && !key.contains("revision")
                    && !key.contains("mutation")
            }),
            "{request}"
        );
    }
}
