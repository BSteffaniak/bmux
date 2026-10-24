use bmux_permissions_plugin_api::permissions_commands::client::{
    GrantResourceRequest, RevokeResourceRequest,
};

#[test]
fn resource_grant_contract_uses_stable_subject_and_explicit_scope() {
    let request = GrantResourceRequest {
        subject_id: uuid::Uuid::from_u128(1),
        session_id: None,
        role: "writer".into(),
    };
    assert!(request.session_id.is_none());
    let revoke = RevokeResourceRequest {
        subject_id: request.subject_id,
        session_id: Some(uuid::Uuid::from_u128(2)),
    };
    assert_eq!(revoke.subject_id, request.subject_id);
    assert!(revoke.session_id.is_some());
}
