//! Noninteractive health checks in the selected server's scope. An admitted
//! peer is not authorized to borrow the server's outbound credentials.
use crate::{block_on_connection, target, transport};
use bmux_plugin::{ServiceCaller, StreamOrigin, TypedServiceCaller};
use bmux_plugin_sdk::{NativeCommandContext, PluginCommandError};

pub fn nested_probe(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let doctor = match context.command.as_str() {
        "remote-test" => false,
        "remote-doctor" => true,
        _ => return Err(PluginCommandError::failed("not a health probe")),
    };
    let [name] = context.arguments.as_slice() else {
        return Err(PluginCommandError::failed(
            "read-only health probe requires one configured target",
        ));
    };
    if name.trim().is_empty() || name.starts_with('-') {
        return Err(PluginCommandError::failed(
            "expected a target, not an option",
        ));
    }
    let caller = TypedServiceCaller::from_command_context(context);
    if !matches!(
        caller.current_stream_origin(),
        Ok(StreamOrigin::Local | StreamOrigin::HostInternal)
    ) {
        return Err(PluginCommandError::failed(
            "health probe requires trusted local runtime authority",
        ));
    }
    let mut config = target::load_config(&context.connection)
        .map_err(|e| PluginCommandError::failed(e.to_string()))?;
    if name != "local" && !config.connections.targets.contains_key(name) {
        return Err(PluginCommandError::failed(
            "health target is not configured in this runtime",
        ));
    }
    // Never enroll a new TLS pin during a probe, even under trust-new policy.
    config.connections.tls_trust.mode = bmux_config::TlsTrustMode::RequireKnown;
    let paths = target::context_paths(&context.connection);
    let endpoint = target::resolve_internal(&config, name)
        .map_err(|_| PluginCommandError::failed("invalid health target"))?;
    block_on_connection(async {
        if let target::ResolvedTarget::Ssh(ssh) = &endpoint {
            transport::probe_ssh(ssh, doctor)
                .await
                .map_err(|_| PluginCommandError::failed("SSH health probe failed"))?;
        } else {
            let mut client = transport::connect(&config, &paths, &endpoint)
                .await
                .map_err(|_| PluginCommandError::failed("health endpoint connection failed"))?;
            tokio::time::timeout(std::time::Duration::from_secs(8), client.ping())
                .await
                .map_err(|_| PluginCommandError::failed("health probe timed out"))?
                .map_err(|_| PluginCommandError::failed("health endpoint ping failed"))?;
        }
        Ok(0)
    })
}
