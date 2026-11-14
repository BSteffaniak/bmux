//! Shared implementation for loader tests and the explicit native fixture artifact.
use bmux_plugin_sdk::definition::{
    ComponentRole, PluginCallContext, PluginCreateContext, PluginDefinition, PluginFuture,
    PluginInstance, PluginMetadata, PluginResult,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Counter {
    scope: PluginCreateContext,
    mutations: AtomicUsize,
}
impl PluginInstance for Counter {
    fn invoke(
        &self,
        _call: PluginCallContext,
        context: bmux_plugin_sdk::NativeServiceContext,
    ) -> PluginFuture<'_, PluginResult<bmux_plugin_sdk::ServiceResponse>> {
        Box::pin(async move {
            bmux_plugin_sdk::service_contract::decode_for(
                "acceptance.counter/v1",
                &[7; 32],
                &context.request.payload,
            )
            .map_err(|error| bmux_plugin_sdk::PluginCommandError::unavailable(error.to_string()))?;
            self.mutations.fetch_add(1, Ordering::SeqCst);
            Ok(bmux_plugin_sdk::ServiceResponse::ok(b"committed".to_vec()))
        })
    }
    fn extension(
        &self,
        call: PluginCallContext,
        contract: String,
        _payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            match contract.as_str() {
                "scope" => Ok(serde_json::to_vec(&self.scope).unwrap()),
                "mutate" => {
                    self.mutations.fetch_add(1, Ordering::SeqCst);
                    // Exceeds the loader's initial 4 KiB outcome buffer, below ABI bounds.
                    Ok(vec![42; 64 * 1024])
                }
                "count" => Ok(self
                    .mutations
                    .load(Ordering::SeqCst)
                    .to_string()
                    .into_bytes()),
                "pending" => {
                    std::future::poll_fn(move |_cx| {
                        if call.cancellation.is_cancelled() {
                            std::task::Poll::Ready(Ok(b"cancelled".to_vec()))
                        } else {
                            std::task::Poll::Pending
                        }
                    })
                    .await
                }
                _ => Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                    "unknown fixture extension",
                )),
            }
        })
    }
}
pub fn plugin_definition() -> PluginDefinition {
    PluginDefinition::new(
        PluginMetadata {
            plugin_id: "acceptance.scoped-loader".into(),
            version: "1.0.0".into(),
            roles: vec![ComponentRole::Backend, ComponentRole::Frontend],
            required_capabilities: vec!["acceptance.allowed".into()],
            contracts: vec![],
            manifest_toml: None,
        },
        |scope| {
            Ok(Arc::new(Counter {
                scope,
                mutations: AtomicUsize::new(0),
            }) as Arc<dyn PluginInstance>)
        },
    )
}
