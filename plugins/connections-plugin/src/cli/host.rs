//! Host commands: `bmux remote enable|disable|pair|list|revoke|role|status`.
//! They call the running server's owner-local services over the local socket.
use super::{blocking, confirm, flag, option, positional, qr};
use anyhow::{Context, Result, bail, ensure};
use bmux_client::BmuxClient;
use bmux_connections_plugin_api::hosting_types::{
    Device, DeviceRole, DeviceState, PairingEventKind,
};
use bmux_connections_plugin_api::{hosting_admin_state, hosting_commands, hosting_state};
use std::time::Duration;

const PAIR_TTL_SECS: u64 = 600;
const EVENT_WAIT_MS: u64 = 10_000;

async fn connect(paths: &bmux_config::ConfigPaths) -> Result<BmuxClient> {
    let mut client = BmuxClient::connect_with_paths(paths, "bmux-remote-admin")
        .await
        .context("the bmux server for this runtime is not running; start bmux first")?;
    client.set_request_timeout(Duration::from_millis(EVENT_WAIT_MS + 15_000));
    Ok(client)
}

pub const fn role_name(role: DeviceRole) -> &'static str {
    match role {
        DeviceRole::Full => "full",
        DeviceRole::ViewOnly => "view-only",
    }
}

fn parse_role(text: &str) -> Result<DeviceRole> {
    match text {
        "full" => Ok(DeviceRole::Full),
        "view-only" => Ok(DeviceRole::ViewOnly),
        _ => bail!("role must be full or view-only"),
    }
}

const fn state_name(state: DeviceState) -> &'static str {
    match state {
        DeviceState::Pending => "pending",
        DeviceState::Active => "active",
        DeviceState::Revoked => "revoked",
    }
}

fn short(key: &str) -> &str {
    &key[..key.len().min(12)]
}

fn ago(timestamp: Option<u64>) -> String {
    timestamp.map_or_else(
        || "never".to_owned(),
        |at| {
            let seconds = crate::trust::now_secs().saturating_sub(at);
            match seconds {
                0..60 => format!("{seconds}s ago"),
                60..3_600 => format!("{}m ago", seconds / 60),
                3_600..86_400 => format!("{}h ago", seconds / 3_600),
                _ => format!("{}d ago", seconds / 86_400),
            }
        },
    )
}

fn print_devices(devices: &[Device]) {
    if devices.is_empty() {
        println!("No paired devices. Pair one with `bmux remote pair`.");
        return;
    }
    println!(
        "{:<20} {:<8} {:<10} {:<13} LAST SEEN",
        "NAME", "STATE", "ROLE", "KEY"
    );
    for device in devices {
        println!(
            "{:<20} {:<8} {:<10} {:<13} {}",
            device.name,
            state_name(device.state),
            role_name(device.role),
            short(&device.device_key),
            ago(device.last_seen)
        );
    }
}

/// Run one host command.
pub async fn run(paths: &bmux_config::ConfigPaths, command: &str, args: &[String]) -> Result<i32> {
    let mut client = connect(paths).await?;
    match command {
        "remote-enable" => {
            if !flag(args, "--yes")
                && !blocking(|| {
                    confirm(
                        "Paired devices will be able to reach this machine's bmux sessions over the network. Continue? [y/N] ",
                    )
                })
                .await?
            {
                bail!("cancelled");
            }
            let status = hosting_commands::client::set_enabled(&mut client, true).await?;
            println!("Remote access enabled.");
            println!("Host key: {}", status.endpoint_id);
            println!("Pair a device with: bmux remote pair");
        }
        "remote-disable" => {
            hosting_commands::client::set_enabled(&mut client, false).await?;
            println!("Remote access disabled. Remote connections were closed.");
        }
        "remote-pair" => pair(&mut client, args).await?,
        "remote-list" => {
            let devices = hosting_admin_state::client::list_devices(&mut client).await?;
            print_devices(&devices);
        }
        "remote-revoke" => {
            let [device] = positional(args, &[])
                .try_into()
                .map_err(|_| anyhow::anyhow!("usage: bmux remote revoke <device>"))?;
            let device = hosting_commands::client::revoke_device(&mut client, device).await?;
            println!("Revoked {}. Its connections were closed.", device.name);
        }
        "remote-role" => {
            let [device, role] = positional(args, &[]).try_into().map_err(|_| {
                anyhow::anyhow!("usage: bmux remote role <device> <full|view-only>")
            })?;
            let device =
                hosting_commands::client::set_device_role(&mut client, device, parse_role(&role)?)
                    .await?;
            println!("{} is now {}.", device.name, role_name(device.role));
        }
        "remote-status" => status(&mut client).await?,
        _ => bail!("unknown remote command"),
    }
    Ok(0)
}

