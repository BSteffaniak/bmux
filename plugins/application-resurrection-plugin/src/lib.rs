//! Plugin-owned application resurrection lifecycle and local host transport.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

pub mod execution;
#[cfg(unix)]
mod host;
#[cfg(unix)]
mod process;
mod recovery;
/// Local shell lifecycle callbacks using the ordinary BMUX executable.
#[cfg(unix)]
pub mod shell;

#[cfg(unix)]
use bmux_application_resurrection_plugin_api::{resurrection_host_v1, resurrection_launch_v1};
use bmux_plugin_sdk::{
    NativeLifecycleContext, NativeServiceContext, PluginCommandError, RustPlugin, ServiceResponse,
};

/// Statically bundleable, independently disableable host adapter.
#[derive(Default)]
pub struct ApplicationResurrectionPlugin {
    #[cfg(unix)]
    host: Option<host::Host>,
}

impl RustPlugin for ApplicationResurrectionPlugin {
    type Contract = bmux_application_resurrection_plugin_api::Contract;

    #[cfg(unix)]
    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        self.host = Some(
            host::Host::new(
                std::path::PathBuf::from(&context.connection.state_dir)
                    .join("application-resurrection"),
            )
            .map_err(PluginCommandError::failed)?,
        );
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    #[cfg(not(unix))]
    fn activate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn deactivate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        #[cfg(unix)]
        {
            self.host = None;
        }
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        #[cfg(unix)]
        {
            let host = self.host.as_ref();
            let unavailable = || resurrection_launch_v1::HostError::Failed {
                message: "host unavailable".into(),
            };
            bmux_plugin_sdk::route_service!(context, {
                resurrection_host_v1::client::StatusEndpoint => |_request, _context| {
                    Ok(resurrection_host_v1::HostStatus { protocol: "application-resurrection/1".into(), available: host.is_some(), reason: None })
                },
                resurrection_launch_v1::client::PrepareEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.prepare(&request.key, request.restored, request.shell, request.replay).map_err(host_error)))
                },
                bmux_application_resurrection_plugin_api::resurrection_launch_v2::client::PrepareEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.prepare_checked(&request.key, request.restored, request.shell, request.replay, host::IdentityCheck::Expected(request.expected_invocation.as_deref())).map_err(host_error)))
                },
                bmux_application_resurrection_plugin_api::resurrection_launch_v2::client::BoundaryEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.boundary_identity(&request.key, &request.endpoint, request.prompt).map_err(host_error)))
                },
                bmux_application_resurrection_plugin_api::resurrection_launch_v2::client::TakeReplayEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.take_replay(&request.key, &request.endpoint).map_err(host_error)))
                },
                resurrection_launch_v1::client::BindEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.bind(&request.key, &request.endpoint, request.pid).map_err(host_error)))
                },
                resurrection_launch_v1::client::BoundaryEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.boundary(&request.key, &request.endpoint, request.prompt).map_err(host_error)))
                },
                resurrection_launch_v1::client::CloseEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.close(&request.key, &request.endpoint).map_err(host_error)))
                },
            })
        }
        #[cfg(not(unix))]
        {
            let _ = context;
            ServiceResponse::error("unsupported", "local transport unavailable")
        }
    }
}

#[cfg(unix)]
fn host_error(message: String) -> resurrection_launch_v1::HostError {
    resurrection_launch_v1::HostError::Failed { message }
}

bmux_plugin_sdk::export_plugin!(
    ApplicationResurrectionPlugin,
    include_str!("../plugin.toml")
);
