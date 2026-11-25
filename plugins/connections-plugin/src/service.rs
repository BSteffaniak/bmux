//! Service routing. Hosting administration is `owner-local` in BPDL (never
//! reachable from a remote stream); handlers also require a local caller.
use crate::host::Host;
use crate::roles::PermissionsGrants;
use crate::trust::{Device, DeviceState, Role};
use bmux_connections_plugin_api::hosting_types::{self as api, DeviceRole, PairingEventKind};
use bmux_connections_plugin_api::{
    connections_commands, hosting_admin_state, hosting_commands, hosting_state,
};
use bmux_plugin::ServiceCaller;
use bmux_plugin_sdk::prelude::*;
use std::time::Duration;

const MAX_EVENT_WAIT: Duration = Duration::from_secs(30);

fn failed(code: &str) -> impl Fn(anyhow::Error) -> ServiceResponse + '_ {
    move |error| ServiceResponse::error(code, format!("{error:#}"))
}

fn require_local(context: &NativeServiceContext) -> Result<(), ServiceResponse> {
    let caller = bmux_plugin::TypedServiceCaller::from_service_context(context);
    match caller.current_stream_origin() {
        Ok(bmux_ipc::StreamProvenance::Local) => Ok(()),
        _ => Err(ServiceResponse::error(
            "owner_required",
            "remote hosting administration requires the local owner",
        )),
    }
}

/// Typed endpoint calls come from local callers or host-internal plugin work.
fn require_local_or_internal(context: &NativeServiceContext) -> Result<(), ServiceResponse> {
    let caller = bmux_plugin::TypedServiceCaller::from_service_context(context);
    match caller.current_stream_origin() {
        Ok(bmux_ipc::StreamProvenance::Local | bmux_ipc::StreamProvenance::HostInternal) => Ok(()),
        _ => Err(ServiceResponse::error(
            "owner_required",
            "endpoint invocation requires a local caller",
        )),
    }
}

fn host(host: Option<&Host>) -> Result<&Host, ServiceResponse> {
    host.ok_or_else(|| {
        ServiceResponse::error(
            "unavailable",
            "remote hosting is not active in this process",
        )
    })
}

const fn role(role: DeviceRole) -> Role {
    match role {
        DeviceRole::Full => Role::Full,
        DeviceRole::ViewOnly => Role::ViewOnly,
    }
}

const fn api_role(role: Role) -> DeviceRole {
    match role {
        Role::Full => DeviceRole::Full,
        Role::ViewOnly => DeviceRole::ViewOnly,
    }
}

fn device(device: Device) -> api::Device {
    api::Device {
        device_key: device.key,
        principal: device.principal,
        name: device.name,
        state: match device.state {
            DeviceState::Pending => api::DeviceState::Pending,
            DeviceState::Active => api::DeviceState::Active,
            DeviceState::Revoked => api::DeviceState::Revoked,
        },
        role: api_role(device.role),
        revision: device.revision,
        approved_at: device.approved_at,
        last_seen: device.last_seen,
    }
}

fn status(host: &Host) -> api::HostingStatus {
    let status = host.status();
    api::HostingStatus {
        enabled: status.enabled,
        source: status.source.to_owned(),
        endpoint_id: status.endpoint_id,
        bound_addrs: status.bound_addrs,
        relay_mode: status.relay_mode.to_owned(),
        relay: status.relay,
        warning: status.warning,
    }
}

fn event(event: crate::pairing::Event) -> api::PairingEvent {
    use crate::pairing::Event;
    let empty = |kind| api::PairingEvent {
        kind,
        request_id: None,
        device_name: None,
        device_key: None,
        phrase: None,
        reason: None,
    };
    match event {
        Event::Request {
            request_id,
            device_name,
            device_key,
            phrase,
        } => api::PairingEvent {
            request_id: Some(request_id),
            device_name: Some(device_name),
            device_key: Some(device_key),
            phrase: Some(phrase),
            ..empty(PairingEventKind::Request)
        },
        Event::Withdrawn { request_id } => api::PairingEvent {
            request_id: Some(request_id),
            ..empty(PairingEventKind::Withdrawn)
        },
        Event::Ended { reason } => api::PairingEvent {
            reason: Some(
                serde_json::to_value(reason)
                    .ok()
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_default(),
            ),
            ..empty(PairingEventKind::Ended)
        },
        Event::Idle => empty(PairingEventKind::Idle),
    }
}

