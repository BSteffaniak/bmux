//! `bmux remote status [--json]` and `bmux remote log [--limit N] [--json]`.
//!
//! Both ask this runtime's server when it runs. Without one they still work:
//! status reads what this machine stored (hosting choice, host key, paired
//! devices) and this device's saved hosts, and the log reads the stored
//! audit log. Neither creates a server, socket, runtime directory or key.
use super::host::{ago, connect, print_devices, role_name, short, state_name};
use super::{CLIENT_PROFILE, flag, option, positional};
use crate::host::{explain_source, offline_trust};
use crate::service::{audit_event, device};
use anyhow::{Result, ensure};
use bmux_client::BmuxClient;
use bmux_connections_plugin_api::hosting_types::{
    AuditEvent, AuditEventKind, Device, HostingStatus, LiveConnection,
};
use bmux_connections_plugin_api::saved_host_types::SavedHost;
use bmux_connections_plugin_api::{hosting_admin_state, hosting_state};
use bmux_plugin_sdk::NativeCommandContext;
use serde_json::{Value, json};

const DEFAULT_LOG_LIMIT: u32 = 50;
const MAX_LOG_LIMIT: u32 = 1_000;

/// The local server, when one runs for this runtime. Never starts one, and
/// never creates anything when its socket does not exist.
async fn running_server(paths: &bmux_config::ConfigPaths) -> Option<BmuxClient> {
    #[cfg(unix)]
    if !paths.server_socket().exists() {
        return None;
    }
    connect(paths).await.ok()
}

/// The host side of status: from the server, or from stored state.
struct HostView {
    server_running: bool,
    status: HostingStatus,
    devices: Vec<Device>,
    live: Vec<LiveConnection>,
    /// Why the devices could not be read (for example, another runtime hosts).
    devices_note: Option<String>,
}

async fn host_view(paths: &bmux_config::ConfigPaths) -> Result<HostView> {
    if let Some(mut client) = running_server(paths).await {
        let status = hosting_state::client::status(&mut client).await?;
        let devices = hosting_admin_state::client::list_devices(&mut client).await?;
        let live = hosting_admin_state::client::live_connections(&mut client).await?;
        return Ok(HostView {
            server_running: true,
            status,
            devices,
            live,
            devices_note: None,
        });
    }
    let settings = crate::connector::remote_settings(paths)?;
    let stored = crate::host::stored(&paths.state_dir)?;
    let (devices, devices_note) = match offline_trust(&paths.state_dir, |trust| Ok(trust.list())) {
        Ok(devices) => (
            devices
                .unwrap_or_default()
                .into_iter()
                .map(device)
                .collect(),
            None,
        ),
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    Ok(HostView {
        server_running: false,
        status: HostingStatus {
            enabled: false,
            source: crate::host::source(&settings, stored.preference).to_owned(),
            endpoint_id: stored
                .host_key
                .map(|key| key.to_string())
                .unwrap_or_default(),
            bound_addrs: Vec::new(),
            relay_mode: match settings.relay {
                crate::config::RelayPolicy::Default => "default",
                crate::config::RelayPolicy::Disabled => "disabled",
                crate::config::RelayPolicy::Custom { .. } => "custom",
            }
            .to_owned(),
            relay: None,
            relay_connected: false,
            relay_latency_ms: None,
            warning: None,
        },
        devices,
        live: Vec::new(),
        devices_note,
    })
}

/// This device's identity and saved hosts (the connecting side).
struct ClientView {
    device_key: Option<String>,
    saved_hosts: Vec<SavedHost>,
}

fn client_view(paths: &bmux_config::ConfigPaths) -> Result<ClientView> {
    let key_file = paths
        .state_dir
        .join("connections")
        .join("devices")
        .join(format!("{CLIENT_PROFILE}.key"));
    let device_key = if key_file.is_file() {
        Some(
            crate::identity::DeviceKey::load_or_create(&paths.state_dir, CLIENT_PROFILE)?
                .public()
                .to_string(),
        )
    } else {
        None
    };
    Ok(ClientView {
        device_key,
        saved_hosts: crate::connector::SavedHostConnector::new(paths.clone()).saved_hosts()?,
    })
}

pub async fn status(context: &NativeCommandContext) -> Result<i32> {
    let args = &context.arguments;
    ensure!(
        positional(args, &[]).is_empty(),
        "usage: bmux remote status [--json]"
    );
    let paths = super::paths(context);
    let host = host_view(&paths).await?;
    let client = client_view(&paths)?;
    if flag(args, "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&status_json(&host, &client))?
        );
    } else {
        print_status(&host, &client);
    }
    Ok(0)
}

fn device_json(device: &Device) -> Value {
    json!({
        "name": device.name,
        "device_key": device.device_key,
        "principal": device.principal.to_string(),
        "state": state_name(device.state),
        "role": role_name(device.role),
        "revision": device.revision,
        "approved_at": device.approved_at,
        "last_seen": device.last_seen,
    })
}

