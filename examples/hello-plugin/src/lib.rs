//! One export-free implementation shared by bundled and dynamic packaging.

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/hello-plugin.bpdl",
        manifest: "plugin.toml",
    }
}
use bmux_plugin_sdk::definition::{
    CommandOutcome, ComponentRole, PluginCallContext, PluginCreateContext, PluginFuture,
    PluginInstance, PluginMetadata, PluginResult,
};
use bmux_plugin_sdk::{EXIT_OK, NativeCommandContext, PluginCommandError};
use std::sync::Arc;

#[derive(Default)]
pub struct HelloPlugin;

impl PluginInstance for HelloPlugin {
    fn command(
        &self,
        _call: PluginCallContext,
        context: NativeCommandContext,
    ) -> PluginFuture<'_, CommandOutcome> {
        Box::pin(async move {
            if context.command != "hello" {
                return Err(PluginCommandError::unknown_command(&context.command)).into();
            }
            let name = context.arguments.first().map_or("world", String::as_str);
            println!("Hello, {name}!");
            Ok(EXIT_OK).into()
        })
    }
}

fn create(_context: PluginCreateContext) -> PluginResult<Arc<dyn PluginInstance>> {
    Ok(Arc::new(HelloPlugin))
}

bmux_plugin_sdk::define_plugin!(
    factory = create,
    metadata = PluginMetadata {
        plugin_id: "example.hello".into(),
        version: "0.0.1".into(),
        roles: vec![ComponentRole::Backend],
        required_capabilities: vec![],
        contracts: vec![],
        manifest_toml: Some(declaration::PLUGIN_MANIFEST.into()),
    },
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_is_inert_and_backend_only() {
        let definition = plugin_definition();
        assert_eq!(definition.metadata.plugin_id, "example.hello");
        assert_eq!(definition.metadata.roles, vec![ComponentRole::Backend]);
        assert!(
            definition
                .metadata
                .manifest_toml
                .unwrap()
                .contains("example.hello")
        );
    }
}
