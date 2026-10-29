//! Local-process client UX, separate from host administration and provenance.
use crate::{remote_preferences as preferences, target};
use anyhow::{Context, Result, bail, ensure};
use bmux_plugin::{HostRuntimeApi, TypedServiceCaller};
use bmux_plugin_sdk::{CoreCliCommandRequest, NativeCommandContext, NativeCommandInvocationSource};
use std::io::{BufRead, IsTerminal, Read, Write};
use std::time::{Duration, Instant};

fn option(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}
fn local(context: &NativeCommandContext) -> Result<()> {
    if context.invocation_source == NativeCommandInvocationSource::LocalProcess
        && context.caller_client_id.is_none()
    {
        return Ok(());
    }
    if context.host_kernel_bridge.is_some() {
        let caller = TypedServiceCaller::from_command_context(context);
        let mut dispatch = bmux_plugin::ServiceCallerDispatchClient::new(&caller);
        let origin = bmux_plugin::block_on_typed_dispatch(
            bmux_clients_plugin_api::clients_identity::client::current_stream_provenance(
                &mut dispatch,
            ),
        );
        ensure!(
            matches!(
                origin,
                Ok(Ok(
                    bmux_clients_plugin_api::clients_identity::StreamProvenance::Local
                        | bmux_clients_plugin_api::clients_identity::StreamProvenance::HostInternal
                ))
            ),
            "client credentials require trusted local provenance"
        );
    } else {
        bail!("client credentials require trusted local provenance or a standalone local process");
    }
    Ok(())
}
#[cfg(unix)]
#[path = "client_cli/echo_unix.rs"]
mod echo;
#[cfg(windows)]
#[path = "client_cli/echo_windows.rs"]
mod echo;

fn prompt(label: &str, secret: bool) -> Result<String> {
    ensure!(
        std::io::stdin().is_terminal(),
        "interactive terminal required"
    );
    eprint!("{label}: ");
    std::io::stderr().flush()?;
    let guard = secret.then(echo::Echo::hide).transpose()?;
    // Bound input without echoing secret-bearing parser errors. Buffering also
    // avoids one syscall per byte while terminal echo is disabled.
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(32769)
        .read_until(b'\n', &mut bytes)?;
    drop(guard);
    if secret {
        eprintln!();
    }
    ensure!(bytes.len() <= 32768, "input too long");
    let line = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("input must be UTF-8"))?;
    Ok(line.trim().to_owned())
}
fn forward(context: &NativeCommandContext, path: &str, arguments: Vec<String>) -> Result<i32> {
    Ok(TypedServiceCaller::from_command_context(context)
        .core_cli_command_run_path(&CoreCliCommandRequest::new(vec![path.into()], arguments))?
        .exit_code)
}
fn positional(args: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if matches!(arg.as_str(), "--name" | "--device-name" | "--follow") {
            skip = true;
        } else if !arg.starts_with('-') {
            result.push(arg.clone());
        }
    }
    result
}
pub fn run(context: &NativeCommandContext) -> Result<Option<i32>> {
    let args = &context.arguments;
    let paths = target::context_paths(&context.connection);
    match context.command.as_str() {
        "remote-pair-request" => {
            local(context)?;
            let identity =
                crate::identity::DeviceIdentity::load_or_create_local(&paths.state_dir, "default")?;
            println!(
                "{}",
                serde_json::json!({"version": 1, "device_key": identity.public_key()})
            );
            Ok(Some(0))
        }
        "remote-list" | "remote-rename" | "remote-forget" | "remote-status"
            if context.command != "remote-status" || !args.is_empty() =>
        {
            local(context)?;
            match context.command.as_str() {
                "remote-list" => {
                    println!("Saved hosts on this client (not host-side device grants):");
                    for host in preferences::hosts(&paths.state_dir, "default")? {
                        println!("{}", host.name);
                    }
                }
                "remote-rename" => preferences::rename_host(
                    &paths.state_dir,
                    "default",
                    args.first().context("host alias required")?,
                    args.get(1).context("new alias required")?,
                )?,
                "remote-forget" => {
                    forget(context, args.first().context("host alias required")?)?;
                    println!("Forgot local host credentials; host-side authorization is unchanged");
                }
                _ => {
                    let name = args.first().context("host alias required")?;
                    ensure!(
                        preferences::host_target(&paths.state_dir, "default", name)?.is_some(),
                        "unknown saved host"
                    );
                    println!("{name}: saved on this client (connect to check reachability)");
                }
            }
            Ok(Some(0))
        }
        "connect" => {
            local(context)?;
            let positions = positional(args);
            let host = if args.iter().any(|arg| arg == "--pair") {
                ensure!(
                    positions.is_empty(),
                    "--pair takes no positional target; paste the invitation at the prompt"
                );
                pair(context)?
            } else {
                let Some(name) = positions.first() else {
                    return forward(context, "connect", args.clone()).map(Some);
                };
                if preferences::host_target(&paths.state_dir, "default", name)?.is_none() {
                    return forward(context, "connect", args.clone()).map(Some);
                }
                name.clone()
            };
            ensure!(
                option(args, "--follow").is_none() && !args.iter().any(|arg| arg == "--global"),
                "paired terminals do not support follow mode"
            );
            let terminal = positions.get(1).cloned().unwrap_or_else(|| "main".into());
            preferences::name_valid(&terminal)?;
            crate::attach_provider::install();
            crate::attach_provider::set_paths(paths);
            forward(
                context,
                "attach",
                vec![format!("paired-terminal://{host}/{terminal}")],
            )
            .map(Some)
        }
        _ => Ok(None),
    }
}
fn forget(context: &NativeCommandContext, name: &str) -> Result<()> {
    let paths = target::context_paths(&context.connection);
    preferences::host_target(&paths.state_dir, "default", name)?.context("unknown saved host")?;
    let config = target::load_config(&context.connection)?;
    forget_with_cleanup(&paths, &config, name, |endpoint| {
        crate::terminal_attach::forget_terminals(&paths, &config, endpoint)
    })
}

