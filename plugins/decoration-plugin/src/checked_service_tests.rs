//! Checked transport regressions execute the actual legacy decoration handler.
use super::*;
use bmux_plugin_sdk::definition::{PluginCallContext, PluginCreateContext, PluginInstance};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct CountedDecoration {
    plugin: DecorationPlugin,
    effects: Arc<AtomicUsize>,
}
impl RustPlugin for CountedDecoration {
    type Contract = bmux_decoration_plugin_api::Contract;

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.operation == "apply-view-theme-extension" {
            self.effects.fetch_add(1, Ordering::SeqCst);
        }
        RustPlugin::invoke_service(&self.plugin, context)
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
        invocation_id: context.cancellation.invocation_id.clone(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: context.host_service_port.clone(),
    }
}

#[tokio::test]
async fn wrong_schema_has_no_effect_and_checked_apply_executes_once() {
    let counted = CountedDecoration::default();
    let effects = counted.effects.clone();
    let adapter = bmux_plugin_sdk::definition::RustPluginAdapter::new(counted);
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