// The generated-operation routing table reads best as one list.
#[allow(clippy::too_many_lines)]
pub fn invoke(owner: Option<&Host>, context: &NativeServiceContext) -> ServiceResponse {
    bmux_plugin_sdk::route_service!(context, {
        hosting_state::client::StatusEndpoint => |(): (), _ctx| {
            Ok::<_, ServiceResponse>(status(host(owner)?))
        },
        hosting_admin_state::client::ListDevicesEndpoint => |(): (), ctx| {
            require_local(ctx)?;
            let caller = bmux_plugin::TypedServiceCaller::from_service_context(ctx);
            let devices = host(owner)?
                .list_devices(&PermissionsGrants { caller: &caller })
                .map_err(failed("trust_store"))?;
            Ok::<_, ServiceResponse>(devices.into_iter().map(device).collect::<Vec<_>>())
        },
        hosting_admin_state::client::LiveConnectionsEndpoint => |(): (), ctx| {
            require_local(ctx)?;
            Ok::<_, ServiceResponse>(
                host(owner)?
                    .live_connections()
                    .into_iter()
                    .map(|live| api::LiveConnection {
                        device_key: live.device_key,
                        name: live.device_name,
                        path: live.path.to_owned(),
                        rtt_ms: live.rtt.map(|rtt| u64::try_from(rtt.as_millis()).unwrap_or(u64::MAX)),
                        connected_secs: live.connected_secs,
                    })
                    .collect::<Vec<_>>(),
            )
        },
        hosting_commands::client::SetEnabledEndpoint => |request, ctx| {
            require_local(ctx)?;
            let host = host(owner)?;
            let caller = bmux_plugin::TypedServiceCaller::from_service_context(ctx);
            host.set_enabled(request.enabled, &PermissionsGrants { caller: &caller })
                .map_err(failed("hosting"))?;
            Ok::<_, ServiceResponse>(status(host))
        },
        hosting_commands::client::BeginPairingEndpoint => |request, ctx| {
            require_local(ctx)?;
            let opened = host(owner)?
                .begin_pairing(
                    role(request.role),
                    request.name,
                    Duration::from_secs(request.ttl_seconds),
                )
                .map_err(failed("pairing"))?;
            Ok::<_, ServiceResponse>(api::PairingSession {
                session_id: opened.session_id,
                link: opened.link,
                expires_at: opened.expires_at,
            })
        },
        hosting_commands::client::NextPairingEventEndpoint => |request, ctx| {
            require_local(ctx)?;
            let wait = Duration::from_millis(request.wait_ms).min(MAX_EVENT_WAIT);
            host(owner)?
                .next_pairing_event(&request.session_id, wait)
                .map(event)
                .map_err(failed("pairing"))
        },
        hosting_commands::client::DecidePairingEndpoint => |request, ctx| {
            require_local(ctx)?;
            let caller = bmux_plugin::TypedServiceCaller::from_service_context(ctx);
            host(owner)?
                .decide_pairing(
                    &request.session_id,
                    &request.request_id,
                    request.approve,
                    &PermissionsGrants { caller: &caller },
                )
                .map(|approved| approved.map(device))
                .map_err(failed("pairing"))
        },
        hosting_commands::client::EndPairingEndpoint => |request, ctx| {
            require_local(ctx)?;
            host(owner)?.end_pairing(&request.session_id);
            Ok::<_, ServiceResponse>(true)
        },
        hosting_commands::client::RevokeDeviceEndpoint => |request, ctx| {
            require_local(ctx)?;
            let caller = bmux_plugin::TypedServiceCaller::from_service_context(ctx);
            host(owner)?
                .revoke(&request.device, &PermissionsGrants { caller: &caller })
                .map(device)
                .map_err(failed("revoke"))
        },
        hosting_commands::client::SetDeviceRoleEndpoint => |request, ctx| {
            require_local(ctx)?;
            let caller = bmux_plugin::TypedServiceCaller::from_service_context(ctx);
            host(owner)?
                .set_role(&request.device, role(request.role), &PermissionsGrants { caller: &caller })
                .map(device)
                .map_err(failed("role"))
        },
        connections_commands::client::InvokeServiceEndpoint => |request, ctx| {
            require_local_or_internal(ctx)?;
            let paths = crate::cli::paths_from(&ctx.connection);
            let cancellation = ctx.cancellation.clone();
            crate::host::wait(async move {
                crate::invoke::invoke(&paths, &cancellation, request.invocation).await
            })
            .map_err(failed("unavailable"))
        },
    })
}
