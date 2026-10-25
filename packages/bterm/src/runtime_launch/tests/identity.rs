use super::*;

fn selected(root: &Root) -> crate::runtime_selection::RuntimeSelection {
    use crate::runtime_selection::{ManagedTargetIdentity, RuntimeSelection};
    let durable_roots = crate::runtime_paths::RuntimeRoots::from_root(root.0.clone()).unwrap();
    let identity = ManagedTargetIdentity {
        schema_version: 1,
        compatibility_id: crate::compatibility::COMPATIBILITY_ID.into(),
        artifact_set_sha256: "a".repeat(64),
        companion_sha256: "b".repeat(64),
        configuration_id: "c".repeat(64),
        scope_id: "d".repeat(64),
        durable_roots: durable_roots.clone(),
    };
    RuntimeSelection::Managed {
        roots: durable_roots,
        companion: crate::compatibility::VerifiedCompanion {
            executable: root.0.join("companion"),
            bundle_id: identity.compatibility_id.clone(),
            artifact_set_sha256: identity.artifact_set_sha256.clone(),
            sha256: identity.companion_sha256.clone(),
            protocol: bmux_ipc::ProtocolContract::current(
                bmux_ipc::default_supported_capabilities(),
            ),
        },
        identity: Box::new(identity),
    }
}

fn readiness(
    selection: &crate::runtime_selection::RuntimeSelection,
) -> bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::RuntimeReadiness {
    // Decode wire fields so additive readiness fields do not obscure the exact
    // identity dimensions this boundary authenticates.
    let roots = selection.roots();
    serde_json::from_value(serde_json::json!({
        "instance": "00000000-0000-0000-0000-000000000001",
        "process_id": 7,
        "executable_sha256": "b".repeat(64),
        "managed_target_identity": serde_json::to_string(selection.managed_identity().unwrap()).unwrap(),
        "runtime_dir": roots.runtime_dir,
        "config_dir": roots.config_dir,
        "data_dir": roots.data_dir,
        "state_dir": roots.state_dir,
        "helper_ready": true,
        "helper_error": null,
        "restart_blocker": null,
        "fenced": false,
    })).unwrap()
}

#[test]
fn live_identity_checks_every_dimension_and_never_uses_receipt_as_proof() {
    let root = Root::new();
    let selection = selected(&root);
    let live = readiness(&selection);
    authenticate_managed_readiness(&selection, &live).unwrap();
    for dimension in 0..7 {
        let mut changed = selection.managed_identity().unwrap().clone();
        match dimension {
            0 => changed.compatibility_id.push('x'),
            1 => changed.artifact_set_sha256.push('x'),
            2 => changed.companion_sha256.push('x'),
            3 => changed.configuration_id.push('x'),
            4 => changed.scope_id.push('x'),
            5 => changed.durable_roots.data_dir.push("other"),
            _ => changed.schema_version += 1,
        }
        let mut live = readiness(&selection);
        live.managed_target_identity = Some(serde_json::to_string(&changed).unwrap());
        assert!(authenticate_managed_readiness(&selection, &live).is_err());
    }
    for identity in [None, Some("not-json".into())] {
        let mut live = readiness(&selection);
        live.managed_target_identity = identity;
        assert!(authenticate_managed_readiness(&selection, &live).is_err());
    }
    let mut live = readiness(&selection);
    live.executable_sha256 = Some("e".repeat(64));
    assert!(authenticate_managed_readiness(&selection, &live).is_err());
    let mut live = readiness(&selection);
    live.runtime_dir.push_str("/other");
    assert!(authenticate_managed_readiness(&selection, &live).is_err());
}

#[test]
fn generated_readiness_retries_only_typed_transport_dispatch() {
    use bmux_plugin_sdk::{TypedDispatchClientError, TypedServiceClientError};
    let transport = TypedServiceClientError::Dispatch(TypedDispatchClientError::transport(
        "readiness",
        "runtime-readiness",
        "opaque transport diagnostic",
    ));
    assert!(retryable_readiness_dispatch(&transport));
    // Probe failures are wrapped in anyhow contexts before reconciliation.
    assert!(retryable_disconnect(
        &anyhow::Error::new(transport).context("probe")
    ));
    for error in [
        TypedServiceClientError::Dispatch(TypedDispatchClientError::server(
            "readiness",
            "runtime-readiness",
            "connection reset is only text",
        )),
        TypedServiceClientError::Dispatch(TypedDispatchClientError::UnexpectedResponse {
            interface: "readiness".into(),
            operation: "runtime-readiness".into(),
            details: "transport failure is only text".into(),
        }),
        TypedServiceClientError::Encode {
            op: "runtime-readiness".into(),
            details: "broken pipe is only text".into(),
        },
        TypedServiceClientError::Decode {
            op: "runtime-readiness".into(),
            details: "unexpected EOF is only text".into(),
        },
    ] {
        assert!(!retryable_readiness_dispatch(&error));
        assert!(!retryable_disconnect(
            &anyhow::Error::new(error).context("probe")
        ));
    }
}

#[test]
fn retries_only_transport_disconnects_never_application_or_authentication_failures() {
    for kind in [
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::UnexpectedEof,
        std::io::ErrorKind::NotFound,
    ] {
        let error = anyhow::Error::new(bmux_client::ClientError::Transport(
            bmux_ipc::transport::IpcTransportError::Io(std::io::Error::from(kind)),
        ));
        assert!(retryable_disconnect(&error));
    }
    for kind in [
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::InvalidData,
    ] {
        let error = anyhow::Error::new(bmux_client::ClientError::Transport(
            bmux_ipc::transport::IpcTransportError::Io(std::io::Error::from(kind)),
        ));
        assert!(!retryable_disconnect(&error));
    }
    let error = anyhow::Error::new(bmux_client::ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: "application refusal".into(),
    });
    assert!(!retryable_disconnect(&error));
}

#[cfg(unix)]
#[tokio::test]
async fn startup_claim_symlink_is_rejected_without_launching() {
    let root = Root::new();
    let spec = root.spec();
    let other = root.0.join("other-lock");
    std::fs::write(&other, b"retain").unwrap();
    std::os::unix::fs::symlink(&other, &spec.ownership_file).unwrap();
    assert!(
        ensure_runtime(&spec, || async { Ok(RuntimeProbe::Absent) })
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&other).unwrap(), b"retain");
    assert!(!spec.diagnostic_file.exists());
}
