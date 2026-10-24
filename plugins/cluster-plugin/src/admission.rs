//! Transport admission only exposes independently authenticated protocols.
//! A paired device is neither a cluster member nor a cluster principal.
use bmux_plugin::{ServiceCallerDispatchClient, TypedServiceCaller, block_on_typed_dispatch};
use bmux_plugin_sdk::{NativeServiceContext, ServiceKind, ServiceResponse};

const REMOTE_PROTOCOLS: &[(&str, &str, &[&str])] = &[
    (
        "bmux.server_clusters.auth",
        "cluster-peer-read/v1",
        &["read_linearizable"],
    ),
    (
        "bmux.server_clusters.write",
        "cluster-command/v1",
        &[
            "redeem_enrollment",
            "accept_leave",
            "credential_rotate_accept",
            "consensus_reconcile_members",
            "consensus_remove_voter",
        ],
    ),
    (
        "bmux.server_clusters.auth",
        "cluster-peer-auth/v1",
        &["challenge", "authenticate"],
    ),
    (
        "bmux.server_clusters.auth",
        "cluster-raft-rpc/v1",
        &["append_entries", "install_snapshot", "vote"],
    ),
];

pub fn routes() -> Vec<bmux_plugin::stream_admission::StreamAdmittedRoute> {
    REMOTE_PROTOCOLS
        .iter()
        .flat_map(|(capability, interface, operations)| {
            operations.iter().map(move |operation| {
                bmux_plugin::stream_admission::StreamAdmittedRoute {
                    capability: (*capability).into(),
                    kind: ServiceKind::Command,
                    interface_id: (*interface).into(),
                    operation: (*operation).into(),
                }
            })
        })
        .collect()
}

fn denied(reason: &'static str) -> ServiceResponse {
    tracing::debug!(reason, "cluster admission denied");
    ServiceResponse::error(
        "cluster_admission_denied",
        format!("cluster protocol origin is not authorized ({reason})"),
    )
}

pub fn authorize(context: &NativeServiceContext) -> Result<(), ServiceResponse> {
    use bmux_clients_plugin_api::clients_identity::{self, StreamProvenance};
    if context.cancellation.is_cancelled() {
        return Err(denied("cancelled"));
    }
    let caller = TypedServiceCaller::from_service_context(context);
    let mut client = ServiceCallerDispatchClient::new(&caller);
    let origin = block_on_typed_dispatch(clients_identity::client::current_stream_provenance(
        &mut client,
    ))
    .map_err(|_| denied("provenance_dispatch_unavailable"))?
    .map_err(|_| denied("provenance_unavailable"))?;
    match origin {
        StreamProvenance::Local | StreamProvenance::HostInternal => Ok(()),
        StreamProvenance::Admitted { .. } => {
            let service = &context.request.service;
            if !routes().iter().any(|route| {
                route.capability == service.capability.as_str()
                    && route.kind == service.kind
                    && route.interface_id == service.interface_id
                    && route.operation == context.request.operation
            }) {
                return Err(denied("protocol_not_admitted"));
            }
            // The host registry lookup revalidates the retained lease against
            // the admission authority, including revocation. Do not substitute
            // a caller-chosen subject or an owner-only pairing query.
            if context.cancellation.is_cancelled() {
                return Err(denied("cancelled"));
            }
            // Device and node identity remain independent. Protocol handlers
            // verify request-bound signatures, credentials and transitions.
            Ok(())
        }
        StreamProvenance::Invalid => Err(denied("invalid_lease")),
    }
}
