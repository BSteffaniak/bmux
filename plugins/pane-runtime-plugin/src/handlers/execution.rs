//! Authorization at the exact execution service boundary.
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalExecutionError as ExecutionError, TerminalExecutionRef as ExecutionRef,
};
use bmux_plugin_sdk::{NativeServiceContext, ServiceResponse};

// Explicit service/operation pairs only. Unmigrated routes require positive
// Local provenance even when invoked directly via generated service clients.
pub(crate) fn migrated_route(interface: &str, operation: &str) -> bool {
    matches!(
        (interface, operation),
        (
            "pane-runtime-execution-v4",
            "reserve"
                | "launch"
                | "reconcile"
                | "send-admitted"
                | "input-receipt"
                | "resolve"
                | "retire"
                | "new-program-session"
                | "status"
                | "screen"
                | "send"
                | "resize"
                | "terminate"
                | "wait"
        ) | (
            "pane-runtime-presentation-v1",
            "capabilities" | "resize" | "capture-events" | "present"
        ) | (
            "pane-runtime-presentation-wait-v1",
            "capabilities" | "wait-present"
        ) | (
            "pane-runtime-pointer-grid-v1",
            "capabilities" | "screen" | "wait-present"
        ) | (
            "pane-runtime-spawn-commands-v1",
            "new-session" | "split-shell"
        ) | (
            "pane-runtime-spawn-v1",
            "home-directory" | "capture-source" | "observe-directory"
        )
    )
}

pub(super) fn authorize_route(ctx: &NativeServiceContext) -> Result<(), ServiceResponse> {
    use bmux_plugin::StreamOrigin as StreamProvenance;
    let provenance = super::spawn::caller_provenance(ctx)
        .map_err(|error| ServiceResponse::error("authorization_denied", format!("{error:?}")))?;
    match provenance {
        StreamProvenance::Local => Ok(()),
        StreamProvenance::Admitted { .. }
            if migrated_route(&ctx.request.service.interface_id, &ctx.request.operation) =>
        {
            Ok(())
        }
        StreamProvenance::Admitted { .. } => Err(ServiceResponse::error(
            "authorization_denied",
            "remote route is not migrated to authoritative resource checks",
        )),
        StreamProvenance::Invalid | StreamProvenance::HostInternal => Err(ServiceResponse::error(
            "authorization_denied",
            "trusted caller provenance invalid or revoked",
        )),
    }
}

pub(crate) fn local_input_authority(
    ctx: &NativeServiceContext,
    session_id: uuid::Uuid,
) -> crate::runtime::input_authority::InputAuthority {
    let ctx = ctx.clone();
    crate::runtime::input_authority::InputAuthority::new(move || {
        if ctx.cancellation.is_cancelled() {
            return Err("local input cancelled".into());
        }
        authorize_route(&ctx).map_err(|_| "local input authority ended".to_string())?;
        super::spawn::authorize_session(&ctx, session_id, "pane.direct_input")
            .map_err(|error| format!("{error:?}"))?;
        if let Some(client) = ctx.caller_client_id {
            let runtime = super::session_runtime_handle()
                .ok_or_else(|| "input runtime unavailable".to_string())?;
            if !runtime.0.client_can_write(
                bmux_session_models::SessionId(session_id),
                bmux_session_models::ClientId(client),
            ) {
                return Err("local input write permission revoked".into());
            }
        }
        Ok(())
    })
}

/// Retain the owned host bridge: deferred checks must never run under ambient
/// worker identity or substitute caller-supplied IDs for admission authority.
pub(crate) fn input_authority(
    ctx: &NativeServiceContext,
    execution: &ExecutionRef,
) -> Result<crate::runtime::input_authority::InputAuthority, ExecutionError> {
    authorize(ctx, execution, "pane.direct_input")?;
    let ctx = ctx.clone();
    let execution = execution.clone();
    Ok(crate::runtime::input_authority::InputAuthority::new(
        move || {
            authorize(&ctx, &execution, "pane.direct_input").map_err(|error| format!("{error:?}"))
        },
    ))
}

