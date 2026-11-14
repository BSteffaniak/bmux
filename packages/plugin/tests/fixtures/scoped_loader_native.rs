//! Thin native fixture input for X1 packaging; not a standalone Cargo package.
//! Build as a cdylib with bmux_plugin_sdk and serde_json dependencies.
mod scoped_loader_definition;
bmux_plugin_sdk::export_plugin_entrypoint!(scoped_loader_definition::plugin_definition);
