//! Activation-time recording configuration, with no process-global lookup.
use super::{RecordingActivationConfiguration, RecordingPlugin, definition_metadata};
use bmux_plugin_sdk::definition::{
    CommandOutcome, PluginCallContext, PluginCreateContext, PluginDefinition, PluginFuture,
    PluginHostServices, PluginInstance, PluginLifecycleContext, PluginResult, RustPluginAdapter,
};
use bmux_plugin_sdk::{
    NativeCommandContext, NativeServiceContext, PluginCommandError, ServiceResponse,
    TypedServiceRegistrationContext, TypedServiceRegistry,
};
use std::sync::{Arc, Mutex, RwLock};

type Binder = Arc<
    dyn Fn(
            &PluginLifecycleContext,
            &PluginHostServices,
        ) -> PluginResult<RecordingActivationConfiguration>
        + Send
        + Sync,
>;

struct Instance {
    created: PluginCreateContext,
    bind: Binder,
    configuration: Arc<RwLock<Option<RecordingActivationConfiguration>>>,
    adapter: RustPluginAdapter<RecordingPlugin>,
    active: std::sync::atomic::AtomicBool,
    activation: Mutex<Option<bmux_plugin_sdk::definition::ActivationId>>,
}

impl PluginInstance for Instance {
    fn activate_scoped(
        &self,
        context: PluginLifecycleContext,
        services: Option<PluginHostServices>,
    ) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            if context.scope != self.created {
                return Err(PluginCommandError::unavailable(
                    "recording activation scope mismatch",
                ));
            }
            let services = services.ok_or_else(|| {
                PluginCommandError::unavailable("recording activation host services not bound")
            })?;
            {
                let mut activation = self
                    .activation
                    .lock()
                    .map_err(|_| PluginCommandError::failed("recording activation poisoned"))?;
                if activation.is_some() {
                    return Err(PluginCommandError::unavailable(
                        "recording activation already bound or cleanup required",
                    ));
                }
                *activation = Some(context.activation);
                drop(activation);
            }
            let configuration = (self.bind)(&context, &services)?;
            if !configuration.config.recordings_dir.is_absolute()
                || !configuration.config.rolling_recordings_dir.is_absolute()
                || configuration.process_caller.is_none()
            {
                return Err(PluginCommandError::failed(
                    "recording roots must be explicitly selected absolute paths",
                ));
            }
            {
                let mut slot = self
                    .configuration
                    .write()
                    .map_err(|_| PluginCommandError::failed("recording configuration poisoned"))?;
                if slot.is_some() {
                    return Err(PluginCommandError::unavailable(
                        "recording activation already bound or cleanup required",
                    ));
                }
                *slot = Some(configuration);
                drop(slot);
            }
            // Keep configuration on failure: partially initialized writers/sinks
            // still require the same authority for cleanup. No silent reset.
            self.adapter
                .activate_scoped(context, Some(services))
                .await?;
            self.active
                .store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        })
    }

    fn activate(&self, _context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async {
            Err(PluginCommandError::unavailable(
                "recording requires scoped activation host services",
            ))
        })
    }

    fn deactivate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        self.deactivate_scoped(context, None)
    }

    fn deactivate_scoped(
        &self,
        context: PluginLifecycleContext,
        services: Option<PluginHostServices>,
    ) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            if context.scope != self.created {
                return Err(PluginCommandError::unavailable(
                    "recording cleanup scope mismatch",
                ));
            }
            if *self
                .activation
                .lock()
                .map_err(|_| PluginCommandError::failed("recording activation poisoned"))?
                != Some(context.activation)
            {
                return Err(PluginCommandError::unavailable(
                    "recording cleanup activation mismatch",
                ));
            }
            self.active
                .store(false, std::sync::atomic::Ordering::Release);
            self.adapter.deactivate_scoped(context, services).await?;
            self.configuration
                .write()
                .map_err(|_| PluginCommandError::failed("recording configuration poisoned"))?
                .take();
            self.activation
                .lock()
                .map_err(|_| PluginCommandError::failed("recording activation poisoned"))?
                .take();
            Ok(())
        })
    }

    fn command(
        &self,
        call: PluginCallContext,
        context: NativeCommandContext,
    ) -> PluginFuture<'_, CommandOutcome> {
        if !self.active.load(std::sync::atomic::Ordering::Acquire) {
            return Box::pin(async {
                Err(PluginCommandError::unavailable(
                    "recording activation is not ready or requires cleanup",
                ))
                .into()
            });
        }
        self.adapter.command(call, context)
    }
    fn invoke(
        &self,
        call: PluginCallContext,
        context: NativeServiceContext,
    ) -> PluginFuture<'_, PluginResult<ServiceResponse>> {
        if !self.active.load(std::sync::atomic::Ordering::Acquire) {
            return Box::pin(async {
                Err(PluginCommandError::unavailable(
                    "recording activation is not ready or requires cleanup",
                ))
            });
        }
        self.adapter.invoke(call, context)
    }
    fn service_contract_for(&self, interface: &str) -> Option<&'static [u8]> {
        self.adapter.service_contract_for(interface)
    }
    fn register_typed_services_with_context(
        &self,
        context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) -> PluginResult<()> {
        if self
            .configuration
            .read()
            .map_err(|_| PluginCommandError::failed("recording configuration poisoned"))?
            .is_none()
        {
            return Err(PluginCommandError::unavailable(
                "recording service registration requires activation configuration",
            ));
        }
        self.adapter
            .register_typed_services_with_context(context, registry)
    }
    fn portable_services(
        &self,
    ) -> PluginResult<Vec<Arc<dyn bmux_plugin_sdk::PortableServiceProvider>>> {
        self.adapter.portable_services()
    }
}

