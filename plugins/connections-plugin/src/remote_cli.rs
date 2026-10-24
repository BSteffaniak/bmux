//! Account-free CLI workflow; all authority changes use local generated services.
use crate::{remote_preferences, target};
use anyhow::{Context, Result, bail, ensure};
use bmux_connections_plugin_api::{
    pairing_commands::client as commands,
    pairing_state::client as state,
    pairing_types::{DeviceAccess, Invitation},
};
use bmux_plugin_sdk::NativeCommandContext;
use serde::{Deserialize, Serialize};
use std::io::{IsTerminal, Read, Write};

const LIMIT: u64 = 32_768;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairRequest {
    version: u8,
    device_key: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Offer {
    version: u8,
    target: String,
    invitation: Invitation,
}

fn value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|v| v[0] == flag).map(|v| v[1].clone())
}
fn input(args: &[String]) -> Result<String> {
    let text = if let Some(file) = value(args, "--file") {
        let mut text = String::new();
        std::fs::File::open(file)?
            .take(LIMIT + 1)
            .read_to_string(&mut text)?;
        text
    } else if args.iter().any(|a| a == "--stdin") {
        let mut text = String::new();
        std::io::stdin().take(LIMIT + 1).read_to_string(&mut text)?;
        text
    } else {
        args.first()
            .filter(|a| !a.starts_with('-'))
            .cloned()
            .context("provide a value, --file PATH, or --stdin")?
    };
    ensure!(text.len() as u64 <= LIMIT, "pairing input too large");
    Ok(text)
}
fn confirm(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--yes") {
        return Ok(());
    }
    ensure!(
        std::io::stdin().is_terminal(),
        "explicit full runtime access requires confirmation; use --yes for automation"
    );
    eprint!(
        "This enables full local runtime access for an explicitly paired device. Continue? [y/N] "
    );
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    ensure!(line.trim().eq_ignore_ascii_case("y"), "cancelled");
    Ok(())
}

async fn approve(
    paths: &bmux_config::ConfigPaths,
    client: &mut bmux_client::BmuxClient,
    args: &[String],
) -> Result<()> {
    let request: PairRequest = serde_json::from_str(&input(args)?)
        .map_err(|_| anyhow::anyhow!("invalid pairing request"))?;
    ensure!(request.version == 1, "unsupported pairing request version");
    let name = value(args, "--name").context("--name is required")?;
    remote_preferences::name_valid(&name)?;
    remote_preferences::update(paths, |p| {
        ensure!(!p.devices.contains_key(&name), "device name already exists");
        Ok(())
    })?;
    confirm(args)?;
    let listener = state::listener_status(client).await?;
    ensure!(
        listener.enabled,
        "remote listener is disabled; run bmux remote enable first"
    );
    // Relay first for a portable invitation, direct addressing for offline/LAN use.
    let target = listener
        .endpoint_targets
        .iter()
        .find(|t| t.contains("?relay="))
        .or_else(|| listener.endpoint_targets.first())
        .cloned()
        .context("listener has no advertised address yet; retry status")?;
    let invitation = commands::create_invitation(
        client,
        request.device_key.clone(),
        300,
        DeviceAccess::FullLocalRuntime,
    )
    .await?;
    // This is an explicit resource grant by the local owner, not a consequence
    // of transport admission. Redemption binds this subject to the exact device key.
    bmux_permissions_plugin_api::permissions_commands::client::grant_resource(
        client,
        invitation.invitation_id.parse()?,
        None,
        "writer".into(),
    )
    .await?;
    remote_preferences::update(paths, |p| {
        ensure!(!p.devices.contains_key(&name), "device name already exists");
        p.devices.insert(name.clone(), request.device_key);
        Ok(())
    })?;
    println!(
        "{}",
        serde_json::to_string(&Offer {
            version: 1,
            target,
            invitation
        })?
    );
    Ok(())
}

