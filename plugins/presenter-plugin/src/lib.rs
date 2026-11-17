//! Optional plugin-owned presentation commands and authoritative managed hosts.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_presenter_plugin_api::export_schema,
            crate_path: ::bmux_presenter_plugin_api,
        },
        settings: "plugin.toml",
    }
}

mod broadcast;
mod cli;
mod commands;
mod demo;
mod gallery;
mod managed_input;
mod mirror;
mod owned_runtime;
mod owner;
pub mod playbook;
pub mod playbook_capture;
pub mod playbook_component;
pub mod playbook_controller;
mod playbook_live;
mod playbook_observer;
mod player;
mod player_evidence;
mod rich;
mod services;
mod source;
mod tls;
mod viewer;
mod web;
mod web_host;

pub use demo::linked_registry as linked_demo_registry;
pub use owner::LinkedOwner;

use bmux_plugin_sdk::{
    NativeCommandContext, NativeServiceContext, PluginCommandError, RustPlugin, ServiceResponse,
};

#[derive(Default)]
pub struct PresenterPlugin {
    managed: services::ManagedHosts,
}
impl RustPlugin for PresenterPlugin {
    type Contract = bmux_presenter_plugin_api::Contract;
    fn activate(
        &mut self,
        _context: bmux_plugin_sdk::NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        self.managed
            .bind_runtime(tokio::runtime::Handle::try_current().ok());
        Ok(bmux_plugin_sdk::EXIT_OK)
    }
    fn deactivate(
        &mut self,
        _context: bmux_plugin_sdk::NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        self.managed.admit_retirement()?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        self.run(context)
    }
    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        services::invoke(&self.managed, context)
    }
}

impl PresenterPlugin {
    fn run(&self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        if !context.invocation_source.is_cli() {
            return Err(PluginCommandError::failed(
                "present commands require standalone CLI invocation; attach terminal is already owned",
            ));
        }
        let command = cli::Command::parse(&context.command, &context.arguments)
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| PluginCommandError::failed(error.to_string()))?;
                    runtime
                        .block_on(commands::run(command, &context))
                        .map(|()| bmux_plugin_sdk::EXIT_OK)
                        .map_err(|error| PluginCommandError::failed(format!("{error:#}")))
                })
                .join()
                .unwrap_or_else(|_| {
                    Err(PluginCommandError::failed(
                        "presentation command panicked; terminal cleanup attempted",
                    ))
                })
        })
    }
}

bmux_plugin_sdk::define_plugin!(
    factory = |scope: bmux_plugin_sdk::definition::PluginCreateContext| Ok(std::sync::Arc::new(
        bmux_plugin_sdk::definition::RustPluginAdapter::for_instance(
            PresenterPlugin::default(),
            scope.instance
        )
    )
        as std::sync::Arc<dyn bmux_plugin_sdk::definition::PluginInstance>),
    metadata = {
        let manifest_toml = crate::declaration::PLUGIN_MANIFEST;
        let manifest = bmux_plugin::PluginManifest::from_toml_str(manifest_toml)
            .expect("embedded plugin declaration must be valid");
        bmux_plugin_sdk::definition::PluginMetadata {
            plugin_id: manifest.id,
            version: manifest.version,
            roles: manifest.component_roles,
            required_capabilities: manifest
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            contracts: manifest.contracts,
            manifest_toml: Some(manifest_toml.into()),
        }
    },
);

#[cfg(test)]
mod tests {
    #[test]
    fn manifest_declares_packaged_mermaid_and_caller_owned_commands() {
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(crate::declaration::PLUGIN_MANIFEST)
                .unwrap();
        assert!(manifest.auxiliary_executables.is_empty());
        assert_eq!(manifest.commands.len(), 4);
        assert!(
            manifest
                .commands
                .iter()
                .all(|command| command.path.len() == 2 && command.path[0] == "present")
        );
        for name in [
            "present-play",
            "present-host",
            "present-join",
            "present-check",
        ] {
            let command = manifest
                .commands
                .iter()
                .find(|command| command.name == name)
                .unwrap();
            assert_eq!(
                command.execution,
                bmux_plugin_sdk::CommandExecutionKind::CallerProcess
            );
        }
    }
}

#[cfg(test)]
mod embedded_declaration_identity_tests {
    #[test]
    fn definition_and_selected_manifest_have_one_identity() {
        let definition = super::plugin_definition();
        definition.validate().expect("definition validates");
        let embedded = definition
            .metadata
            .manifest_toml
            .as_deref()
            .expect("embedded declaration");
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(embedded).expect("valid manifest");
        assert_eq!(definition.metadata.plugin_id, manifest.id);
        assert_eq!(definition.metadata.version, manifest.version);
        assert_eq!(definition.metadata.roles, manifest.component_roles);
        assert_eq!(definition.metadata.contracts, manifest.contracts);
        assert_eq!(
            definition.metadata.required_capabilities,
            manifest
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );

        // Deterministic verifier fixtures, not claims about built executable bytes.
        // Real selected artifact hashing remains the loader's responsibility.
        let expected_digest = "a".repeat(64);
        let wrong_digest = "b".repeat(64);
        let pinned = format!("artifact_sha256 = \"{expected_digest}\"\n{embedded}");
        let mut inventory = bmux_plugin::PluginCandidateInventory::new();
        inventory
            .add_bundled_manifest(&pinned)
            .expect("inventory declaration");
        for role in &manifest.component_roles {
            let selected = inventory
                .select(&bmux_plugin::PluginSelectionRequest {
                    plugin_id: manifest.id.clone(),
                    role: *role,
                    preference: bmux_plugin::PluginCandidatePreference::Bundled,
                    plugin_api: bmux_plugin_sdk::ApiVersion::new(1, 1),
                    native_abi: bmux_plugin_sdk::ApiVersion::new(1, 1),
                    contracts: manifest.contracts.clone(),
                })
                .expect("select declared role and contracts");
            selected
                .validate_embedded_metadata(&definition.metadata, &expected_digest)
                .expect("selected declaration and embedded identity match");
            assert!(
                selected
                    .validate_embedded_metadata(&definition.metadata, &wrong_digest)
                    .is_err()
            );
            let mut conflicting = definition.metadata.clone();
            conflicting.version.push_str("-conflict");
            assert!(
                selected
                    .validate_embedded_metadata(&conflicting, &expected_digest)
                    .is_err()
            );
        }
    }
}