/// Create an inert recording definition, binding product configuration only when
/// the loader supplies actual activation-scoped host services.
#[must_use]
pub fn definition(
    bind: impl Fn(
        &PluginLifecycleContext,
        &PluginHostServices,
    ) -> PluginResult<RecordingActivationConfiguration>
    + Send
    + Sync
    + 'static,
) -> PluginDefinition {
    let bind: Binder = Arc::new(bind);
    PluginDefinition::new(
        definition_metadata(),
        move |created: PluginCreateContext| {
            let configuration = Arc::new(RwLock::new(None));
            let plugin = RecordingPlugin {
                owner: RwLock::new(None),
                binding: Some(configuration.clone()),
            };
            let instance = created.instance;
            Ok(Arc::new(Instance {
                created,
                bind: bind.clone(),
                configuration,
                adapter: RustPluginAdapter::for_instance(plugin, instance),
                active: std::sync::atomic::AtomicBool::new(false),
                activation: Mutex::new(None),
            }) as Arc<dyn PluginInstance>)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::NativeCommandInvocationSource;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn selected_command_cannot_run_or_resolve_config_before_activation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let definition = definition(move |_, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            Err(PluginCommandError::unavailable(
                "test authority unavailable",
            ))
        });
        let plugin = definition
            .create(PluginCreateContext {
                runtime_scope: bmux_plugin_sdk::definition::RuntimeScopeId::default(),
                instance: bmux_plugin_sdk::definition::LoadedInstanceId::from_uuid(
                    uuid::Uuid::from_u128(0x91),
                ),
                role: bmux_plugin_sdk::definition::ComponentRole::Frontend,
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let context = crate::tests::command_context(
            "recording-process-start",
            &["--session", "retired"],
            NativeCommandInvocationSource::Cli,
        );
        let result = plugin
            .command(
                PluginCallContext {
                    invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                    cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
                    host_services: None,
                },
                context,
            )
            .await;
        assert_ne!(result.exit_code, 0);
        assert!(result.error.unwrap().contains("not ready"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