async fn status(client: &mut BmuxClient) -> Result<()> {
    let status = hosting_state::client::status(client).await?;
    println!(
        "Remote access: {} ({})",
        if status.enabled {
            "enabled"
        } else {
            "disabled"
        },
        status.source
    );
    println!("Host key: {}", status.endpoint_id);
    if !status.bound_addrs.is_empty() {
        println!("Bound: {}", status.bound_addrs.join(", "));
    }
    println!(
        "Relay: {}{}",
        status.relay_mode,
        status
            .relay
            .as_deref()
            .map_or_else(String::new, |relay| format!(" ({relay})"))
    );
    if let Some(warning) = status.warning {
        println!("Warning: {warning}");
    }
    let devices = hosting_admin_state::client::list_devices(client).await?;
    println!();
    print_devices(&devices);
    let live = hosting_admin_state::client::live_connections(client).await?;
    println!();
    if live.is_empty() {
        println!("No live remote connections.");
    } else {
        println!("Live connections:");
        for connection in live {
            println!(
                "  {} ({}) via {}{}, connected {}s",
                connection.name,
                short(&connection.device_key),
                connection.path,
                connection
                    .rtt_ms
                    .map_or_else(String::new, |rtt| format!(", rtt {rtt}ms")),
                connection.connected_secs
            );
        }
    }
    Ok(())
}

async fn pair(client: &mut BmuxClient, args: &[String]) -> Result<()> {
    let role = option(args, "--role").map_or(Ok(DeviceRole::Full), |role| parse_role(&role))?;
    let name = option(args, "--name");
    ensure!(
        positional(args, &["--role", "--name"]).is_empty(),
        "usage: bmux remote pair [--role full|view-only] [--name NAME]"
    );
    let session =
        hosting_commands::client::begin_pairing(client, role, name, PAIR_TTL_SECS).await?;
    println!("Pairing link: {}", session.link);
    if let Some(code) = qr(&session.link) {
        println!("{code}");
    }
    println!("On the other device run: bmux connect --pair");
    println!(
        "The link works once and expires in {} minutes. Waiting for a device (Ctrl-C to stop)...",
        PAIR_TTL_SECS / 60
    );
    let result = wait_for_requests(client, &session.session_id, role).await;
    let _ = hosting_commands::client::end_pairing(client, session.session_id.clone()).await;
    result
}

async fn wait_for_requests(
    client: &mut BmuxClient,
    session_id: &str,
    role: DeviceRole,
) -> Result<()> {
    loop {
        let event = hosting_commands::client::next_pairing_event(
            client,
            session_id.to_owned(),
            EVENT_WAIT_MS,
        )
        .await?;
        match event.kind {
            PairingEventKind::Idle => {}
            PairingEventKind::Withdrawn => println!("The waiting device disconnected."),
            PairingEventKind::Ended => {
                bail!(
                    "pairing ended ({}); run `bmux remote pair` again for a new link",
                    event.reason.as_deref().unwrap_or("expired")
                );
            }
            PairingEventKind::Request => {
                let request_id = event.request_id.context("pairing request without id")?;
                let name = event.device_name.unwrap_or_default();
                let key = event.device_key.unwrap_or_default();
                println!();
                println!("Device requesting access: {name} ({})", short(&key));
                println!(
                    "Compare this phrase on the other device: {}",
                    event.phrase.unwrap_or_default()
                );
                let question = format!(
                    "Approve this device with {} access? [y/N] ",
                    role_name(role)
                );
                let approve = blocking(move || confirm(&question))
                    .await
                    .unwrap_or(false);
                let device = hosting_commands::client::decide_pairing(
                    client,
                    session_id.to_owned(),
                    request_id,
                    approve,
                )
                .await?;
                if let Some(device) = device {
                    println!(
                        "Paired {} with {} access. Revoke it with `bmux remote revoke {}`.",
                        device.name,
                        role_name(device.role),
                        device.name
                    );
                    return Ok(());
                }
                println!("Rejected. Still waiting for a device...");
            }
        }
    }
}
