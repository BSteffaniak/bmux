//! Synchronous declared bodies: entered execution, checked framing and
//! command-outcome metadata.
use super::{Declarations, DeclaredInstance, Plugin};
use crate::definition::{LoadedInstanceId, PluginCallContext, PluginInstance};
use crate::{
    CapabilityId, InterfaceId, NativeCommandContext, NativeServiceContext, PluginBlockingCallback,
    PluginCallbackAuthority, PluginCallbackAuthoritySource, PluginCallbackContext,
    PluginCallbackEntryGuard, PluginCallbackExecution, PluginCallbackExecutor,
    PluginCallbackSettlement, PluginCommandError, PluginResult, ServiceInterfaceDescriptor,
    ServiceKind, ServiceResponse,
};
use std::sync::Arc;

const CONTRACT: [u8; 32] = [1; 32];
static SERVICES: [ServiceInterfaceDescriptor; 1] = [ServiceInterfaceDescriptor {
    capability: CapabilityId::from_static("example.read"),
    kind: ServiceKind::Query,
    interface_id: InterfaceId::from_static("generated/v3"),
    contract: &CONTRACT,
    reentrant_operations: &[],
    access: None,
    operation_access: &[],
}];
const MANIFEST: &str = r#"
id = "example.body"
version = "1"
component_roles = ["backend"]

[[commands]]
name = "status"
"#;

/// Test-only host authority: always live, never cancelled.
struct Authority;
impl PluginCallbackAuthoritySource for Authority {
    fn check_live(&self) -> PluginResult<()> {
        Ok(())
    }
    fn enter_adapter_entry(&self, _: LoadedInstanceId) -> PluginResult<PluginCallbackEntryGuard> {
        Ok(PluginCallbackEntryGuard::from_host(()))
    }
    fn cancelled(&self) -> PluginCallbackSettlement {
        Box::pin(std::future::pending())
    }
}

/// Test-only selected executor: runs entered callbacks on a blocking worker.
struct Executor;
impl PluginCallbackExecution for Executor {
    fn execute(
        &self,
        _: PluginCallbackAuthority,
        _: LoadedInstanceId,
        _: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        Box::pin(async { Err(PluginCommandError::unavailable("entry required")) })
    }
    fn execute_entered(
        &self,
        authority: PluginCallbackAuthority,
        instance: LoadedInstanceId,
        entry: PluginCallbackEntryGuard,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        Box::pin(async move {
            entry.validate_for(&authority, instance)?;
            tokio::task::spawn_blocking(move || {
                entry.validate_for(&authority, instance)?;
                let result = callback();
                drop(entry);
                result
            })
            .await
            .map_err(|error| PluginCommandError::failed(error.to_string()))?
        })
    }
}

fn execution() -> PluginCallbackContext {
    PluginCallbackContext::from_host(
        PluginCallbackAuthority::from_host(Arc::new(Authority)),
        PluginCallbackExecutor::from_host(Arc::new(Executor)),
    )
}

#[derive(Default)]
struct Body;

impl Plugin for Body {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_services(Self::serve, None)
            .blocking_command("status", |_, _| {
                crate::record_command_outcome_metadata(
                    crate::COMMAND_OUTCOME_STATUS_MESSAGE_KEY,
                    serde_json::Value::String("done".into()),
                );
                Ok(crate::EXIT_OK)
            });
    }
}

impl Body {
    fn serve(_: &Self, context: &NativeServiceContext) -> ServiceResponse {
        assert_eq!(context.request.payload, b"body");
        ServiceResponse::ok(b"reply".to_vec())
    }
}

fn instance() -> DeclaredInstance<Body> {
    DeclaredInstance::new(Body, MANIFEST, &SERVICES)
        .unwrap()
        .for_instance(LoadedInstanceId::from_uuid(uuid::Uuid::from_u128(7)))
}

fn service_context(payload: &[u8]) -> NativeServiceContext {
    serde_json::from_value(serde_json::json!({
        "plugin_id": "example.body",
        "request": {
            "caller_plugin_id": "example.caller",
            "service": {
                "capability": "example.read",
                "kind": "query",
                "interface_id": "generated/v3",
                "provider": {"Plugin": "example.body"}
            },
            "operation": "read",
            "payload": payload
        },
        "required_capabilities": [],
        "provided_capabilities": [],
        "services": [],
        "available_capabilities": [],
        "enabled_plugins": [],
        "plugin_search_roots": [],
        "host": {"product_name": "test", "product_version": "1",
            "plugin_api_version": {"major": 1, "minor": 0},
            "plugin_abi_version": {"major": 2, "minor": 0}},
        "connection": {"config_dir": "", "runtime_dir": "", "data_dir": "", "state_dir": ""}
    }))
    .unwrap()
}

fn call(execution: Option<PluginCallbackContext>) -> PluginCallContext {
    PluginCallContext {
        callback_execution: execution,
        invocation_id: crate::PluginInvocationId::new(),
        cancellation: crate::ServiceCancellation::new(),
        host_services: None,
    }
}

#[tokio::test]
async fn checked_blocking_body_is_decoded_and_its_response_framed_once() {
    let frame = crate::service_contract::encode("generated/v3", &CONTRACT, b"body").unwrap();
    let response = instance()
        .invoke(call(Some(execution())), service_context(&frame))
        .await
        .unwrap();
    let body = crate::service_contract::decode_response(&response.payload).unwrap();
    assert_eq!(body, b"reply");
    assert!(crate::service_contract::decode_response(body).is_err());

    let plain = instance()
        .invoke(call(Some(execution())), service_context(b"body"))
        .await
        .unwrap();
    assert_eq!(plain.payload, b"reply");

    let wrong = crate::service_contract::encode("generated/v3", &[2; 32], b"body").unwrap();
    assert!(
        instance()
            .invoke(call(Some(execution())), service_context(&wrong))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn blocking_bodies_need_callback_authority_and_an_instance_binding() {
    let missing = instance()
        .invoke(call(None), service_context(b"body"))
        .await
        .unwrap_err();
    assert!(
        missing
            .message
            .contains("callback execution authority missing")
    );
    let unbound = DeclaredInstance::new(Body, MANIFEST, &SERVICES)
        .unwrap()
        .invoke(call(Some(execution())), service_context(b"body"))
        .await
        .unwrap_err();
    assert!(
        unbound
            .message
            .contains("callback instance binding missing")
    );
}

#[tokio::test]
async fn blocking_command_returns_the_metadata_its_body_records() {
    let context: NativeCommandContext = serde_json::from_value(serde_json::json!({
        "plugin_id": "example.body",
        "command": "status",
        "arguments": [],
        "required_capabilities": [],
        "provided_capabilities": [],
        "services": [],
        "available_capabilities": [],
        "enabled_plugins": [],
        "plugin_search_roots": [],
        "host": {"product_name": "test", "product_version": "1",
            "plugin_api_version": {"major": 1, "minor": 0},
            "plugin_abi_version": {"major": 2, "minor": 0}},
        "connection": {"config_dir": "", "runtime_dir": "", "data_dir": "", "state_dir": ""}
    }))
    .unwrap();
    let outcome = instance().command(call(Some(execution())), context).await;
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome
            .metadata
            .get(crate::COMMAND_OUTCOME_STATUS_MESSAGE_KEY),
        Some(&serde_json::Value::String("done".into()))
    );
}
