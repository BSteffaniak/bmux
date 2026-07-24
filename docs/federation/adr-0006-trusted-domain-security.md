# ADR-0006: Trusted-Domain Membership, Delegation, Authorization, and Revocation

- **Status:** Accepted
- **Date:** 2026-07-23

## Context

The initial federation target connects machines in one trusted user/administrative domain. Trusting cluster nodes does not justify replacing the originating user with the ingress node's identity. A compromised or misconfigured ingress must not be able to widen permissions when forwarding operations to a leader or worker.

Cluster membership and user permissions are related but distinct. Node credentials establish which machines may participate; principal identity and workspace permissions establish which user actions are allowed.

## Decision

Use persistent cryptographic node identity, mutually authenticated node links, short-lived enrollment, and end-to-end principal delegation. Authorization is checked at ingress for early rejection and rechecked by the authoritative service/worker before side effects.

## Identity layers

- **Cluster identity:** persistent `ClusterId` and cluster trust root/public authority.
- **Node identity:** persistent `NodeId` bound to a node keypair and cluster-issued membership credential.
- **Principal identity:** stable user/admin identity already used by bmux, extended rather than replaced for federation.
- **Client connection identity:** transient connection/client ID; never used as the sole cross-node authorization identity.

Private keys remain local. Replicated state contains only public credentials, roles, serials, validity, and revocation metadata safe for voters.

## Cluster initialization

`cluster init`:

1. Generates a cluster identity and trust authority using secure local entropy.
2. Generates or binds the initializing node identity.
3. Creates the initial one-voter membership with an explicit non-HA warning until quorum is expanded.
4. Writes private material with owner-only permissions using crash-safe replacement.
5. Records public trust and membership state in the initial consensus state.

Export/backup of trust authority is an explicit, protected operational action and never part of ordinary diagnostics.

## Enrollment and join

Enrollment tokens are:

- Single-use
- Short-lived; initial maximum validity **10 minutes**
- Bound to cluster identity
- Bound to requested/allowed roles and optional node name/labels
- Random and unguessable
- Stored only as a verifier/hash where feasible
- Redacted from logs and diagnostics

Join flow:

1. Candidate establishes a server-authenticated bootstrap channel using a pinned invitation/trust root.
2. Candidate proves possession of its new node private key.
3. Existing authority validates token, requested roles, compatibility, and policy.
4. Control plane commits membership intent/credential serial before privileges activate.
5. Candidate receives a signed membership credential and current public trust/membership snapshot.
6. Token is durably consumed; replay is rejected.
7. Voter promotion uses the consensus library's safe membership procedure after catch-up.

Possessing a transport target or account credential alone does not grant cluster membership.

## Node-to-node authentication

All peer and worker RPC uses mutual authentication and verifies:

- Cluster ID
- Node ID and credential chain/signature
- Credential validity and revocation
- Advertised role/capability against committed membership
- Protocol compatibility
- Channel binding/replay protection provided by the transport

SSH, TLS, and Iroh may provide different transport mechanics, but they must produce the same authenticated peer identity semantics to the cluster plugin.

## Delegation

When ingress forwards a user operation, it presents a signed short-lived delegation with:

- Cluster ID
- Original principal ID and authentication strength/context
- Issuing ingress node ID
- Audience node/service
- Workspace/logical resource scope
- Exact action or capability scope
- Command ID for mutations
- Control term/revision where relevant
- Issued-at, expiration, and nonce/delegation ID

Initial maximum delegation lifetime is **30 seconds** and should normally be shorter than the request deadline. Delegation cannot grant capabilities absent from the principal's current authorization or the ingress node's forwarding role.

A receiving service validates signature, audience, scope, expiry, command binding, committed issuer membership, and current revocation state. It then performs the normal typed permissions check using the original principal. It does not trust an ingress-provided role string as an authorization decision.

Delegation is transitive only when explicitly permitted by the token and protocol. Default is one forwarding hop; leader forwarding to a worker uses a newly derived delegation preserving the original principal and command chain.

## Authorization points

- Ingress: authenticate, resolve target, reject obviously unauthorized requests, issue constrained delegation.
- Consensus leader: recheck authorization for replicated mutations and bind the decision inputs to the command.
- Worker: validate delegation/fencing and recheck execution-affecting permission before side effects.
- Attach data path: validate write permission at attach/open and on authority renewal; permission revocation terminates future input authority.

Core provides generic principal transport and typed service invocation only. Permissions remain in the permissions plugin; cluster code invokes its generated services.

## Revocation

- Node revocation is a committed control mutation and includes credential serial/node identity.
- A revoked node cannot receive new leases/delegations or participate as an authorized worker/ingress.
- Voter removal and credential revocation are coordinated; emergency revocation prioritizes denying application authority while consensus membership is safely changed.
- Principal/permission revocation prevents new delegations and lease renewal immediately after the revocation revision is authoritative.
- Existing delegation/interactive lease exposure is bounded by its maximum lifetime. Critical revocation may push explicit cancellation to reachable workers but does not rely on delivery.
- A partitioned node with expired authority cannot continue accepting mutations after the bounded lease window.

## Secrets and audit

Never record:

- Private node/cluster keys
- Enrollment token plaintext
- SSH private keys
- Raw user credentials
- Raw terminal input/output as security audit data

Audit records include actor principal, issuing/receiving nodes, command ID, resource IDs, action, authorization outcome, control revision, generation, and reason code. Security-sensitive values are redacted by construction.

Required audited events include enrollment issuance/use/failure, join/leave/revocation, role changes, permission denial, delegation rejection, placement/restart/move, stale fencing, failover, promotion, and destructive recovery.

## Threat model

The first version addresses:

- Network interception and peer impersonation
- Enrollment replay/theft
- Forged, replayed, expired, wrong-audience, and widened delegations
- Stale/partitioned node authority
- Confused-deputy forwarding
- Protocol downgrade attempts
- Accidental secret logging

A fully compromised authorized voter can disrupt availability and may participate in quorum according to consensus assumptions. Byzantine consensus and hostile multi-tenant isolation are not initial goals. Compromise response relies on revocation, quorum membership change, credential rotation, and recovery procedures.

## Consequences

### Positive

- Routing through another member preserves user accountability and permission boundaries.
- Transport differences do not change cluster identity semantics.
- Stale authorization is bounded by short credential/lease lifetimes.
- Secrets remain outside replicated and diagnostic state.

### Costs

- Node trust bootstrapping and rotation require operational tooling.
- Permission checks occur at multiple authoritative layers.
- Emergency removal must coordinate application revocation and consensus safety.

## Rejected alternatives

- **Treat every cluster node as root for all users:** rejected because ingress compromise would bypass permissions and audit attribution.
- **Forward only ingress identity:** rejected because the worker cannot authorize the originating principal.
- **Long-lived bearer join tokens:** rejected due to replay and leakage risk.
- **Store private trust keys in consensus:** rejected because all voters would expose unnecessary high-value secrets.
- **Rely solely on transport authentication:** rejected because authenticated nodes still need action and principal scope.

## Architecture placement

Membership/delegation orchestration lives in cluster plugins. Stable identity/delegation wire records live in `cluster-plugin-api` where domain-specific. Permission policy and role state remain in permissions plugins. Generic cryptographic/transport helpers may be neutral crates, but core cannot decide cluster roles or workspace permissions.

## Acceptance criteria

Tests must reject token replay, expired enrollment, forged/wrong-audience/widened delegations, revoked nodes/principals, stale lease use, incompatible downgrade, and secret leakage while proving valid principal identity is preserved across ingress, leader, and worker hops.