fn forget_with_cleanup(
    paths: &bmux_config::ConfigPaths,
    config: &bmux_config::BmuxConfig,
    name: &str,
    cleanup: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    let route = preferences::host_target(&paths.state_dir, "default", name)?
        .context("unknown saved host")?;
    let endpoint = match target::resolve_internal(config, &route) {
        Ok(target::ResolvedTarget::Iroh(target)) => Some(target.endpoint_id),
        _ => None,
    };
    if let Some(endpoint) = endpoint {
        // A second alias to this host still needs the same device binding.
        let mut shared = false;
        for host in preferences::hosts(&paths.state_dir, "default")? {
            if host.name == name {
                continue;
            }
            let route = preferences::host_target(&paths.state_dir, "default", &host.name)?
                .context("saved host disappeared")?;
            if matches!(target::resolve_internal(config, &route), Ok(target::ResolvedTarget::Iroh(target)) if target.endpoint_id == endpoint)
            {
                shared = true;
            }
        }
        if !shared {
            crate::outbound::forget_local(&paths.state_dir, "default", &endpoint)?;
            cleanup(&endpoint)?;
        }
    }
    // Alias is the durable retry handle. Do not remove it until all secret
    // cleanup is complete; repeating the preceding removals is idempotent.
    preferences::forget_host(&paths.state_dir, "default", name)?;
    Ok(())
}

