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
}

impl RustPlugin for PlaybookPlugin {
    type Contract = bmux_playbook_plugin_api::Contract;

    fn activate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        let owner = Arc::downgrade(&self.runner);
        std::thread::spawn(move || {
            while let Some(owner) = owner.upgrade() {
                if let Ok(mut owner) = owner.lock() {
                    owner.poll();
                }
                drop(owner);
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        });
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(&mut self, _context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
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

bmux_plugin_sdk::export_plugin!(PlaybookPlugin, include_str!("../plugin.toml"));
