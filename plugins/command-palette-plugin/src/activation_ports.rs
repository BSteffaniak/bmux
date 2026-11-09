//! Private frontend activation binding. The host selects real activation-owned
//! channels; serializable identities are preconditions, never capabilities.
use std::sync::{Arc, Mutex};

use bmux_plugin::{action_dispatch::ActionDispatchPort, prompt::PromptPort};
use bmux_plugin_sdk::PluginCommandError;
use bmux_plugin_sdk::definition::{
    ActivationId, ComponentRole, PluginCreateContext, PluginLifecycleContext,
};

pub type Ports = (PromptPort, ActionDispatchPort);
pub type Binder =
    Arc<dyn Fn(&PluginLifecycleContext) -> Result<Ports, PluginCommandError> + Send + Sync>;

#[derive(Clone, PartialEq, Eq)]
struct Key {
    scope: PluginCreateContext,
    activation: ActivationId,
}

impl From<&PluginLifecycleContext> for Key {
    fn from(context: &PluginLifecycleContext) -> Self {
        Self {
            scope: context.scope.clone(),
            activation: context.activation,
        }
    }
}

#[derive(Default)]
enum Phase {
    #[default]
    Inactive,
    Binding(Key),
    Active(Key, Ports),
    Closing(Key),
    Closed(Key),
}

#[derive(Default)]
pub struct Binding {
    expected: Option<PluginCreateContext>,
    binder: Option<Binder>,
    phase: Mutex<Phase>,
}

fn unavailable(message: &str) -> PluginCommandError {
    PluginCommandError::unavailable(message)
}

impl Binding {
    pub fn unbound(scope: PluginCreateContext) -> Self {
        Self {
            expected: Some(scope),
            binder: None,
            phase: Mutex::default(),
        }
    }

    pub fn deferred(scope: PluginCreateContext, binder: Binder) -> Self {
        Self {
            expected: Some(scope),
            binder: Some(binder),
            phase: Mutex::default(),
        }
    }

    pub fn fixed(prompt: PromptPort, actions: ActionDispatchPort) -> Self {
        let ports = Mutex::new(Some((prompt, actions)));
        Self {
            expected: None,
            binder: Some(Arc::new(move |_| {
                ports
                    .lock()
                    .map_err(|_| unavailable("frontend binding requires recovery"))?
                    .take()
                    .ok_or_else(|| {
                        unavailable("fixed frontend ports cannot be reused for another activation")
                    })
            })),
            phase: Mutex::default(),
        }
    }

    pub fn activate(&self, context: &PluginLifecycleContext) -> Result<(), PluginCommandError> {
        if context.scope.role != ComponentRole::Frontend
            || self
                .expected
                .as_ref()
                .is_some_and(|scope| scope != &context.scope)
        {
            return Err(unavailable(
                "frontend activation does not match created instance",
            ));
        }
        let binder = self
            .binder
            .as_ref()
            .ok_or_else(|| unavailable("activation-bound frontend ports not supplied"))?;
        let key = Key::from(context);
        {
            let mut phase = self
                .phase
                .lock()
                .map_err(|_| unavailable("frontend binding requires recovery"))?;
            match &*phase {
                Phase::Active(current, _) if current == &key => return Ok(()),
                Phase::Binding(_) | Phase::Active(_, _) | Phase::Closing(_) => {
                    return Err(unavailable("frontend activation already admitted"));
                }
                Phase::Closed(current) if current == &key => {
                    return Err(unavailable("frontend activation is retired"));
                }
                Phase::Inactive | Phase::Closed(_) => {}
            }
            *phase = Phase::Binding(key.clone());
        }
        // The host may reenter or fail. Never call it under the binding lock.
        let result = binder(context);
        let mut phase = self
            .phase
            .lock()
            .map_err(|_| unavailable("frontend binding requires recovery"))?;
        if !matches!(&*phase, Phase::Binding(current) if current == &key) {
            return Err(unavailable("frontend activation closed while binding"));
        }
        let result = match result {
            Ok(ports) => {
                *phase = Phase::Active(key, ports);
                Ok(())
            }
            Err(error) => {
                *phase = Phase::Closed(key);
                Err(error)
            }
        };
        drop(phase);
        result
    }

    pub fn ports(&self) -> Result<Ports, PluginCommandError> {
        let phase = self
            .phase
            .lock()
            .map_err(|_| unavailable("frontend binding requires recovery"))?;
        let result = match &*phase {
            Phase::Active(_, ports) => Ok(ports.clone()),
            _ => Err(unavailable("frontend activation ports are not active")),
        };
        drop(phase);
        result
    }

    /// The host closes admissions before this hook, then the plugin drains its
    /// tasks. A stale teardown must not revoke a later activation's channels.
    pub fn close(&self, context: &PluginLifecycleContext) -> Result<(), PluginCommandError> {
        if context.scope.role != ComponentRole::Frontend
            || self
                .expected
                .as_ref()
                .is_some_and(|scope| scope != &context.scope)
        {
            return Err(unavailable(
                "frontend teardown does not match created instance",
            ));
        }
        let key = Key::from(context);
        let mut phase = self
            .phase
            .lock()
            .map_err(|_| unavailable("frontend binding requires recovery"))?;
        match &*phase {
            Phase::Binding(current)
            | Phase::Active(current, _)
            | Phase::Closing(current)
            | Phase::Closed(current)
                if current != &key =>
            {
                return Err(unavailable("frontend teardown does not match activation"));
            }
            _ => {}
        }
        *phase = Phase::Closing(key);
        drop(phase);
        Ok(())
    }

