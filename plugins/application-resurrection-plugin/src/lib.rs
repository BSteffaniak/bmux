//! Plugin-owned application resurrection lifecycle.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

pub mod execution;

use bmux_application_resurrection_plugin_api::resurrection_host_v1;
use bmux_plugin_sdk::{NativeServiceContext, RustPlugin, ServiceResponse};

/// Statically bundleable, independently disableable host adapter.
#[derive(Default)]
pub struct ApplicationResurrectionPlugin;

impl RustPlugin for ApplicationResurrectionPlugin {
    type Contract = bmux_application_resurrection_plugin_api::Contract;

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            resurrection_host_v1::client::StatusEndpoint => |_request, _context| {
                Ok(resurrection_host_v1::HostStatus {
                    protocol: "application-resurrection/1".to_owned(),
                    available: false,
                    reason: Some("execution-scoped transport is not installed".to_owned()),
                })
            },
        })
    }
}

bmux_plugin_sdk::export_plugin!(
    ApplicationResurrectionPlugin,
    include_str!("../plugin.toml")
);
