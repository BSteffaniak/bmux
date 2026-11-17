//! Activation-time recording configuration, with no process-global lookup.
use super::{RecordingActivationConfiguration, RecordingPlugin};
use bmux_plugin_sdk::PluginCommandError;
use bmux_plugin_sdk::definition::{
    ActivationId, PluginCreateContext, PluginDefinition, PluginHostServices,
    PluginLifecycleContext, PluginResult,
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

type Configuration = RwLock<Option<RecordingActivationConfiguration>>;

/// The activation binding of a scoped recording instance: configuration is
/// resolved through the actual activation's host services, and commands and
/// services are refused until that activation completed.
pub(super) struct Scoped {
    created: PluginCreateContext,
    bind: Binder,
    active: std::sync::atomic::AtomicBool,
    activation: Mutex<Option<ActivationId>>,
}

fn not_ready() -> PluginCommandError {
    PluginCommandError::unavailable("recording activation is not ready or requires cleanup")
}

impl Scoped {
    /// Bind this activation's configuration before the owner is activated.
    pub(super) fn bind(
        &self,
        context: &PluginLifecycleContext,
        configuration: &Configuration,
    ) -> PluginResult<()> {
        if context.scope != self.created {
            return Err(PluginCommandError::unavailable(
                "recording activation scope mismatch",
            ));
        }
        let services = context.native.host_service_port.as_ref().ok_or_else(|| {
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
        }
        let selected = (self.bind)(context, services)?;
        if !selected.config.recordings_dir.is_absolute()
            || !selected.config.rolling_recordings_dir.is_absolute()
            || selected.process_caller.is_none()
        {
            return Err(PluginCommandError::failed(
                "recording roots must be explicitly selected absolute paths",
            ));
        }
        let mut slot = configuration
            .write()
            .map_err(|_| PluginCommandError::failed("recording configuration poisoned"))?;
        if slot.is_some() {
            return Err(PluginCommandError::unavailable(
                "recording activation already bound or cleanup required",
            ));
        }
        // Kept on a later activation failure: partially initialized
        // writers/sinks still require the same authority for cleanup.
        *slot = Some(selected);
        Ok(())
    }

    /// The owner activated: admit commands and services.
    pub(super) fn mark_active(&self) {
        self.active
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Refuse work before activation completed or after cleanup began.
    pub(super) fn ready(&self) -> PluginResult<()> {
        if self.active.load(std::sync::atomic::Ordering::Acquire) {
            Ok(())
        } else {
            Err(not_ready())
        }
    }

    /// Start cleanup of exactly the bound activation.
    pub(super) fn begin_cleanup(&self, context: &PluginLifecycleContext) -> PluginResult<()> {
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
        Ok(())
    }

    /// The owner deactivated: release the activation's configuration.
    pub(super) fn finish_cleanup(&self, configuration: &Configuration) -> PluginResult<()> {
        configuration
            .write()
            .map_err(|_| PluginCommandError::failed("recording configuration poisoned"))?
            .take();
        self.activation
            .lock()
            .map_err(|_| PluginCommandError::failed("recording activation poisoned"))?
            .take();
        Ok(())
    }
}

/// Typed services need the activation's configuration.
pub(super) fn require_configuration(configuration: &Configuration) -> PluginResult<()> {
    if configuration
        .read()
        .map_err(|_| PluginCommandError::failed("recording configuration poisoned"))?
        .is_none()
    {
        return Err(PluginCommandError::unavailable(
            "recording service registration requires activation configuration",
        ));
    }
    Ok(())
}

/// Create an inert recording definition, binding product configuration only when
/// the loader supplies actual activation-scoped host services.
///
/// # Panics
/// Panics if the embedded manifest is rejected.
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
    bmux_plugin_sdk::author::definition::<RecordingPlugin>(
        crate::declaration::PLUGIN_MANIFEST,
        crate::declaration::SERVICE_CONTRACTS,
        move |created: &PluginCreateContext| {
            Ok(RecordingPlugin {
                binding: Some(Arc::new(RwLock::new(None))),
                scoped: Some(Scoped {
                    created: created.clone(),
                    bind: bind.clone(),
                    active: std::sync::atomic::AtomicBool::new(false),
                    activation: Mutex::new(None),
                }),
                ..RecordingPlugin::default()
            })
        },
    )
    .expect("embedded recording declaration must be valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::NativeCommandInvocationSource;
    use bmux_plugin_sdk::definition::PluginCallContext;
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
                    callback_execution: None,
                    invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                    cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
                    host_services: None,
                },
                context,
            )
            .await;
        assert_ne!(result.exit_code, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unbound_scoped_instance_refuses_work() {
        let scoped = Scoped {
            created: PluginCreateContext {
                runtime_scope: bmux_plugin_sdk::definition::RuntimeScopeId::default(),
                instance: bmux_plugin_sdk::definition::LoadedInstanceId::from_uuid(
                    uuid::Uuid::from_u128(0x92),
                ),
                role: bmux_plugin_sdk::definition::ComponentRole::Backend,
            },
            bind: Arc::new(|_, _| Err(PluginCommandError::unavailable("unused"))),
            active: std::sync::atomic::AtomicBool::new(false),
            activation: Mutex::new(None),
        };
        assert!(scoped.ready().unwrap_err().message.contains("not ready"));
        assert!(require_configuration(&RwLock::new(None)).is_err());
    }
}
