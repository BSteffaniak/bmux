//! Managed execution owner. The selected native BMUX executable is the actual
//! executor, with a private generation-fenced control/snapshot channel. stdout
//! is diagnostics only, never a display transport or lifecycle acknowledgement.
mod runner;

use std::sync::{Arc, Mutex};

use bmux_playbook_plugin_api::{playbook_commands, playbook_state};
use bmux_plugin_sdk::prelude::{
    NativeCommandContext, NativeLifecycleContext, NativeServiceContext, PluginCommandError,
    RustPlugin, ServiceResponse,
};

#[derive(Default)]
pub struct PlaybookPlugin {
    runner: Arc<Mutex<runner::Owner>>,
    poller: Mutex<Option<Poller>>,
}

struct Poller {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Drop for PlaybookPlugin {
    fn drop(&mut self) {
        // The host drains before destruction. A defensive drop must at least
        // close the weak-owner poll loop, without claiming runner cleanup.
        if let Ok(Some(poller)) = self.poller.get_mut() {
            poller
                .stop
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

impl RustPlugin for PlaybookPlugin {
    type Contract = bmux_playbook_plugin_api::Contract;

    fn activate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        self.start_poller()?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(&mut self, _context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn deactivate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        self.drain()?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        self.service(context)
    }
}

impl PlaybookPlugin {
    fn service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            playbook_commands::client::PrepareEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.prepare(req.input)))
            },
            playbook_commands::client::SetPlaybackSpeedEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.set_playback_speed(req.generation, &req.speed)))
            },
            playbook_commands::client::StartEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.start(req.generation)))
            },
            playbook_commands::client::PauseScriptEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.control(req.generation, runner::Command::Pause)))
            },
            playbook_commands::client::NextStepEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.control(req.generation, runner::Command::Step)))
            },
            playbook_commands::client::ResumeScriptEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.control(req.generation, runner::Command::Resume)))
            },
            playbook_commands::client::StopEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.control(req.generation, runner::Command::Stop)))
            },
            playbook_commands::client::RestartEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.restart(req.generation)))
            },
            playbook_state::client::LookupPreparationEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.lookup_preparation(&req.request_id)))
            },
            playbook_state::client::StatusEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.status(req.generation)))
            },
            playbook_state::client::WatchEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.watch(req.generation, req.after_revision)))
            },
            playbook_state::client::SettledEndpoint => |req, _ctx| {
                Ok(self.with_owner(|owner| owner.status(req.generation)))
            },
        })
    }
}

