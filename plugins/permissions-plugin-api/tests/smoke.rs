use bmux_permissions_plugin_api::remote_role_commands::client::SetRemoteRoleRequest;
use bmux_permissions_plugin_api::remote_role_types::RemoteRole;

#[test]
fn remote_role_contract_names_a_stable_principal_and_role() {
    let request = SetRemoteRoleRequest {
        principal_id: uuid::Uuid::from_u128(1),
        role: RemoteRole::ViewOnly,
    };
    assert_eq!(request.principal_id, uuid::Uuid::from_u128(1));
    assert_ne!(request.role, RemoteRole::Full);
}

#[test]
fn role_administration_is_owner_local_and_policy_checks_are_observe() {
    use bmux_permissions_plugin_api::{
        permissions_commands, permissions_state, remote_role_commands, remote_role_state,
        session_policy_commands, session_policy_state,
    };
    use bmux_plugin_sdk::RouteAccess;
    assert_eq!(remote_role_commands::ACCESS, Some(RouteAccess::OwnerLocal));
    assert_eq!(remote_role_state::ACCESS, Some(RouteAccess::OwnerLocal));
    assert_eq!(permissions_commands::ACCESS, Some(RouteAccess::OwnerLocal));
    assert_eq!(
        session_policy_commands::ACCESS,
        Some(RouteAccess::OwnerLocal)
    );
    assert_eq!(session_policy_state::ACCESS, Some(RouteAccess::Observe));
    assert_eq!(permissions_state::ACCESS, Some(RouteAccess::Observe));
}