fn status_json(host: &HostView, client: &ClientView) -> Value {
    let status = &host.status;
    json!({
        "host": {
            "server_running": host.server_running,
            "enabled": status.enabled,
            "source": status.source,
            "reason": explain_source(status.enabled, &status.source),
            "endpoint_id": (!status.endpoint_id.is_empty()).then_some(&status.endpoint_id),
            "bound_addrs": status.bound_addrs,
            "relay_mode": status.relay_mode,
            "relay": status.relay,
            "relay_connected": status.relay_connected,
            "relay_latency_ms": status.relay_latency_ms,
            "warning": status.warning,
            "devices": host.devices.iter().map(device_json).collect::<Vec<_>>(),
            "devices_unavailable": host.devices_note,
            "live_connections": host.live.iter().map(|live| json!({
                "device_key": live.device_key,
                "name": live.name,
                "path": live.path,
                "rtt_ms": live.rtt_ms,
                "connected_secs": live.connected_secs,
            })).collect::<Vec<_>>(),
        },
        "client": {
            "device_key": client.device_key,
            "saved_hosts": client.saved_hosts.iter().map(|saved| json!({
                "alias": saved.alias,
                "host_key": saved.host_key,
                "last_path": saved.last_path,
            })).collect::<Vec<_>>(),
        },
    })
}

fn print_status(host: &HostView, client: &ClientView) {
    let status = &host.status;
    if !host.server_running {
        println!("bmux server: not running for this runtime (showing stored state)");
    }
    println!(
        "Remote access: {} ({})",
        if status.enabled {
            "enabled"
        } else {
            "disabled"
        },
        status.source
    );
    println!("  {}", explain_source(status.enabled, &status.source));
    if status.endpoint_id.is_empty() {
        println!("Host key: none yet (created when this machine first hosts)");
    } else {
        println!("Host key: {}", status.endpoint_id);
    }
    if !status.bound_addrs.is_empty() {
        println!("Bound: {}", status.bound_addrs.join(", "));
    }
    let relay = match (&status.relay, status.enabled) {
        (Some(url), _) => format!(
            " ({url}, {}{})",
            if status.relay_connected {
                "connected"
            } else {
                "not connected"
            },
            status
                .relay_latency_ms
                .map_or_else(String::new, |ms| format!(", latency {ms}ms"))
        ),
        (None, true) if status.relay_mode != "disabled" => " (no home relay yet)".to_owned(),
        (None, _) => String::new(),
    };
    println!("Relay: {}{relay}", status.relay_mode);
    if let Some(warning) = &status.warning {
        println!("Warning: {warning}");
    }
    println!();
    if let Some(note) = &host.devices_note {
        println!("Paired devices unavailable: {note}");
    } else {
        print_devices(&host.devices);
    }
    if host.server_running {
        println!();
        if host.live.is_empty() {
            println!("No live remote connections.");
        } else {
            println!("Live connections:");
            for connection in &host.live {
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
    }
    println!();
    match &client.device_key {
        Some(key) => println!("This device: {key}"),
        None => println!("This device: no device key yet (created when it first pairs)"),
    }
    if client.saved_hosts.is_empty() {
        println!("No saved hosts. Pair one with `bmux connect --pair`.");
    } else {
        println!("Saved hosts:");
        for saved in &client.saved_hosts {
            println!(
                "  {:<20} {:<13} last path {}",
                saved.alias,
                short(&saved.host_key),
                saved.last_path.as_deref().unwrap_or("-")
            );
        }
    }
}

pub async fn log(context: &NativeCommandContext) -> Result<i32> {
    let args = &context.arguments;
    ensure!(
        positional(args, &["--limit"]).is_empty(),
        "usage: bmux remote log [--limit N] [--json]"
    );
    let limit = option(args, "--limit").map_or(Ok(DEFAULT_LOG_LIMIT), |text| {
        text.parse::<u32>()
            .ok()
            .filter(|limit| (1..=MAX_LOG_LIMIT).contains(limit))
            .ok_or_else(|| anyhow::anyhow!("--limit takes 1..={MAX_LOG_LIMIT}"))
    })?;
    let paths = super::paths(context);
    let events = if let Some(mut client) = running_server(&paths).await {
        hosting_admin_state::client::audit_log(&mut client, limit).await?
    } else {
        offline_trust(&paths.state_dir, |trust| trust.audit(limit as usize))?
            .unwrap_or_default()
            .into_iter()
            .map(audit_event)
            .collect()
    };
    if flag(args, "--json") {
        let events: Vec<_> = events.iter().map(event_json).collect();
        println!("{}", serde_json::to_string_pretty(&events)?);
        return Ok(0);
    }
    if events.is_empty() {
        println!("No remote access events.");
        return Ok(0);
    }
    println!(
        "{:<10} {:<14} {:<20} {:<10} DETAIL",
        "WHEN", "EVENT", "DEVICE", "ROLE"
    );
    for event in &events {
        println!(
            "{:<10} {:<14} {:<20} {:<10} {}",
            ago(Some(event.at)),
            kind_name(event.kind),
            event
                .device_name
                .clone()
                .or_else(|| event.device_key.as_deref().map(|key| short(key).to_owned()))
                .unwrap_or_else(|| "-".to_owned()),
            event.role.map_or("-", role_name),
            event.detail.as_deref().unwrap_or("")
        );
    }
    Ok(0)
}

const fn kind_name(kind: AuditEventKind) -> &'static str {
    match kind {
        AuditEventKind::Paired => "paired",
        AuditEventKind::PairRejected => "pair-rejected",
        AuditEventKind::Revoked => "revoked",
        AuditEventKind::RoleChanged => "role-changed",
        AuditEventKind::Connected => "connected",
        AuditEventKind::Disconnected => "disconnected",
        AuditEventKind::Refused => "refused",
    }
}

fn event_json(event: &AuditEvent) -> Value {
    json!({
        "at": event.at,
        "kind": kind_name(event.kind),
        "device_key": event.device_key,
        "device_name": event.device_name,
        "role": event.role.map(role_name),
        "detail": event.detail,
    })
}
