//! Authoritative access checks for execution-directory queries.
use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
use bmux_plugin_sdk::NativeServiceContext;
use bmux_session_models::ClientId;
use uuid::Uuid;

fn check_read_identity(
    permissions_enabled: bool,
    caller: Option<Uuid>,
) -> Result<Option<Uuid>, SpawnError> {
    if !permissions_enabled {
        return Ok(None);
    }
    caller.map(Some).ok_or_else(|| SpawnError::Denied {
        reason: "directory query requires caller identity when permissions are enabled".into(),
    })
}

pub fn authorize_read(ctx: &NativeServiceContext, session_id: Uuid) -> Result<(), SpawnError> {
    // Local single-user fallback is tied to plugin absence, never an error
    // substring or a failed policy request. Federated services must retain
    // their own authenticated authority checks before reaching local execution.
    let Some(client_id) = check_read_identity(
        ctx.enabled_plugins
            .iter()
            .any(|id| id == "bmux.permissions"),
        ctx.caller_client_id,
    )?
    else {
        return Ok(());
    };
    let denied = |reason: String| SpawnError::Denied { reason };
    let principal = bmux_plugin::global_plugin_state_registry()
        .get::<bmux_client_state::ClientPrincipalHandle>()
        .and_then(|entry| entry.read().ok().map(|guard| guard.clone()))
        .and_then(|handle| handle.0.get(ClientId(client_id)))
        .ok_or_else(|| denied("directory query principal is unavailable".into()))?;
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(ctx);
    let response = bmux_plugin::block_on_typed_dispatch(
        bmux_permissions_plugin_api::session_policy_state::client::check(
            &mut client,
            session_id,
            None,
            client_id,
            principal,
            "read".into(),
            None,
            None,
            None,
        ),
    )
    .map_err(|error| denied(format!("directory policy check failed: {error}")))?;
    if response.allowed {
        Ok(())
    } else {
        Err(denied(
            response
                .reason
                .unwrap_or_else(|| "directory access denied".into()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_fallback_only_applies_to_missing_permissions() {
        let caller = Uuid::new_v4();
        assert_eq!(check_read_identity(false, None), Ok(None));
        assert_eq!(check_read_identity(false, Some(caller)), Ok(None));
        assert_eq!(check_read_identity(true, Some(caller)), Ok(Some(caller)));
        assert!(matches!(
            check_read_identity(true, None),
            Err(SpawnError::Denied { .. })
        ));
    }
}