impl PlaybookPlugin {
    fn start_poller(&self) -> Result<(), PluginCommandError> {
        let mut poller = self
            .poller
            .lock()
            .map_err(|_| PluginCommandError::failed("playbook poller lock poisoned"))?;
        if poller.is_some() {
            return Err(PluginCommandError::failed("playbook already active"));
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let owner = Arc::downgrade(&self.runner);
        let thread = std::thread::Builder::new()
            .name("playbook-owner".into())
            .spawn(move || {
                while !stopped.load(std::sync::atomic::Ordering::Acquire) {
                    let Some(owner) = owner.upgrade() else {
                        break;
                    };
                    if let Ok(mut owner) = owner.lock() {
                        owner.poll();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            })
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        *poller = Some(Poller { stop, thread });
        Ok(())
    }

    fn drain(&self) -> Result<(), PluginCommandError> {
        self.with_owner(runner::Owner::request_shutdown)
            .map_err(|error| PluginCommandError::failed(format!("{error:?}")))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let settled = self
                .with_owner(runner::Owner::shutdown_settled)
                .map_err(|error| PluginCommandError::failed(format!("{error:?}")))?;
            if settled {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(PluginCommandError::failed(
                    "playbook cleanup required: runner exit and sandbox cleanup not acknowledged",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let poller = self
            .poller
            .lock()
            .map_err(|_| PluginCommandError::failed("playbook poller lock poisoned"))?
            .take();
        if let Some(poller) = poller {
            poller
                .stop
                .store(true, std::sync::atomic::Ordering::Release);
            poller
                .thread
                .join()
                .map_err(|_| PluginCommandError::failed("playbook poller panicked"))?;
        }
        Ok(())
    }

    fn with_owner<T>(
        &self,
        action: impl FnOnce(&mut runner::Owner) -> Result<T, String>,
    ) -> Result<T, bmux_playbook_plugin_api::playbook_types::PlaybookError> {
        self.runner
            .lock()
            .map_err(|_| "playbook owner lock poisoned".to_string())
            .and_then(|mut owner| action(&mut owner))
            .map_err(
                |message| bmux_playbook_plugin_api::playbook_types::PlaybookError::Rejected {
                    message,
                },
            )
    }
}

struct SelectedPlaybook {
    instance: bmux_plugin_sdk::definition::LoadedInstanceId,
    plugin: Arc<PlaybookPlugin>,
}
impl SelectedPlaybook {
    fn execute<T: Send + 'static>(
        &self,
        execution: Option<bmux_plugin_sdk::PluginCallbackContext>,
        work: impl FnOnce(&PlaybookPlugin) -> Result<T, PluginCommandError> + Send + 'static,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'static, Result<T, PluginCommandError>> {
        let instance = self.instance;
        let plugin = self.plugin.clone();
        Box::pin(async move {
            let execution = execution.ok_or_else(|| {
                PluginCommandError::unavailable("selected playbook callback authority missing")
            })?;
            let entry = execution.enter_adapter_entry(instance)?;
            let output = Arc::new(Mutex::new(None));
            let writer = output.clone();
            execution
                .execute_entered(
                    instance,
                    entry,
                    Box::new(move || {
                        let result = work(&plugin);
                        *writer.lock().map_err(|_| {
                            PluginCommandError::failed("playbook result poisoned")
                        })? = Some(result);
                        Ok(())
                    }),
                )
                .await?;
            let result = output
                .lock()
                .map_err(|_| PluginCommandError::failed("playbook result poisoned"))?
                .take();
            result.ok_or_else(|| {
                PluginCommandError::failed("playbook callback settled without result")
            })?
        })
    }
}
impl bmux_plugin_sdk::definition::PluginInstance for SelectedPlaybook {
    fn activate(
        &self,
        context: bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>>
    {
        self.execute(context.callback_execution, PlaybookPlugin::start_poller)
    }
    fn deactivate(
        &self,
        context: bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>>
    {
        self.execute(context.callback_execution, PlaybookPlugin::drain)
    }
    fn invoke(
        &self,
        call: bmux_plugin_sdk::definition::PluginCallContext,
        context: NativeServiceContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        bmux_plugin_sdk::definition::PluginResult<ServiceResponse>,
    > {
        self.execute(call.callback_execution, move |plugin| {
            if call.cancellation.is_cancelled() {
                return Err(PluginCommandError::unavailable(
                    "playbook invocation cancelled",
                ));
            }
            Ok(plugin.service(context))
        })
    }
}

bmux_plugin_sdk::define_plugin!(
    factory =
        |scope: bmux_plugin_sdk::definition::PluginCreateContext| Ok(Arc::new(SelectedPlaybook {
            instance: scope.instance,
            plugin: Arc::new(PlaybookPlugin::default()),
        })
            as Arc<dyn bmux_plugin_sdk::definition::PluginInstance>),
    metadata = definition_metadata(),
);

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn selected_adapter_rejects_missing_authority_without_starting_poller() {
        let selected = SelectedPlaybook {
            instance: bmux_plugin_sdk::definition::LoadedInstanceId::from_uuid(uuid::Uuid::new_v4()),
            plugin: Arc::new(PlaybookPlugin::default()),
        };
        let mut work = std::pin::pin!(selected.execute(None, PlaybookPlugin::start_poller));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            std::future::Future::poll(work.as_mut(), &mut context),
            std::task::Poll::Ready(Err(_))
        ));
        assert!(selected.plugin.poller.lock().unwrap().is_none());
    }

    #[test]
    fn poller_is_activation_owned_and_reactivation_is_explicit() {
        let plugin = PlaybookPlugin::default();
        plugin.start_poller().unwrap();
        assert!(plugin.start_poller().is_err());
        plugin.drain().unwrap();
        assert!(plugin.poller.lock().unwrap().is_none());
        plugin.start_poller().unwrap();
        plugin.drain().unwrap();
    }

    #[test]
    fn independent_instances_do_not_share_owner_or_tasks() {
        let first = PlaybookPlugin::default();
        let second = PlaybookPlugin::default();
        assert!(!Arc::ptr_eq(&first.runner, &second.runner));
        first.start_poller().unwrap();
        assert!(second.poller.lock().unwrap().is_none());
        first.drain().unwrap();
    }
}

fn definition_metadata() -> bmux_plugin_sdk::definition::PluginMetadata {
    bmux_plugin_sdk::definition::PluginDefinition::from_rust_plugin::<PlaybookPlugin>(
        include_str!("../plugin.toml"),
        vec![bmux_plugin_sdk::definition::ComponentRole::Backend],
    )
    .expect("plugin manifest must define valid canonical metadata")
    .metadata
}

#[cfg(test)]
mod canonical_metadata_tests {
    #[test]
    fn embedded_manifest_matches_definition_metadata() {
        let definition = super::plugin_definition();
        definition
            .validate()
            .expect("canonical embedded identity must validate");
    }
}
