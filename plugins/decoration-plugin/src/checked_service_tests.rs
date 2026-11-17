//! Checked transport regressions execute the actual synchronous decoration handler.
use super::*;
use bmux_plugin_sdk::author::{Declarations, Plugin};
use bmux_plugin_sdk::definition::{
    LoadedInstanceId, PluginCallContext, PluginCreateContext, PluginInstance,
};
use bmux_plugin_sdk::{
    PluginBlockingCallback, PluginCallbackAuthority, PluginCallbackAuthoritySource,
    PluginCallbackContext, PluginCallbackEntryGuard, PluginCallbackExecution,
    PluginCallbackExecutor, PluginCallbackSettlement, PluginCommandError,
};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Test-only host authority: always live, never cancelled. Stands in for the
/// admitted invocation scope a real host binds per call.
struct TestAuthority;
impl PluginCallbackAuthoritySource for TestAuthority {
    fn check_live(&self) -> bmux_plugin_sdk::definition::PluginResult<()> {
        Ok(())
    }
    fn enter_adapter_entry(
        &self,
        _: LoadedInstanceId,
    ) -> bmux_plugin_sdk::definition::PluginResult<PluginCallbackEntryGuard> {
        Ok(PluginCallbackEntryGuard::from_host(()))
    }
    fn cancelled(&self) -> PluginCallbackSettlement {
        Box::pin(std::future::pending())
    }
}

/// Test-only selected executor that only accepts already entered callbacks and
/// runs them on a tokio blocking worker after revalidating the entry.
struct TestExecutor;
impl PluginCallbackExecution for TestExecutor {
    fn execute(
        &self,
        _: PluginCallbackAuthority,
        _: LoadedInstanceId,
        _: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        Box::pin(async {
            Err(PluginCommandError::unavailable(
                "test executor requires a prepared entry",
            ))
        })
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

fn test_callback_execution() -> PluginCallbackContext {
    PluginCallbackContext::from_host(
        PluginCallbackAuthority::from_host(Arc::new(TestAuthority)),
        PluginCallbackExecutor::from_host(Arc::new(TestExecutor)),
    )
}

#[derive(Default)]
struct CountedDecoration {
    plugin: DecorationPlugin,
    effects: Arc<AtomicUsize>,
}
impl Plugin for CountedDecoration {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_services(Self::invoke_service, None);
    }
}

impl CountedDecoration {
    fn invoke_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        if context.request.operation == "apply-view-theme-extension" {
            self.effects.fetch_add(1, Ordering::SeqCst);
        }
        self.plugin.invoke_service(context)
    }
}

fn apply_context(contract: &[u8]) -> NativeServiceContext {
    let request = commands::ApplyViewThemeExtensionRequest {
        toml: "border = 'double'".into(),
        config_dir_candidates: Vec::new(),
    };
    context(
        Some(Uuid::from_u128(0x41)),
        "decoration-commands",
        "apply-view-theme-extension",
        bmux_plugin_sdk::service_contract::encode(
            "decoration-commands",
            contract,
            &encode_service_message(&request).unwrap(),
        )
        .unwrap(),
    )
}

fn invocation(context: &NativeServiceContext) -> PluginCallContext {
    PluginCallContext {
        callback_execution: Some(test_callback_execution()),
        invocation_id: context.cancellation.invocation_id.clone(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: context.host_service_port.clone(),
    }
}

#[tokio::test]
async fn wrong_schema_has_no_effect_and_checked_apply_executes_once() {
    let counted = CountedDecoration::default();
    let effects = counted.effects.clone();
    let adapter = bmux_plugin_sdk::author::DeclaredInstance::new(
        counted,
        crate::declaration::PLUGIN_MANIFEST,
        crate::declaration::SERVICE_CONTRACTS,
    )
    .unwrap()
    .for_instance(LoadedInstanceId::from_uuid(Uuid::from_u128(0x43)));
    let wrong = apply_context(&[0; 32]);
    assert!(adapter.invoke(invocation(&wrong), wrong).await.is_err());
    assert_eq!(effects.load(Ordering::SeqCst), 0);

    let valid = apply_context(bmux_decoration_plugin_api::decoration_commands::SERVICE_CONTRACT);
    let response = adapter.invoke(invocation(&valid), valid).await.unwrap();
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let body = bmux_plugin_sdk::service_contract::decode_response(&response.payload).unwrap();
    assert!(
        bmux_plugin_sdk::service_contract::decode_response(body).is_err(),
        "response framed twice"
    );
    assert!(
        decode_service_message::<
            Result<(), bmux_decoration_plugin_api::decoration_state::ValidationResult>,
        >(body)
        .unwrap()
        .is_ok()
    );
}

#[tokio::test]
async fn selected_definition_uses_the_same_checked_boundary() {
    let definition = crate::plugin_definition();
    let instance = definition
        .create(PluginCreateContext {
            runtime_scope: bmux_plugin_sdk::definition::RuntimeScopeId::default(),
            instance: bmux_plugin_sdk::definition::LoadedInstanceId::from_uuid(Uuid::from_u128(
                0x42,
            )),
            role: bmux_plugin_sdk::definition::ComponentRole::Backend,
        })
        .unwrap();
    let wrong = apply_context(&[0; 32]);
    assert!(instance.invoke(invocation(&wrong), wrong).await.is_err());
    let valid = apply_context(bmux_decoration_plugin_api::decoration_commands::SERVICE_CONTRACT);
    let response = instance.invoke(invocation(&valid), valid).await.unwrap();
    assert!(response.error.is_none(), "{:?}", response.error);
    let body = bmux_plugin_sdk::service_contract::decode_response(&response.payload).unwrap();
    assert!(
        decode_service_message::<
            Result<(), bmux_decoration_plugin_api::decoration_state::ValidationResult>,
        >(body)
        .unwrap()
        .is_ok()
    );
}
