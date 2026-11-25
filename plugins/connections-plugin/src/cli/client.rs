//! Client commands: `bmux connect --pair`, `bmux connect <host> [session]`,
//! `bmux remote hosts|rename|forget`. None of them needs or starts a server.
use super::{CLIENT_PROFILE, flag, option, positional, prompt};
use crate::identity::DeviceKey;
use crate::known_hosts::{KnownHost, KnownHosts, valid_alias};
use crate::link::PairingLink;
use crate::pairing::client::pair;
use crate::transport::manager::ConnectionManager;
use crate::trust::Role;
use anyhow::{Context, Result, bail, ensure};
use bmux_plugin::{HostRuntimeApi, TypedServiceCaller};
use bmux_plugin_sdk::{CoreCliCommandRequest, NativeCommandContext};

const CONNECT_VALUE_FLAGS: &[&str] = &["--name", "--device-name"];

/// Run `attach --target <alias> [session]` through the core CLI. The attach
/// blocks for the whole session, so it runs on the blocking pool; the core
/// command drives it on this process's runtime.
async fn attach(context: &NativeCommandContext, alias: &str, session: Option<&str>) -> Result<i32> {
    let mut arguments = vec!["--target".to_owned(), alias.to_owned()];
    arguments.extend(session.map(ToOwned::to_owned));
    let caller = TypedServiceCaller::from_command_context(context);
    let response = super::blocking(move || {
        Ok(caller.core_cli_command_run_path(&CoreCliCommandRequest::new(
            vec!["attach".to_owned()],
            arguments,
        ))?)
    })
    .await?;
    if let Some(error) = response.error {
        bail!("{error}");
    }
    Ok(response.exit_code)
}

pub async fn connect(context: &NativeCommandContext) -> Result<i32> {
    let args = &context.arguments;
    let paths = super::paths(context);
    let positions = positional(args, CONNECT_VALUE_FLAGS);
    if flag(args, "--pair") {
        ensure!(
            positions.is_empty(),
            "`bmux connect --pair` takes no host; paste the link at the prompt"
        );
        let alias = pair_interactively(
            &paths,
            option(args, "--name"),
            option(args, "--device-name"),
        )
        .await?;
        return attach(context, &alias, None).await;
    }
    ensure!(
        option(args, "--name").is_none() && option(args, "--device-name").is_none(),
        "--name and --device-name are only used with --pair"
    );
    let hosts = KnownHosts::open(&paths.state_dir, CLIENT_PROFILE)?;
    let Some(alias) = positions.first() else {
        let saved = hosts.list()?;
        if saved.is_empty() {
            bail!("no saved hosts; pair one with `bmux connect --pair`");
        }
        bail!(
            "choose a host: bmux connect <host> [session]\nSaved hosts: {}",
            saved.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    };
    ensure!(positions.len() <= 2, "usage: bmux connect <host> [session]");
    hosts
        .get(alias)?
        .with_context(|| format!("no saved host named '{alias}'; see `bmux remote hosts`"))?;
    attach(context, alias, positions.get(1).map(String::as_str)).await
}

/// Pair with the host whose link is pasted at a hidden prompt; returns the alias.
async fn pair_interactively(
    paths: &bmux_config::ConfigPaths,
    alias: Option<String>,
    device_name: Option<String>,
) -> Result<String> {
    if let Some(alias) = &alias {
        valid_alias(alias)?;
    }
    let link = PairingLink::decode(
        &super::blocking(|| prompt("Pairing link (input hidden): ", true)).await?,
    )?;
    let device = DeviceKey::load_or_create(&paths.state_dir, CLIENT_PROFILE)?;
    let hosts = KnownHosts::open(&paths.state_dir, CLIENT_PROFILE)?;
    let manager =
        ConnectionManager::shared(device.secret(), &crate::connector::client_transport(paths)?);
    let device_name = device_name.unwrap_or_else(crate::host::host_name);
    let paired = pair(
        &manager,
        device.public(),
        &link,
        &device_name,
        Role::Full,
        |phrase| {
            println!("Verify on the host: {phrase}");
            println!("Waiting for the host owner to approve...");
        },
    )
    .await?;
    let saved = hosts.list()?;
    let host_key = link.host_key.to_string();
    let alias = alias.unwrap_or_else(|| {
        let base = crate::pairing::protocol::sanitize_name(&paired.host_name);
        let base = if valid_alias(&base).is_ok() {
            base
        } else {
            "host".to_owned()
        };
        let free = |name: &str| saved.get(name).is_none_or(|host| host.host_key == host_key);
        if free(&base) {
            return base;
        }
        (2..1_000)
            .map(|n| format!("{base}-{n}"))
            .find(|name| free(name))
            .unwrap_or(base)
    });
    hosts.insert(
        &alias,
        KnownHost {
            host_key,
            addrs: link.addrs,
            relay: link.relay,
            principal: paired.principal,
            last_path: None,
            last_runtime: None,
        },
    )?;
    println!("Paired. Saved host: {alias}");
    Ok(alias)
}

pub fn hosts(context: &NativeCommandContext) -> Result<i32> {
    let paths = super::paths(context);
    let hosts = KnownHosts::open(&paths.state_dir, CLIENT_PROFILE)?.list()?;
    if hosts.is_empty() {
        println!("No saved hosts. Pair one with `bmux connect --pair`.");
        return Ok(0);
    }
    println!("{:<20} {:<13} LAST PATH", "HOST", "HOST KEY");
    for (alias, host) in hosts {
        println!(
            "{alias:<20} {:<13} {}",
            &host.host_key[..host.host_key.len().min(12)],
            host.last_path.as_deref().unwrap_or("-")
        );
    }
    Ok(0)
}

pub fn rename(context: &NativeCommandContext) -> Result<i32> {
    let paths = super::paths(context);
    let [old, new] = positional(&context.arguments, &[])
        .try_into()
        .map_err(|_| anyhow::anyhow!("usage: bmux remote rename <old> <new>"))?;
    KnownHosts::open(&paths.state_dir, CLIENT_PROFILE)?.rename(&old, &new)?;
    println!("Renamed saved host {old} to {new}.");
    Ok(0)
}

pub fn forget(context: &NativeCommandContext) -> Result<i32> {
    let paths = super::paths(context);
    let [alias] = positional(&context.arguments, &[])
        .try_into()
        .map_err(|_| anyhow::anyhow!("usage: bmux remote forget <host>"))?;
    KnownHosts::open(&paths.state_dir, CLIENT_PROFILE)?.forget(&alias)?;
    println!(
        "Forgot {alias} on this device. The host still lists this device until its owner runs `bmux remote revoke`."
    );
    Ok(0)
}