pub(crate) fn authorize(
    ctx: &NativeServiceContext,
    execution: &ExecutionRef,
    action: &str,
) -> Result<(), ExecutionError> {
    if ctx.cancellation.is_cancelled() {
        return Err(ExecutionError::OperationFailed {
            reason: "execution request cancelled".into(),
        });
    }
    super::spawn::authorize_session(ctx, execution.session_id, action).map_err(|error| {
        ExecutionError::Denied {
            reason: format!("{error:?}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::StreamOrigin as StreamProvenance;
    use bmux_plugin_sdk::{
        ApiVersion, HostConnectionInfo, HostMetadata, HostScope, ProviderId, RegisteredService,
        ServiceKind, ServiceRequest, encode_service_message,
    };
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    thread_local! {
        static TEST_CANCELLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static TEST_ORIGIN: std::cell::RefCell<Arc<Mutex<StreamProvenance>>> =
            std::cell::RefCell::new(Arc::new(Mutex::new(StreamProvenance::Invalid)));
    }

    unsafe extern "C" fn origin_bridge(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        // Live cancellation is owned by the bound host bridge, not the SDK's
        // serialized cancellation snapshot retained in the cloned context.
        if TEST_CANCELLED.with(std::cell::Cell::get) {
            return 1;
        }
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let Ok(request) = bmux_plugin_sdk::decode_service_message::<
            bmux_plugin_sdk::HostKernelBridgeRequest,
        >(input) else {
            return 1;
        };
        let Ok(bmux_ipc::Request::CurrentStreamProvenance) = bmux_ipc::decode(&request.payload)
        else {
            return 1;
        };
        let provenance = TEST_ORIGIN.with(|current| current.borrow().lock().unwrap().clone());
        let payload = bmux_ipc::encode(&bmux_ipc::Response::Ok(
            bmux_ipc::ResponsePayload::CurrentStreamProvenance { provenance },
        ))
        .unwrap();
        let output =
            encode_service_message(&bmux_plugin_sdk::HostKernelBridgeResponse { payload }).unwrap();
        if output.len() > output_capacity {
            return 4;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(output.as_ptr(), output_ptr, output.len());
            *output_len = output.len();
        }
        0
    }

    fn context(interface: &str, operation: &str, payload: Vec<u8>) -> NativeServiceContext {
        TEST_CANCELLED.with(|cancelled| cancelled.set(false));
        NativeServiceContext {
            plugin_id: "bmux.pane_runtime".into(),
            request: ServiceRequest {
                caller_plugin_id: "test.forged-owner".into(),
                service: RegisteredService {
                    capability: HostScope::new("bmux.pane_runtime.write").unwrap(),
                    kind: ServiceKind::Command,
                    interface_id: interface.into(),
                    provider: ProviderId::Plugin("bmux.pane_runtime".into()),
                },
                operation: operation.into(),
                payload,
            },
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".into(),
                product_version: "test".into(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: Vec::new(),
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: Some(Uuid::new_v4()),
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: Some(bmux_plugin_sdk::HostKernelBridge::from_fn(origin_bridge)),
        }
    }

    #[test]
    fn queued_service_input_rechecks_origin_and_cancellation() {
        let origin = Arc::new(Mutex::new(StreamProvenance::Local));
        TEST_ORIGIN.with(|current| *current.borrow_mut() = origin.clone());
        let mut ctx = context("pane-runtime-execution-v4", "send", Vec::new());
        ctx.caller_client_id = None;
        let execution = ExecutionRef {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_token: Uuid::new_v4(),
        };
        // No optional domain plugins: baseline local execution still works.
        let authority = input_authority(&ctx, &execution).unwrap();
        let mut output = Vec::new();
        authority.write(&mut output, b"local").unwrap();
        assert_eq!(output, b"local");
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(authority).unwrap();
        *origin.lock().unwrap() = StreamProvenance::Invalid;
        assert!(rx.recv().unwrap().write(&mut output, b"revoked").is_err());
        assert_eq!(output, b"local");
        *origin.lock().unwrap() = StreamProvenance::Local;
        let authority = input_authority(&ctx, &execution).unwrap();
        ctx.cancellation.cancel();
        TEST_CANCELLED.with(|cancelled| cancelled.set(true));
        assert!(authority.write(&mut output, b"cancelled").is_err());
        assert_eq!(output, b"local");
        *origin.lock().unwrap() = StreamProvenance::Admitted {
            subject_id: Uuid::new_v4(),
            lease_id: Uuid::new_v4(),
        };
        let remote = context("pane-runtime-execution-v4", "send", Vec::new());
        assert!(input_authority(&remote, &execution).is_err());
    }

    #[test]
    fn legacy_local_queue_keeps_authority_after_ingress_scope() {
        use crate::runtime::input_authority::{local_command, with_local};
        let origin = Arc::new(Mutex::new(StreamProvenance::Local));
        TEST_ORIGIN.with(|current| *current.borrow_mut() = origin.clone());
        let mut ctx = context("attach-runtime-commands", "attach-input", Vec::new());
        ctx.caller_client_id = None;
        let session = bmux_session_models::SessionId(Uuid::new_v4());
        let command = with_local(ctx.clone(), || local_command(b"queued".to_vec(), session));
        let crate::runtime::PaneRuntimeCommand::AuthorizedInput(bytes, authority) = command else {
            panic!("service input lost authority");
        };
        let mut output = Vec::new();
        authority.write(&mut output, &bytes).unwrap();
        assert_eq!(output, b"queued");
        ctx.cancellation.cancel();
        TEST_CANCELLED.with(|cancelled| cancelled.set(true));
        assert!(authority.write(&mut output, &bytes).is_err());
        // Scope restoration keeps ordinary in-process terminal input unchanged.
        assert!(matches!(
            local_command(b"local".to_vec(), session),
            crate::runtime::PaneRuntimeCommand::Input(_)
        ));
    }

    #[test]
    #[allow(clippy::result_large_err)] // Test router must return the SDK's fixed error type.
    fn service_boundary_denies_legacy_remote_and_rechecks_migrated_mutations() {
        let origin = Arc::new(Mutex::new(StreamProvenance::Admitted {
            subject_id: Uuid::new_v4(),
            lease_id: Uuid::new_v4(),
        }));
        TEST_ORIGIN.with(|current| *current.borrow_mut() = Arc::clone(&origin));
        for (interface, operation) in [
            ("pane-runtime-commands", "send-input"),
            ("pane-runtime-commands", "set-client-write-permission"),
            ("attach-runtime-commands", "attach-input"),
            ("pane-runtime-readiness-v1", "fence-unhealthy"),
            ("pane-runtime-execution-v4", "future-mutation"),
        ] {
            let response = super::super::route(context(interface, operation, Vec::new()));
            assert_eq!(response.error.unwrap().code, "authorization_denied");
        }
        let execution = ExecutionRef {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_token: Uuid::new_v4(),
        };
        let ctx = context("pane-runtime-execution-v4", "send", Vec::new());
        for action in [
            "pane.direct_input",
            "pane.resize",
            "pane.close",
            "session.create",
        ] {
            // Missing permissions cannot become remote local fallback.
            assert!(
                super::super::spawn::authorize_session(&ctx, execution.session_id, action).is_err()
            );
        }
        let payload = encode_service_message(
            &bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::SendRequest {
                execution,
                data: vec![1],
            },
        )
        .unwrap();
        let response = super::super::route(context("pane-runtime-execution-v4", "send", payload));
        assert!(response.error.is_none());
        let result: Result<(), ExecutionError> =
            bmux_plugin_sdk::decode_service_message(&response.payload).unwrap();
        assert!(matches!(result, Err(ExecutionError::Denied { .. })));
        *origin.lock().unwrap() = StreamProvenance::Invalid;
        let response = super::super::route(context(
            "pane-runtime-presentation-v1",
            "capabilities",
            Vec::new(),
        ));
        assert_eq!(response.error.unwrap().code, "authorization_denied");
        *origin.lock().unwrap() = StreamProvenance::HostInternal;
        assert!(
            super::super::route(context(
                "pane-runtime-presentation-v1",
                "capabilities",
                Vec::new()
            ))
            .error
            .is_some()
        );
        let mut unbound = context("pane-runtime-presentation-v1", "capabilities", Vec::new());
        unbound.host_kernel_bridge = None;
        assert!(super::super::route(unbound).error.is_some());
        *origin.lock().unwrap() = StreamProvenance::Local;
        // No clients, connections, permissions, or other domain services exist.
        assert!(ctx.enabled_plugins.is_empty() && ctx.services.is_empty());
        assert!(
            super::super::spawn::authorize_session(&ctx, Uuid::new_v4(), "pane.direct_input")
                .is_ok()
        );
        let response = super::super::route(context(
            "pane-runtime-presentation-v1",
            "capabilities",
            encode_service_message(&()).unwrap(),
        ));
        assert!(response.error.is_none());
    }
}
