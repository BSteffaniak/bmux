#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Prompted Actions plugin for bmux.
//!
//! Provides config-driven prompted actions — keybinding-triggered flows that
//! collect values with one form, substitute them into an action template, and
//! dispatch the result to the attach loop.
//!
//! # Configuration
//!
//! Actions are defined in the user's bmux config:
//!
//! ```toml
//! [plugins.settings."bmux.prompted_actions"]
//!
//! [[plugins.settings."bmux.prompted_actions".actions]]
//! name = "recording-cut"
//! title = "Cut Recording"
//! submit_label = "Export"
//! command = "plugin:bmux.recording:recording-cut --last-seconds {seconds} --export-fps {fps}"
//!
//! [[plugins.settings."bmux.prompted_actions".actions.prompts]]
//! key = "seconds"
//! type = "text"
//! title = "Seconds"
//! placeholder = "last N seconds"
//! validation = "positive_integer"
//!
//! [[plugins.settings."bmux.prompted_actions".actions.prompts]]
//! key = "fps"
//! type = "text"
//! title = "Frame rate"
//! default = "24"
//! validation = "positive_integer"
//! ```
//!
//! Keybinding:
//!
//! ```toml
//! [keybindings.global]
//! "ctrl+alt+r" = "plugin:bmux.prompted_actions:run recording-cut"
//! ```

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/prompted-actions-plugin.bpdl",
        manifest: "plugin.toml",
    }
}

mod config;
mod sequence;

mod activation_ports;

use bmux_plugin_sdk::author::{self, Activation, Call, Deactivation, Declarations, Plugin};
use bmux_plugin_sdk::prelude::*;

#[derive(Default)]
pub struct PromptedActionsPlugin {
    binding: activation_ports::Binding,
    tasks: std::sync::Mutex<tokio::task::JoinSet<()>>,
}

impl Drop for PromptedActionsPlugin {
    fn drop(&mut self) {
        // Dropping JoinSet aborts only this frontend's prompt flows, never the
        // backend work that a successfully submitted action may have started.
        if let Ok(tasks) = self.tasks.get_mut() {
            tasks.abort_all();
        }
    }
}

impl PromptedActionsPlugin {
    /// Supply ports for one subsequent scoped activation; no channels are live
    /// until `PluginInstance::activate` binds that activation.
    #[must_use]
    pub fn with_ports(
        prompt: bmux_plugin::prompt::PromptPort,
        actions: bmux_plugin::action_dispatch::ActionDispatchPort,
    ) -> Self {
        Self {
            binding: activation_ports::Binding::fixed(prompt, actions),
            tasks: std::sync::Mutex::default(),
        }
    }

    fn run(&self, context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
        if context.command != "run" {
            return Err(PluginCommandError::unknown_command(&context.command));
        }
        let (prompt, actions) = self.binding.ports()?;
        let action = resolve_prompted_action(context)?;
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            PluginCommandError::unavailable("prompted actions require a frontend async runtime")
        })?;
        let mut tasks = self.tasks.lock().map_err(|_| {
            PluginCommandError::unavailable("prompted action task owner requires recovery")
        })?;
        while tasks.try_join_next().is_some() {}
        if tasks.len() >= 16 {
            return Err(PluginCommandError::unavailable(
                "too many active prompted action flows",
            ));
        }
        tasks.spawn_on(
            sequence::run_prompted_sequence(action, prompt, actions),
            &handle,
        );
        drop(tasks);
        Ok(EXIT_OK)
    }
}

fn resolve_prompted_action(
    context: &NativeCommandContext,
) -> Result<config::ActionDef, PluginCommandError> {
    let action_name = context
        .arguments
        .first()
        .ok_or_else(|| PluginCommandError::invalid_arguments("missing action name argument"))?;

    let plugin_config =
        config::parse_config(context.settings.as_ref()).map_err(PluginCommandError::failed)?;

    let action_def = plugin_config
        .actions
        .into_iter()
        .find(|a| a.name == *action_name)
        .ok_or_else(|| {
            PluginCommandError::failed(format!(
                "unknown prompted action: {action_name:?} (available: {})",
                plugin_config_action_names_display(context.settings.as_ref())
            ))
        })?;

    Ok(action_def)
}