async fn pair(
    paths: &bmux_config::ConfigPaths,
    client: &mut bmux_client::BmuxClient,
    args: &[String],
) -> Result<()> {
    let offer: Offer = serde_json::from_str(&input(args)?)
        .map_err(|_| anyhow::anyhow!("invalid pairing invitation"))?;
    ensure!(offer.version == 1, "unsupported invitation version");
    let name = value(args, "--name").context("--name is required")?;
    remote_preferences::name_valid(&name)?;
    remote_preferences::update(paths, |p| {
        ensure!(!p.aliases.contains_key(&name), "target name already exists");
        Ok(())
    })?;
    let target = commands::pair_endpoint(client, offer.target, offer.invitation).await?;
    remote_preferences::update(paths, |p| {
        ensure!(!p.aliases.contains_key(&name), "target name already exists");
        p.aliases.insert(name.clone(), target);
        Ok(())
    })?;
    println!("Paired. Connect with: bmux connect {name}");
    Ok(())
}

pub fn run(context: &NativeCommandContext) -> Result<i32> {
    let caller = bmux_plugin::TypedServiceCaller::from_command_context(context);
    let mut dispatch = bmux_plugin::ServiceCallerDispatchClient::new(&caller);
    let origin = bmux_plugin::block_on_typed_dispatch(
        bmux_clients_plugin_api::clients_identity::client::current_stream_provenance(&mut dispatch),
    );
    ensure!(
        matches!(
            origin,
            Ok(Ok(
                bmux_clients_plugin_api::clients_identity::StreamProvenance::Local
                    | bmux_clients_plugin_api::clients_identity::StreamProvenance::HostInternal
            ))
        ),
        "remote management requires a trusted local owner"
    );
    let paths = target::context_paths(&context.connection);
    crate::block_on_connection(async {
        let mut client =
            bmux_client::BmuxClient::connect_with_paths(&paths, "bmux-remote-owner").await?;
        // Local endpoint proof and server-side ownership checks are mandatory.
        state::local_device_identity(&mut client).await?;
        let args = &context.arguments;
        match context.command.as_str() {
            "remote-pair-request" => {
                let key = state::local_device_identity(&mut client).await?;
                println!(
                    "{}",
                    serde_json::to_string(&PairRequest {
                        version: 1,
                        device_key: key
                    })?
                );
            }
            "remote-enable" => {
                confirm(args)?;
                let status = commands::set_enabled(&mut client, true).await?;
                println!("Remote enabled: {}", status.endpoint_id);
                for target in status.endpoint_targets {
                    println!("{target}");
                }
            }
            "remote-disable" => {
                commands::set_enabled(&mut client, false).await?;
                println!("Remote disabled");
            }
            "remote-approve" => approve(&paths, &mut client, args).await?,
            "remote-pair" => pair(&paths, &mut client, args).await?,
            "remote-list" => {
                remote_preferences::update(&paths, |p| {
                    for (name, target) in &p.aliases {
                        println!("target {name}: {target}");
                    }
                    for name in p.devices.keys() {
                        println!("device {name}");
                    }
                    Ok(())
                })?;
                for device in state::list_devices(&mut client).await? {
                    println!("{} {:?}", device.device_id, device.access);
                }
            }
            "remote-revoke" => {
                let name = args.first().context("device name or id required")?;
                let key = remote_preferences::update(&paths, |p| Ok(p.devices.get(name).cloned()))?;
                let devices = state::list_devices(&mut client).await?;
                let device = devices
                    .iter()
                    .find(|d| d.device_id == *name || key.as_ref() == Some(&d.device_key))
                    .context("paired device not found")?;
                commands::revoke_device(&mut client, device.device_id.clone()).await?;
                println!("Device revoked");
            }
            "remote-status" => {
                if let Some(name) = args.first() {
                    let target = remote_preferences::alias(&paths, name)?
                        .context("unknown paired target")?;
                    match crate::connect_paired(&paths, &target).await {
                        Ok(_) => println!("{name}: reachable and admitted"),
                        Err(error) => bail!("{name}: {error}"),
                    }
                } else {
                    let status = state::listener_status(&mut client).await?;
                    println!("enabled={} endpoint={}", status.enabled, status.endpoint_id);
                    for target in status.endpoint_targets {
                        println!("{target}");
                    }
                }
            }
            _ => bail!("unknown remote command"),
        }
        Ok(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requests_reject_unknown_fields() {
        assert!(
            serde_json::from_str::<PairRequest>(r#"{"version":1,"device_key":"x","secret":"bad"}"#)
                .is_err()
        );
    }
    #[test]
    fn noninteractive_confirmation_is_explicit() {
        assert!(confirm(&["--yes".into()]).is_ok());
    }
}
