//! Host-owned frontend instances.
//!
//! A host program (for example one attachment) declares what it offers its
//! frontends' activations through one trusted `ActivationBinder`. Each
//! frontend-only plugin the host runs gets exactly one instance, activated
//! once with that binder and kept for the host's lifetime, so work it starts
//! from a command (an open prompt awaiting the user) outlives that command.
//! The owner deactivates every instance when it ends; whatever the binder
//! minted from those activations is revoked with them.
//!
//! Dynamic and process-packaged frontends cannot receive in-process activation
//! values, so they are refused explicitly instead of silently running with a
//! command-local lifetime and no capabilities.
#[cfg(test)]
#[path = "owned_frontends_tests.rs"]
mod tests;
use super::{ProductionPlugin, RuntimeCommandState};
use anyhow::{Context, Result};
use bmux_plugin_sdk::NativeCommandContext;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub struct OwnedFrontends {
    binder: Arc<bmux_plugin::ActivationBinder>,
    host_handles: Option<HostHandles>,
    instances: Mutex<BTreeMap<String, ProductionPlugin>>,
}

/// The owner's event bus and plugin state registry, offered to each frontend
/// activation as scoped views (as the server does for its plugins).
struct HostHandles {
    events: Arc<bmux_plugin::EventBus>,
    states: Arc<bmux_plugin::PluginStateRegistry>,
}

impl OwnedFrontends {
    #[must_use]
    pub fn new(binder: Arc<bmux_plugin::ActivationBinder>) -> Self {
        Self {
            binder,
            host_handles: None,
            instances: Mutex::new(BTreeMap::new()),
        }
    }

    /// Also offer each frontend activation scoped views of the owner's event
    /// bus and plugin state registry.
    #[must_use]
    pub fn with_host_handles(
        mut self,
        events: Arc<bmux_plugin::EventBus>,
        states: Arc<bmux_plugin::PluginStateRegistry>,
    ) -> Self {
        self.host_handles = Some(HostHandles { events, states });
        self
    }

    /// The owner's activated instance of `plugin`, created on first use.
    /// Returns `None` when `plugin` is not a frontend-only component, which
    /// keeps its ordinary execution path.
    ///
    /// # Errors
    /// Loading, binding (including refusal of dynamic frontends) and
    /// activation failures; a failed instance is never cached.
    pub(super) fn instance(
        &self,
        plugin: &bmux_plugin::RegisteredPlugin,
        state: &RuntimeCommandState,
        context: &NativeCommandContext,
    ) -> Result<Option<ProductionPlugin>> {
        if !matches!(
            super::deployed_component_role(&plugin.manifest),
            Ok(bmux_plugin_sdk::definition::ComponentRole::Frontend)
        ) {
            return Ok(None);
        }
        let id = plugin.declaration.id.as_str();
        // Commands from one owner are issued serially; holding the lock across
        // creation keeps one activation per plugin without a second owner.
        let mut instances = self
            .instances
            .lock()
            .map_err(|_| anyhow::anyhow!("owned frontend instances poisoned"))?;
        if let Some(existing) = instances.get(id) {
            return Ok(Some(existing.clone()));
        }
        let definitions = super::standalone_provider_definitions(state)?;
        let loaded = super::load_plugin_with_definitions(
            plugin,
            &super::plugin_host_metadata(),
            &state.available_capability_providers,
            &state.config,
            &definitions,
            state,
            None,
        )
        .with_context(|| format!("failed loading frontend plugin '{id}'"))?;
        loaded
            .bind_activation_binder(self.binder.clone())
            .with_context(|| {
                format!(
                    "frontend plugin '{id}' cannot receive this host's activation capabilities \
                     (only bundled in-process frontends can)"
                )
            })?;
        if let Some(handles) = &self.host_handles {
            loaded
                .bind_host_handles(&handles.events, &handles.states)
                .with_context(|| {
                    format!("frontend plugin '{id}' cannot receive this host's handles")
                })?;
        }
        if let Err(error) = loaded.activate_owned(context) {
            // Settle the failed activation's cleanup on this blocking worker so
            // it does not pin the runtime; report the activation failure.
            let lifecycle = super::command_lifecycle_context(state, &loaded.declaration);
            if let Err(cleanup) = super::settle_command_cleanup(loaded.deactivate_async(&lifecycle))
            {
                tracing::warn!(plugin_id = %id, %cleanup, "failed frontend activation cleanup");
            }
            return Err(anyhow::Error::from(error))
                .with_context(|| format!("frontend plugin '{id}' activation failed"));
        }
        instances.insert(id.to_string(), loaded.clone());
        drop(instances);
        Ok(Some(loaded))
    }

    /// Deactivate every owned instance, releasing their activation capabilities.
    ///
    /// # Errors
    /// Returns the first deactivation failure after attempting all instances.
    pub async fn deactivate_all(&self, state: &RuntimeCommandState) -> Result<()> {
        let instances = std::mem::take(
            &mut *self
                .instances
                .lock()
                .map_err(|_| anyhow::anyhow!("owned frontend instances poisoned"))?,
        );
        let mut first_error = None;
        for (id, instance) in instances {
            let lifecycle = super::command_lifecycle_context(state, &instance.declaration);
            if let Err(error) = instance.deactivate_async(&lifecycle).await {
                tracing::warn!(plugin_id = %id, %error, "owned frontend deactivation failed");
                // Revoke what deactivation could not settle.
                let _ = instance.cancel_background_tasks();
                first_error.get_or_insert_with(|| {
                    anyhow::anyhow!("frontend plugin '{id}' deactivation failed: {error}")
                });
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    pub(super) fn contains(&self, plugin_id: &str) -> bool {
        self.instances
            .lock()
            .is_ok_and(|instances| instances.contains_key(plugin_id))
    }
}

impl Drop for OwnedFrontends {
    /// An owner that ended without graceful deactivation still closes each
    /// activation, revoking every capability minted from it.
    fn drop(&mut self) {
        if let Ok(instances) = self.instances.get_mut() {
            for instance in instances.values() {
                let _ = instance.cancel_background_tasks();
            }
        }
    }
}