    pub fn drained(&self, context: &PluginLifecycleContext) -> Result<(), PluginCommandError> {
        let key = Key::from(context);
        let mut phase = self
            .phase
            .lock()
            .map_err(|_| unavailable("frontend binding requires recovery"))?;
        if !matches!(&*phase, Phase::Closing(current) if current == &key) {
            return Err(unavailable(
                "frontend drain does not match closing activation",
            ));
        }
        *phase = Phase::Closed(key);
        drop(phase);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};
    use bmux_plugin_sdk::{ApiVersion, HostConnectionInfo, HostMetadata, NativeLifecycleContext};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn lifecycle(scope: PluginCreateContext, activation: ActivationId) -> PluginLifecycleContext {
        PluginLifecycleContext {
            callback_execution: None,
            capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
            scope,
            activation,
            native: NativeLifecycleContext {
                plugin_id: "test.frontend".into(),
                required_capabilities: Vec::new(),
                provided_capabilities: Vec::new(),
                services: Vec::new(),
                available_capabilities: Vec::new(),
                enabled_plugins: Vec::new(),
                plugin_search_roots: Vec::new(),
                registered_plugins: Vec::new(),
                host: HostMetadata {
                    product_name: "test".into(),
                    product_version: "test".into(),
                    plugin_api_version: ApiVersion::new(1, 1),
                    plugin_abi_version: ApiVersion::new(1, 1),
                },
                connection: HostConnectionInfo {
                    config_dir: String::new(),
                    config_dir_candidates: Vec::new(),
                    runtime_dir: String::new(),
                    data_dir: String::new(),
                    state_dir: String::new(),
                },
                settings: None,
                plugin_settings_map: std::collections::BTreeMap::new(),
                host_kernel_bridge: None,
                auxiliary_executables: std::collections::BTreeMap::new(),
                host_service_port: None,
                auxiliary_executable_errors: std::collections::BTreeMap::new(),
            },
        }
    }

    #[tokio::test]
    async fn binding_is_deferred_idempotent_owner_checked_and_revoked() {
        let instance = InstanceScope::new(PluginRuntimeScopeId::default());
        let owner = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        owner.mark_active().unwrap();
        let scope = PluginCreateContext {
            runtime_scope: owner.runtime_id(),
            instance: owner.instance_id(),
            role: ComponentRole::Frontend,
        };
        let context = lifecycle(scope.clone(), owner.id());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let inboxes = Arc::new(Mutex::new(None));
        let retained = inboxes.clone();
        let binding = Binding::deferred(
            scope,
            Arc::new(move |context| {
                assert_eq!(context.activation, owner.id());
                assert_eq!(context.scope.instance, owner.instance_id());
                observed.fetch_add(1, Ordering::SeqCst);
                let (prompt, prompts) = PromptPort::channel(owner.clone(), 2)
                    .map_err(|error| unavailable(&error.to_string()))?;
                let (actions, action_inbox) = ActionDispatchPort::channel(owner.clone(), 2)
                    .map_err(|error| unavailable(&error.to_string()))?;
                *retained.lock().unwrap() = Some((prompts, action_inbox));
                Ok((prompt, actions))
            }),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(binding.ports().is_err());
        binding.activate(&context).unwrap();
        binding.activate(&context).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let (prompt, actions) = binding.ports().unwrap();
        let response = prompt
            .submit(bmux_plugin_sdk::PromptRequest::confirm("owned"))
            .unwrap();
        let (mut prompts, mut action_inbox) = inboxes.lock().unwrap().take().unwrap();
        prompts
            .recv()
            .await
            .unwrap()
            .respond(bmux_plugin_sdk::PromptResponse::Cancelled)
            .unwrap();
        assert_eq!(
            response.await.unwrap(),
            bmux_plugin_sdk::PromptResponse::Cancelled
        );
        actions.dispatch("focus_next_pane").unwrap();
        assert_eq!(action_inbox.recv().await.unwrap().action, "focus_next_pane");
        let mut foreign = context.clone();
        foreign.scope.role = ComponentRole::Backend;
        assert!(binding.close(&foreign).is_err());
        assert!(binding.ports().is_ok());
        binding.close(&context).unwrap();
        assert!(binding.ports().is_err());
        assert!(binding.activate(&context).is_err());
        binding.drained(&context).unwrap();
        assert!(binding.activate(&context).is_err());
    }

    #[tokio::test]
    async fn author_factory_does_not_bind_until_activation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let definition = super::super::plugin_definition_with_activation_ports(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            Err(unavailable("selected test host has no frontend"))
        });
        let instance = InstanceScope::new(PluginRuntimeScopeId::default());
        let owner = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        let scope = PluginCreateContext {
            runtime_scope: owner.runtime_id(),
            instance: owner.instance_id(),
            role: ComponentRole::Frontend,
        };
        let plugin = definition.create(scope.clone()).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(plugin.activate(lifecycle(scope, owner.id())).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn missing_activation_port_is_an_error_not_a_global_fallback() {
        assert!(Binding::default().ports().is_err());
    }
}