fn pair(context: &NativeCommandContext) -> Result<String> {
    use bmux_connections_plugin_api::pairing_types::SessionPhase;
    let paths = target::context_paths(&context.connection);
    let link =
        crate::pairing::invitation::InvitationLink::parse(&prompt("Pairing link (hidden)", true)?)?;
    let name =
        option(&context.arguments, "--name").map_or_else(|| prompt("Save host as", false), Ok)?;
    let device_name = option(&context.arguments, "--device-name")
        .map_or_else(|| prompt("Device name", false), Ok)?;
    preferences::name_valid(&name)?;
    preferences::name_valid(&device_name)?;
    ensure!(
        preferences::host_target(&paths.state_dir, "default", &name)?.is_none(),
        "host alias already exists"
    );
    let identity =
        crate::identity::DeviceIdentity::load_or_create_local(&paths.state_dir, "default")?;
    let route = link
        .endpoint_targets
        .iter()
        .find(|route| route.contains("?relay="))
        .or_else(|| link.endpoint_targets.first())
        .context("invitation has no route")?
        .clone();
    let config = target::load_config(&context.connection)?;
    let target::ResolvedTarget::Iroh(resolved) = target::resolve_internal(&config, &route)
        .map_err(|_| anyhow::anyhow!("invalid invitation route"))?
    else {
        bail!("invalid invitation route");
    };
    ensure!(
        resolved.endpoint_id == link.host_key,
        "invitation route host mismatch"
    );
    let mut address = iroh::EndpointAddr::new(link.host_key.parse()?);
    if let Some(relay) = &resolved.relay_url {
        address = address.with_relay_url(relay.parse()?);
    }
    if let Some(ip) = resolved.ip_addr {
        address = address.with_ip_addr(ip);
    }
    crate::block_on_connection(async {
        let settings = crate::config::RemoteSettings::load(context.settings.as_ref())?;
        let client = crate::device_client::PairingClient::connect_named_with_policy(
            &identity,
            link,
            address,
            Some(device_name.clone()),
            &settings.relay,
            Duration::from_secs(15),
        )
        .await?;
        eprintln!("Pairing session {}", client.session_id());
        eprintln!(
            "Device {device_name}. Verify on the host: {}",
            client.verification_phrase()
        );
        let deadline = Instant::now() + Duration::from_mins(10);
        let device = loop {
            ensure!(Instant::now() < deadline, "pairing timed out");
            match client.status(Duration::from_secs(15)).await?.phase {
                SessionPhase::Active => break client.result(Duration::from_secs(15)).await?,
                SessionPhase::Rejected | SessionPhase::Cancelled | SessionPhase::Expired => {
                    bail!("pairing was rejected, cancelled, or expired")
                }
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        };
        ensure!(
            device.device_key == identity.public_key(),
            "paired device identity mismatch"
        );
        crate::outbound::remember_local(
            &paths.state_dir,
            "default",
            &resolved.endpoint_id,
            &identity.public_key(),
            device.device_id.parse()?,
        )?;
        preferences::remember_host(&paths.state_dir, "default", &name, &route)?;
        client.close().await;
        println!("Paired. Saved host: {name}");
        Ok(name)
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn context(root: &std::path::Path) -> NativeCommandContext {
        serde_json::from_value(serde_json::json!({
            "plugin_id": "bmux.connections", "command": "remote-list", "arguments": [],
            "host": {"product_name": "test", "product_version": "test", "plugin_api_version": {"major": 1, "minor": 0}, "plugin_abi_version": {"major": 1, "minor": 0}},
            "connection": {"config_dir": root, "runtime_dir": root.join("missing-runtime"), "data_dir": root, "state_dir": root},
            "invocation_source": "local_process"
        })).unwrap()
    }
    #[test]
    fn daemonless_identity_and_saved_hosts_need_no_runtime_directory() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        context.command = "remote-pair-request".into();
        assert_eq!(run(&context).unwrap(), Some(0));
        context.command = "remote-list".into();
        assert_eq!(run(&context).unwrap(), Some(0));
        assert!(!root.path().join("missing-runtime").exists());
    }
    unsafe extern "C" fn untrusted_bridge(
        _: *const u8,
        _: usize,
        _: *mut u8,
        _: usize,
        _: *mut usize,
    ) -> i32 {
        -1
    }
    #[test]
    fn local_process_with_forwarding_bridge_needs_no_server() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        context.host_kernel_bridge =
            Some(bmux_plugin_sdk::HostKernelBridge::from_fn(untrusted_bridge));
        context.command = "remote-pair-request".into();
        assert_eq!(run(&context).unwrap(), Some(0));
        assert!(!root.path().join("missing-runtime").exists());
    }
    #[test]
    fn local_process_with_caller_id_cannot_bypass_provenance() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        context.caller_client_id = Some(uuid::Uuid::new_v4());
        assert!(run(&context).is_err());
        context.host_kernel_bridge =
            Some(bmux_plugin_sdk::HostKernelBridge::from_fn(untrusted_bridge));
        assert!(run(&context).is_err());
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    }
    #[test]
    fn plain_cli_without_bridge_cannot_write_credentials() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        context.invocation_source = NativeCommandInvocationSource::Cli;
        assert!(run(&context).is_err());
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    }
    #[test]
    fn server_invocation_without_a_bridge_cannot_write_credentials() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        context.invocation_source = NativeCommandInvocationSource::AttachKeybinding;
        context.command = "remote-pair-request".into();
        assert!(run(&context).is_err());
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    }
    #[test]
    fn a_cli_label_cannot_override_an_untrusted_host_bridge() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        context.invocation_source = NativeCommandInvocationSource::Cli;
        context.host_kernel_bridge =
            Some(bmux_plugin_sdk::HostKernelBridge::from_fn(untrusted_bridge));
        assert!(run(&context).is_err());
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    }
    #[test]
    fn interrupted_forget_preserves_retry_handle_and_retries_missing_binding() {
        let root = tempfile::tempdir().unwrap();
        let context = context(root.path());
        let paths = target::context_paths(&context.connection);
        let config = bmux_config::BmuxConfig::default();
        let host = iroh::SecretKey::generate().public().to_string();
        let identity =
            crate::identity::DeviceIdentity::load_or_create_local(&paths.state_dir, "default")
                .unwrap();
        let route = format!("device://{host}?addr=127.0.0.1:5555");
        preferences::remember_host(&paths.state_dir, "default", "desktop", &route).unwrap();
        crate::outbound::remember_local(
            &paths.state_dir,
            "default",
            &host,
            &identity.public_key(),
            uuid::Uuid::new_v4(),
        )
        .unwrap();
        let terminal_key = format!("endpoint:{host}/main");
        preferences::client_update(&paths.state_dir, "default", |preferences| {
            preferences
                .terminals
                .insert(terminal_key.clone(), "private-reference".into());
            preferences
                .terminals
                .insert("unrelated".into(), "preserve".into());
            Ok(())
        })
        .unwrap();
        assert!(
            forget_with_cleanup(&paths, &config, "desktop", |_| bail!(
                "interrupted terminal cleanup"
            ))
            .is_err()
        );
        assert!(
            preferences::host_target(&paths.state_dir, "default", "desktop")
                .unwrap()
                .is_some()
        );
        assert!(!crate::outbound::forget_local(&paths.state_dir, "default", &host).unwrap());
        forget_with_cleanup(&paths, &config, "desktop", |endpoint| {
            crate::terminal_attach::forget_terminals(&paths, &config, endpoint)
        })
        .unwrap();
        preferences::client_update(&paths.state_dir, "default", |preferences| {
            assert!(!preferences.terminals.contains_key(&terminal_key));
            assert_eq!(
                preferences.terminals.get("unrelated").map(String::as_str),
                Some("preserve")
            );
            Ok(())
        })
        .unwrap();
        assert!(
            preferences::host_target(&paths.state_dir, "default", "desktop")
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn forgetting_one_of_two_aliases_preserves_shared_credentials() {
        let root = tempfile::tempdir().unwrap();
        let context = context(root.path());
        let paths = target::context_paths(&context.connection);
        let config = bmux_config::BmuxConfig::default();
        let host = iroh::SecretKey::generate().public().to_string();
        let route = format!("device://{host}?addr=127.0.0.1:5555");
        preferences::remember_host(&paths.state_dir, "default", "first", &route).unwrap();
        preferences::remember_host(&paths.state_dir, "default", "second", &route).unwrap();
        forget_with_cleanup(&paths, &config, "first", |_| {
            bail!("shared credentials must not be cleaned")
        })
        .unwrap();
        assert!(
            preferences::host_target(&paths.state_dir, "default", "first")
                .unwrap()
                .is_none()
        );
        assert!(
            preferences::host_target(&paths.state_dir, "default", "second")
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn manifest_declares_generic_forward_capability() {
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(include_str!("../plugin.toml")).unwrap();
        assert!(
            manifest
                .required_capabilities
                .iter()
                .any(|capability| capability.as_str()
                    == bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY)
        );
    }
    #[test]
    fn connect_positionals_exclude_option_values() {
        assert_eq!(
            positional(&[
                "desktop".into(),
                "main".into(),
                "--name".into(),
                "alias".into(),
                "--pair".into()
            ]),
            vec!["desktop", "main"]
        );
    }
}
