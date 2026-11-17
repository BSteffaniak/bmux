//! Managed execution owner. The selected native BMUX executable is the actual
//! executor, with a private generation-fenced control/snapshot channel. stdout
//! is diagnostics only, never a display transport or lifecycle acknowledgement.
/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_playbook_plugin_api::export_schema,
            crate_path: ::bmux_playbook_plugin_api,
        },
        settings: "plugin.toml",
    }
}

mod runner;

use std::sync::{Arc, Mutex};

use bmux_plugin_sdk::author::{Declarations, Plugin};
use bmux_plugin_sdk::prelude::{NativeServiceContext, PluginCommandError, ServiceResponse};

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

// The owner, its poller and every service body are synchronous: they run
// on the host's blocking pool inside the call's callback entry.
impl Plugin for PlaybookPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_activate(|plugin, _, _| plugin.start_poller())
            .blocking_deactivate(|plugin, _| plugin.drain())
            .blocking_services(Self::service, None);
    }
}

impl PlaybookPlugin {
    fn service(&self, context: &NativeServiceContext) -> ServiceResponse {
        // Endpoints come from the BPDL world's provided interfaces, so a route
        // the world does not declare fails to compile.
        use crate::declaration::provides::{playbook_commands, playbook_state};
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

bmux_plugin_sdk::plugin!(PlaybookPlugin);

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

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
