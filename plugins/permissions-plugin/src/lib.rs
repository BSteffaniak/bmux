#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::{
    CommandExecutionKind, HostScope, NativeCommandContext, NativeDescriptor, NativeServiceContext,
    PluginCommand, PluginCommandArgument, PluginCommandArgumentKind, PluginService, RustPlugin,
    ServiceKind, ServiceResponse, decode_service_message, encode_service_message,
};
use serde::{Deserialize, Serialize};

#[derive(Default)]
struct PermissionsPlugin;

impl RustPlugin for PermissionsPlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor::builder("bmux.permissions", "bmux Permissions")
            .plugin_version(env!("CARGO_PKG_VERSION"))
            .description("Shipped bmux permissions command plugin")
            .require_capability("bmux.commands")
            .expect("capability should parse")
            .provide_capability("bmux.permissions.read")
            .expect("capability should parse")
            .provide_capability("bmux.permissions.write")
            .expect("capability should parse")
            .provide_feature("bmux.permissions")
            .expect("feature should parse")
            .service(PluginService {
                capability: HostScope::new("bmux.permissions.read")
                    .expect("host scope should parse"),
                kind: ServiceKind::Query,
                interface_id: "permission-query/v1".to_string(),
            })
            .service(PluginService {
                capability: HostScope::new("bmux.permissions.write")
                    .expect("host scope should parse"),
                kind: ServiceKind::Command,
                interface_id: "permission-command/v1".to_string(),
            })
            .command(
                PluginCommand::new("permissions", "Permissions provider status")
                    .path(["permissions"])
                    .alias(["session", "permissions"])
                    .argument(
                        PluginCommandArgument::option("session", PluginCommandArgumentKind::String)
                            .required(true)
                            .short('s'),
                    )
                    .argument(PluginCommandArgument::flag("json").short('j'))
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
            )
            .command(
                PluginCommand::new("grant", "Grant command handled by permissions provider")
                    .path(["grant"])
                    .alias(["session", "grant"])
                    .argument(
                        PluginCommandArgument::option("session", PluginCommandArgumentKind::String)
                            .required(true)
                            .short('s'),
                    )
                    .argument(
                        PluginCommandArgument::option("client", PluginCommandArgumentKind::String)
                            .required(true)
                            .short('c'),
                    )
                    .argument(
                        PluginCommandArgument::option("role", PluginCommandArgumentKind::Choice)
                            .required(true)
                            .short('r')
                            .choice_values(["owner", "writer", "observer"]),
                    )
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
            )
            .command(
                PluginCommand::new("revoke", "Revoke command handled by permissions provider")
                    .path(["revoke"])
                    .alias(["session", "revoke"])
                    .argument(
                        PluginCommandArgument::option("session", PluginCommandArgumentKind::String)
                            .required(true)
                            .short('s'),
                    )
                    .argument(
                        PluginCommandArgument::option("client", PluginCommandArgumentKind::String)
                            .required(true)
                            .short('c'),
                    )
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
            )
            .build()
            .expect("descriptor should validate")
    }

    fn run_command(&mut self, context: NativeCommandContext) -> i32 {
        let _ = context;
        println!("permissions provider active (single-user permissive baseline)");
        0
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.service.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            ("permission-query/v1", "list") => {
                let request = match decode_service_message::<ListPermissionsRequest>(
                    &context.request.payload,
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
                let _ = request;
                let payload = match encode_service_message(&ListPermissionsResponse {
                    entries: Vec::new(),
                }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("permission-command/v1", "grant") | ("permission-command/v1", "revoke") => {
                let payload = match encode_service_message(&CommandAckResponse { ok: true }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            _ => ServiceResponse::error(
                "unsupported_service_operation",
                format!(
                    "unsupported permissions service invocation '{}:{}'",
                    context.request.service.interface_id, context.request.operation,
                ),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsRequest {
    session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionEntry {
    client_id: String,
    role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsResponse {
    entries: Vec<PermissionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CommandAckResponse {
    ok: bool,
}

bmux_plugin::export_plugin!(PermissionsPlugin);
