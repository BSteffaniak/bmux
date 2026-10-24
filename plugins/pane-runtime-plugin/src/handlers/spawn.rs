//! Authoritative access checks for execution-directory queries.
use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
use bmux_plugin_sdk::NativeServiceContext;
use uuid::Uuid;

pub(super) fn home_directory() -> Result<String, SpawnError> {
    validate_home_directory(std::env::home_dir())
}

fn validate_home_directory(home: Option<std::path::PathBuf>) -> Result<String, SpawnError> {
    let home = home.ok_or(SpawnError::SourceUnavailable)?;
    let path = home.to_str().ok_or_else(|| SpawnError::InvalidDirectory {
        path: home.to_string_lossy().into_owned(),
        reason: "execution home directory is not UTF-8".into(),
    })?;
    if !home.is_absolute() || !home.is_dir() {
        return Err(SpawnError::InvalidDirectory {
            path: path.into(),
            reason: "execution home must be an available absolute directory".into(),
        });
    }
    Ok(path.into())
}

fn check_read_identity(
    permissions_enabled: bool,
    caller: Option<Uuid>,
    provenance: &bmux_plugin::StreamOrigin,
) -> Result<Option<Uuid>, SpawnError> {
    use bmux_plugin::StreamOrigin as StreamProvenance;
    if matches!(
        provenance,
        StreamProvenance::Invalid | StreamProvenance::HostInternal
    ) {
        return Err(SpawnError::Denied {
            reason: "trusted caller provenance unavailable or revoked".into(),
        });
    }
    if !permissions_enabled {
        return if matches!(provenance, StreamProvenance::Local) {
            Ok(None)
        } else {
            Err(SpawnError::Denied {
                reason: "remote execution requires resource authorization".into(),
            })
        };
    }
    caller.map(Some).ok_or_else(|| SpawnError::Denied {
        reason: "directory query requires caller identity when permissions are enabled".into(),
    })
}

pub fn authorize_read(ctx: &NativeServiceContext, session_id: Uuid) -> Result<(), SpawnError> {
    authorize_session(ctx, session_id, "read")
}

pub(super) fn authorize_creation(
    ctx: &NativeServiceContext,
    session_id: Uuid,
    action: &str,
) -> Result<(), SpawnError> {
    if ctx.cancellation.is_cancelled() {
        return Err(SpawnError::LaunchFailed {
            reason: "creation request cancelled".into(),
        });
    }
    authorize_session(ctx, session_id, action)
}

pub(super) fn authorize_session(
    ctx: &NativeServiceContext,
    session_id: Uuid,
    action: &str,
) -> Result<(), SpawnError> {
    use bmux_plugin::StreamOrigin as StreamProvenance;
    // Never infer local from absent admission, missing plugins, or wire identity.
    let provenance = caller_provenance(ctx)?;
    let permissions_enabled = ctx
        .enabled_plugins
        .iter()
        .any(|id| id == "bmux.permissions");
    let Some(client_id) =
        check_read_identity(permissions_enabled, ctx.caller_client_id, &provenance)?
    else {
        return Ok(());
    };
    let principal = match provenance {
        StreamProvenance::Admitted { subject_id, .. } => subject_id,
        // The permissions service derives local identity from its own host
        // context; this required legacy request field grants no authority.
        StreamProvenance::Local => client_id,
        StreamProvenance::Invalid | StreamProvenance::HostInternal => {
            return Err(SpawnError::Denied {
                reason: "trusted caller provenance invalid".into(),
            });
        }
    };
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(ctx);
    let response = bmux_plugin::block_on_typed_dispatch(
        bmux_permissions_plugin_api::session_policy_state::client::check(
            &mut client,
            session_id,
            None,
            client_id,
            principal,
            action.into(),
            None,
            None,
            None,
        ),
    )
    .map_err(|error| format!("directory policy check failed: {error}"));
    enforce_read_policy(response.map(|response| (response.allowed, response.reason)))
}

// No process-local global: the foundational service queries the actual host.
pub(super) fn caller_provenance(
    ctx: &NativeServiceContext,
) -> Result<bmux_plugin::StreamOrigin, SpawnError> {
    bmux_plugin::ServiceCaller::current_stream_origin(ctx).map_err(|error| SpawnError::Denied {
        reason: format!("trusted provenance unavailable: {error}"),
    })
}

fn enforce_read_policy(response: Result<(bool, Option<String>), String>) -> Result<(), SpawnError> {
    let (allowed, reason) = response.map_err(|reason| SpawnError::Denied { reason })?;
    if allowed {
        Ok(())
    } else {
        Err(SpawnError::Denied {
            reason: reason.unwrap_or_else(|| "directory access denied".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_home_requires_an_available_absolute_utf8_directory() {
        assert_eq!(
            validate_home_directory(None),
            Err(SpawnError::SourceUnavailable)
        );
        assert!(matches!(
            validate_home_directory(Some("relative-home".into())),
            Err(SpawnError::InvalidDirectory { .. })
        ));
        let root = std::env::temp_dir();
        assert_eq!(
            validate_home_directory(Some(root.clone())).unwrap(),
            root.to_str().unwrap()
        );
        assert!(matches!(
            validate_home_directory(Some(root.join(Uuid::new_v4().to_string()))),
            Err(SpawnError::InvalidDirectory { .. })
        ));
    }

    #[test]
    fn policy_denial_and_service_failure_never_become_local_fallback() {
        assert_eq!(enforce_read_policy(Ok((true, None))), Ok(()));
        for (response, expected) in [
            (
                Ok((false, Some("explicit denial".into()))),
                "explicit denial",
            ),
            (Ok((false, None)), "directory access denied"),
            (
                Err("unsupported policy operation".into()),
                "unsupported policy operation",
            ),
            (
                Err("policy service unavailable".into()),
                "policy service unavailable",
            ),
        ] {
            assert_eq!(
                enforce_read_policy(response),
                Err(SpawnError::Denied {
                    reason: expected.into()
                })
            );
        }
    }

    #[test]
    fn local_fallback_only_applies_to_missing_permissions() {
        use bmux_plugin::StreamOrigin as StreamProvenance;
        let caller = Uuid::new_v4();
        let local = StreamProvenance::Local;
        let remote = StreamProvenance::Admitted {
            subject_id: Uuid::new_v4(),
            lease_id: Uuid::new_v4(),
        };
        assert_eq!(check_read_identity(false, None, &local), Ok(None));
        assert_eq!(check_read_identity(false, Some(caller), &local), Ok(None));
        assert_eq!(
            check_read_identity(true, Some(caller), &local),
            Ok(Some(caller))
        );
        assert!(matches!(
            check_read_identity(true, None, &local),
            Err(SpawnError::Denied { .. })
        ));
        assert!(check_read_identity(false, Some(caller), &remote).is_err());
        assert!(check_read_identity(false, None, &StreamProvenance::Invalid).is_err());
        assert!(check_read_identity(true, Some(caller), &StreamProvenance::Invalid).is_err());
        assert_eq!(
            check_read_identity(true, Some(caller), &remote),
            Ok(Some(caller))
        );
    }
}