/// Helper to produce a human-readable list of available action names for
/// error messages.
fn plugin_config_action_names_display(settings: Option<&toml::Value>) -> String {
    let Ok(config) = config::parse_config(settings) else {
        return "(config parse error)".into();
    };
    if config.actions.is_empty() {
        return "(none configured)".into();
    }
    config
        .actions
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

impl Plugin for PromptedActionsPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.native_command(
            "run",
            |plugin: &Self, cx: &Call, context: NativeCommandContext| {
                std::future::ready(plugin.command_now(cx, &context))
            },
        );
    }

    fn activate(
        &self,
        cx: &mut Activation,
    ) -> impl std::future::Future<Output = author::Result<()>> + Send {
        std::future::ready(self.binding.activate(cx.lifecycle()))
    }

    async fn deactivate(&self, cx: &mut Deactivation) -> author::Result<()> {
        self.binding.close(cx.lifecycle())?;
        let mut tasks = {
            let mut owned = self.tasks.lock().map_err(|_| {
                PluginCommandError::unavailable("prompted action task owner requires recovery")
            })?;
            std::mem::take(&mut *owned)
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        self.binding.drained(cx.lifecycle())
    }
}

impl PromptedActionsPlugin {
    fn command_now(&self, cx: &Call, context: &NativeCommandContext) -> author::Result<i32> {
        if cx
            .cancellation()
            .is_some_and(bmux_plugin_sdk::ServiceCancellation::is_cancelled)
        {
            return Err(PluginCommandError::unavailable(
                "prompted action invocation cancelled",
            ));
        }
        let entry = self.binding.enter(
            cx.invocation()
                .and_then(|call| call.callback_execution.as_ref()),
        )?;
        let result = self.run(context);
        drop(entry);
        result
    }
}

/// Define a frontend whose channels bind at activation time.
///
/// The trusted host must create both ports from the activation belonging to
/// this lifecycle context, retain its inboxes, and never select global channels.
/// # Panics
/// Panics if the embedded plugin declaration is rejected.
#[must_use]
pub fn plugin_definition_with_activation_ports(
    bind: impl Fn(
        &bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> Result<
        (
            bmux_plugin::prompt::PromptPort,
            bmux_plugin::action_dispatch::ActionDispatchPort,
        ),
        PluginCommandError,
    > + Send
    + Sync
    + 'static,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    let bind: activation_ports::Binder = std::sync::Arc::new(bind);
    bmux_plugin_sdk::author::definition::<PromptedActionsPlugin>(
        crate::declaration::PLUGIN_MANIFEST,
        crate::declaration::SERVICE_CONTRACTS,
        move |scope| {
            Ok(PromptedActionsPlugin {
                binding: activation_ports::Binding::deferred(scope.clone(), bind.clone()),
                tasks: std::sync::Mutex::default(),
            })
        },
    )
    .expect("embedded plugin declaration must be valid")
}

/// Retain compatibility with the create-context injector signature.
///
/// Callers must supply the exact activation's channels. The injector now runs
/// at activation, never at creation.
#[must_use]
pub fn plugin_definition_with_ports(
    inject: impl Fn(
        bmux_plugin_sdk::definition::PluginCreateContext,
    ) -> Result<
        (
            bmux_plugin::prompt::PromptPort,
            bmux_plugin::action_dispatch::ActionDispatchPort,
        ),
        PluginCommandError,
    > + Send
    + Sync
    + 'static,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    plugin_definition_with_activation_ports(move |context| inject(context.scope.clone()))
}

bmux_plugin_sdk::plugin!(
    PromptedActionsPlugin,
    create = |scope| Ok(PromptedActionsPlugin {
        binding: activation_ports::Binding::from_activation(scope.clone()),
        tasks: std::sync::Mutex::default(),
    })
);

#[cfg(test)]
mod definition_tests {
    #[test]
    fn definition_preserves_manifest_and_frontend_role() {
        let definition = super::plugin_definition();
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(crate::declaration::PLUGIN_MANIFEST)
                .unwrap();
        assert_eq!(definition.metadata.plugin_id, manifest.id);
        let embedded = bmux_plugin::PluginManifest::from_toml_str(
            definition.metadata.manifest_toml.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(
            (embedded.id, embedded.version, embedded.component_roles),
            (
                manifest.id.clone(),
                manifest.version,
                manifest.component_roles
            )
        );
        assert_eq!(
            definition.metadata.roles,
            vec![bmux_plugin_sdk::definition::ComponentRole::Frontend]
        );
    }

    #[tokio::test]
    async fn dropping_one_frontend_aborts_only_its_prompt_flow() {
        let first = super::PromptedActionsPlugin::default();
        let second = super::PromptedActionsPlugin::default();
        let first_task = first
            .tasks
            .lock()
            .unwrap()
            .spawn(std::future::pending::<()>());
        let second_task = second
            .tasks
            .lock()
            .unwrap()
            .spawn(std::future::pending::<()>());
        drop(first);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !first_task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!second_task.is_finished());
        drop(second);
    }
}

// Dynamic symbols belong to the X1 packaging target, not this implementation.

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
