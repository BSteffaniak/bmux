//! Transport admission only exposes independently authenticated protocols.
//! A paired device is neither a cluster member nor a cluster principal.
use bmux_plugin::{ServiceCaller, StreamOrigin, TypedServiceCaller};
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
        "cluster-consensus-rpc/v1",
        &["vote"],
    ),
    (
        "bmux.server_clusters.auth",
        "cluster-consensus-rpc/v2",
        &["append"],
    ),
    (
        "bmux.server_clusters.auth",
        "cluster-consensus-transfer/v1",
        &["begin", "chunk", "finish", "abort"],
    ),
    (
        "bmux.server_clusters.auth",
        "cluster-consensus-snapshot/v1",
        &["install"],
    ),
];

/// Whether an admitted (remote) caller may use this exact cluster protocol
/// operation. The server already checked the route's BPDL access class; the
/// cluster also keeps remote peers to the protocols that carry their own
/// signatures and proofs, never its local administration.
pub fn remote_protocol(
    capability: &str,
    kind: ServiceKind,
    interface: &str,
    operation: &str,
) -> bool {
    kind == ServiceKind::Command
        && REMOTE_PROTOCOLS
            .iter()
            .any(|(protocol_capability, protocol, operations)| {
                *protocol_capability == capability
                    && *protocol == interface
                    && operations.contains(&operation)
            })
}

/// Every remote protocol operation, for tests.
#[cfg(test)]
pub fn remote_protocol_operations() -> Vec<(&'static str, &'static str, &'static str)> {
    REMOTE_PROTOCOLS
        .iter()
        .flat_map(|(capability, interface, operations)| {
            operations
                .iter()
                .map(move |operation| (*capability, *interface, *operation))
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
    if context.cancellation.is_cancelled() {
        return Err(denied("cancelled"));
    }
    // Origin is a host-intrinsic fact of this invocation (its authenticated
    // context or command route), never a domain-service answer: asking the
    // clients plugin re-entered dispatch and is not an admitted route.
    let caller = TypedServiceCaller::from_service_context(context);
    let origin = caller.current_stream_origin().map_err(|error| {
        // Keep the wire denial generic, but retain the cause for operators.
        tracing::warn!(%error, "cluster admission provenance unavailable");
        denied("provenance_unavailable")
    })?;
    match origin {
        StreamOrigin::Local | StreamOrigin::HostInternal => Ok(()),
        StreamOrigin::Admitted { .. } => {
            let service = &context.request.service;
            if !remote_protocol(
                service.capability.as_str(),
                service.kind,
                &service.interface_id,
                &context.request.operation,
            ) {
                return Err(denied("protocol_not_admitted"));
            }
            // The host's provenance lookup is live: a revoked binding reads as
            // Invalid. Do not substitute a caller-chosen subject or an
            // owner-only pairing query.
            if context.cancellation.is_cancelled() {
                return Err(denied("cancelled"));
            }
            // Device and node identity remain independent. Protocol handlers
            // verify request-bound signatures, credentials and transitions.
            Ok(())
        }
        StreamOrigin::Invalid => Err(denied("invalid_origin")),
    }
}
