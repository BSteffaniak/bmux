# Remote access architecture

The end state for reaching a bmux host from another computer over the internet: pair once,
then `bmux connect desktop` gives the same attach as running `bmux` on the desktop (every
session, tab, pane, the sidebar, mouse, images, scrollback). No account, no port
forwarding, and no server on the connecting machine. This document is both the plan and
the record of why. It replaces the current paired-terminal design rather than extending
it.

Status: **planned**. Nothing here is implemented yet unless the phase table says so.

## Decisions

- **Clean break.** Existing pairings, the invitation store, legacy identity imports and
  preference migrations are dropped. Users re-pair once; no migration code is written.
- **One host key per machine** (per user state directory), not per runtime path. A paired
  device reaches every runtime on that machine; the runtime is selected in `Hello`.
- **Roles.** The first version has two roles, chosen when approving a device: `full`
  (acts as the host user) and `view-only`. Session-scoped grants come later, and they
  require filtering events per connection (see Authorization).
- **No SSH transport for now.** `ssh://` connecting is out of scope. It can be added later
  as another connector (see Client).
- **Hosting policy is unchanged.** Hosting is off by default; `bmux remote enable`
  persists an opt-in; an explicit `allow_full_runtime_ingress = false` is a hard
  prohibition.

## Why: the current state

Measured or read on `a6adfab0`:

- **Remote access stops after about 60 seconds.** Admission leases expire 60 s after they
  are issued (`plugins/connections-plugin/src/admission.rs:116`) and nothing renews them.
  The server checks every 250 ms and treats the expiry as a revocation. A paired terminal
  that sits idle for 75 s then fails with `trusted caller provenance invalid or revoked`.
  This was reproduced with an isolated host and client; the existing smoke test passes
  only because it finishes within a minute.
- **Remote attach is a separate product.** `bmux connect host` starts a new `/bin/sh` at
  80×24 (`terminal_attach.rs:562`), not the host's sessions. It polls the whole screen as
  JSON every 100 ms and sends each read as a reset frame. Every keystroke is a request
  that waits for its reply, and mouse input and layout actions are refused.
- **Admitted streams cannot attach.** The server accepts only `InvokeService*` on them
  (`packages/server/src/lib.rs:3345`), so they get no event push and no attach, tabs or
  sessions routes. Plugins also approve their own admitted routes at bootstrap
  (`packages/cli/src/runtime/bootstrap.rs:669-723`).
- **Two authentication layers.** Iroh's QUIC handshake already authenticates both
  computers' keys. On top of it run a separate device key, a signed challenge, a
  single-use ticket and a lease. The listener then copies bytes into the host's Unix
  socket and replays the ticket to the server (`listener.rs:344-367`). Each validation
  spawns an OS thread and builds a tokio runtime (`server/src/lib.rs:379-428`). If
  validation capacity runs out, healthy streams are torn down as if revoked.
- **Overlapping, dead and broken paths.**
  - Two enrollment mechanisms: invitation redeem, plus a file workflow whose `Offer` no
    command consumes.
  - `hosted/` (about 2.2k lines): an account control plane whose `setup` launches a
    refused command and then times out.
  - Refused TLS, SSH and legacy Iroh gateways whose code remains after the refusal.
  - A duplicate CLI remote stack (`packages/cli/src/runtime/remote_cli.rs`, 2.8k lines).
  - `iroh://` parsed in three places.
  - `access` and `trust` commands that feed only refused gateways.
- **Cross-host cluster attach cannot work.** The CLI's attach endpoint connector rejects
  `device://` targets, dials `iroh://` with the refused ALPN, and needs event push, which
  admitted streams forbid.
- **Smaller defects:**
  - Invitations are never pruned; at 4096 of them, pairing fails permanently.
  - A new Iroh endpoint is bound for every dial.
  - Pairing status is polled every second on both sides.
  - Values are hard-coded: the `"writer"` grant, the `"default"` profile (about 20
    sites), and 80×24 layouts in the cluster client.
  - The pairing smoke script creates its runtime directory 0755, which local-IPC
    hardening now refuses. It is not run in CI.
  - `cargo-deny` 0.18.2 cannot parse the current advisory database, so advisories are
    not checked.

## Principle

**A paired device is a principal, and remote attach is local attach over another
stream.** Local and remote use the same attach code. Only three things are remote-specific,
each owned by one component:

1. how the stream is established: the connections plugin;
2. who the principal is: the stream's provenance binding;
3. what that principal may do: the permissions plugin, through per-route access classes.

Core stays domain-agnostic. It gains generic primitives (stream ingress with a provenance
binding, route access classes, a transport connector registry), and none of them knows
about pairing, devices or Iroh.

## Target architecture

```text
 connecting machine (no server)                     host machine
 ┌──────────────────────────────┐                   ┌─────────────────────────────────────┐
 │ bmux connect desktop         │                   │ connections plugin                  │
 │  └ transport connector ──────┼── QUIC (Iroh) ───▶│  listener: ALPN bmux/session/1      │
 │     (connections frontend)   │  direct or relay  │   peer key ∈ active devices?        │
 │     one endpoint/process     │                   │   └ hand stream + binding to server │
 │     one connection/host      │                   │ server: handle_connection(stream,   │
 │  └ normal attach loop        │◀─ frames pushed ──│          provenance = subject)       │
 │     (same as local)          │── input one-way ─▶│  route class check → permissions    │
 └──────────────────────────────┘                   └─────────────────────────────────────┘
```

### 1. Identity

- **Host key.** One Iroh secret key per machine, under the user state directory
  (`state_dir/connections/host.key`). It does not depend on the runtime path, so moving or
  renaming a runtime directory keeps the pairing.
- **Device key.** One per client profile (`state_dir/connections/devices/<profile>.key`),
  used directly as that client's Iroh endpoint key. The QUIC/TLS handshake proves the host
  key (the client dials by endpoint id) and the device key (the host reads
  `Connection::remote_id()`). There is no challenge, ticket or lease.
- **Principal.** Each approved device maps to a stable principal UUID on the host, issued
  at approval. The permissions plugin holds its role. Device names are untrusted labels.
- **Key purposes stay separate.** A cluster node key is never a device key (the existing
  cluster decision stands).

### 2. Trust stores

- **Host trust store** (connections plugin): device key → {principal, name, state
  (`pending`/`active`/`revoked`), revision, approved_at, last_seen}.
  - It is held in memory as the authority, with crash-safe write-through to redb
    (already a workspace dependency).
  - The connection path never takes a file lock or parses a file.
  - A change publishes an event that the connection registry acts on (see Transport).
- **Grants** (permissions plugin): principal → role. Grants and devices are owned by
  different plugins, so approval is a recoverable transition:
  1. the device is written `pending`;
  2. the grant is committed;
  3. the device is written `active`.

  On startup, a `pending` device left by an interrupted approval is completed if its grant
  exists and removed otherwise. A device is never usable without its grant.
- **Client known-hosts store** (connections plugin, client side): alias → {pinned host
  key, address and relay hints, principal, last path, last runtime}.
  - Owner-only files with atomic replace, as today, but no legacy scans.
  - A changed host key is a hard error with an actionable message, never a silent
    re-pair.
- **Secrets never appear** in listings, diagnostics, logs or resume state.

### 3. Transport (connections plugin)

- **Connection manager.** There is one Iroh endpoint per process, and one QUIC connection
  per host, shared by every user in the process: the attach client, extra clients, and
  cluster RPC.
- **Separate streams per job.** Each `BmuxClient` is one QUIC bidirectional stream on the
  shared connection. Attach opens three: control, push (frames and events), and input.
  The server handles one connection's requests in order, so input never waits behind a
  slow read.
- **Two ALPNs.**
  - `bmux/pair/1` is accepted only while a `remote pair` session is open. It is bounded in
    frames, sizes and deadlines, and carries only the pairing exchange.
  - `bmux/session/1` is accepted only if the peer key is an `active` device. Otherwise the
    connection closes during the handshake, before any bmux bytes are read.
  - The only code an unpaired internet peer can reach is QUIC/TLS (Iroh, noq, rustls)
    and, during a pairing window, the pairing decoder.
- **Direct handoff to the server.** Core's `handle_connection` accepts any
  `ErasedIpcStream` with a provenance binding {subject, grant revision}. The listener
  passes each accepted QUIC stream in directly. The Unix-socket hop, the `AdmitStream`
  preamble and the ticket replay are deleted.
- **Revocation is pushed, not polled.** The listener keeps a registry of live connections
  by device. Revoking a device, changing its role, or disabling hosting closes that
  device's connections and rejects reconnects immediately. The 250 ms per-stream
  re-validation and the thread-per-validation bridge are deleted.
- **Relay.** The public Iroh relay is the default. Self-hosting uses `mode = "custom"`;
  `disabled` is for direct-only setups and tests.
- **Resilience.**
  - QUIC connection migration keeps the session across address changes (for example,
    switching Wi-Fi).
  - On a real drop: reconnect with capped backoff, authenticate again through the
    handshake, and resume the attach from a snapshot.
  - Input whose delivery is uncertain is never replayed; the client shows a reconnecting
    overlay and accepts no input until it resumes.
- **Compression.** zstd is negotiated per connection on the push stream (the transport
  support exists in `CompressedStream`).

### 4. Authorization (core server + permissions plugin)

- **Remote streams get the full protocol.** The request-kind gate is removed;
  `SubscribeEvents`, `EnableEventPush` and every other request work as they do locally.
- **Every service route declares an access class** in BPDL, as an interface attribute
  with optional per-operation overrides:

  | Class | Meaning | Examples |
  | --- | --- | --- |
  | `observe` | read state and receive output | layout and snapshot state, frame push, scene, tab and workspace lists |
  | `input` | change what runs or what is shown | pane input, resize, split, close, tab commands, new session |
  | `manage` | change durable configuration of the runtime | rename, policy-visible settings |
  | `owner-local` | never reachable remotely | pairing admin, enable/disable, plugin and config management, server stop |

- **Enforcement point.** For every request from a non-local principal, the server asks
  the authorization provider (the permissions plugin) whether the principal's role allows
  the route's class.
  - `view-only` allows `observe`.
  - `full` allows `observe`, `input` and `manage`.
  - Nothing allows `owner-local` remotely.
  - Routes that declare no class are denied for remote principals.
  - A plugin command an attach runs on the host (the CLI bridge's `run_plugin`, for
    keybindings such as `next-tab`) takes the class its manifest declares
    (`access = "input"` on the `[[commands]]` entry). Commands without one never run
    for a remote principal, and core CLI paths (`run_path`) are owner-local.
  - Attaching is `observe`. Whether an attachment may type or size the session is
    the session policy's answer (`pane.direct_input`, `pane.resize`); a view-only
    device attaches, selects what it views, and observes the shared geometry.
- **Missing plugins.** With no permissions plugin, non-local principals are denied, and
  local single-user behavior is unchanged. Without the connections plugin there is no
  remote ingress.
- **Admitted route contributions are removed.** The bootstrap self-approval mechanism
  (`bmux.stream-admitted-routes.v1`) goes; cluster protocol routes declare their classes
  like everything else. They also keep their own signature checks.
- **Events.** With whole-runtime roles, the global event broadcast is acceptable for
  remote clients. Session-scoped grants must not ship until events are filtered per
  connection.

### 5. Client and attach

- **Transport connector registry (core).** Core resolves a target to a plugin-provided
  `TransportConnector` that returns a `BmuxClient`, the same way attach providers are
  resolved. The connections plugin's frontend registers saved hosts. Core never parses
  pairing or Iroh targets.
- **`bmux connect <host> [session]`** resolves the host through the connector and runs
  the normal attach loop against it. Every attach feature works because it is the same
  code. OSC 52 copies go to the viewing machine's clipboard. `bmux connect` with no host
  offers a picker of saved hosts with online state.
- **Local-only assumptions removed from the attach path:**
  - Display capture writes to the server's `recording_path`, which assumes a shared
    filesystem. Over a remote stream, capture is server-side or explicitly unavailable.
  - The client's working directory is a hint, never authoritative.
  - `require_service_contracts` makes paired input wait for a reply per write. Paired
    streams use one-way input like local ones.
  - The CLI attach endpoint connector rejects paired hosts. It moves onto the connector
    registry.
- **Deleted:** the paired-terminal adapter (`terminal_attach.rs`, `attach_provider.rs`),
  and the legacy TLS, SSH and Iroh dial code in `packages/cli/src/connection.rs` and
  `packages/cli/src/runtime/remote_cli.rs`.

### 6. Pairing

- **Host:** `bmux remote pair [--role full|view-only] [--name NAME]`.
  - It prints a compact link (`bmux-pair:` followed by host key, relay hint and a
    256-bit secret) and a terminal QR code. The QR encoder is already a dependency.
  - It keeps a stream open and shows each request as it arrives.
- **Client:** `bmux connect --pair` prompts for the link (hidden input, never in argv or
  shell history).
  1. It dials `bmux/pair/1` with its device key and proves the link secret, bound to the
     transcript (host key, device key, secret, requested role, protocol version).
  2. Both sides show a phrase derived from the transcript.
  3. The owner approves on the host, and the trust-store transition commits.
  4. The client saves the host and attaches.
- **Pushed status.** Approval status is pushed over the open pairing stream, not polled.
- **Bounds.** The secret is single-use and expires (default 10 minutes). Attempts and
  concurrent requests per pairing session are bounded. Cancellation, expiry and rejection
  each leave a clear state. Pairing never grants cluster membership.
- **Deleted:** invitation redeem, `remote pair-request`, `remote approve`,
  `pair_endpoint`, and the 4096-invitation store.

### 7. Cluster and mobile

- **Cluster.** Node-to-node RPC and cluster attach use the connection manager. Cross-host
  cluster attach works once admitted principals get event push. The cluster client's
  hard-coded 80×24 layouts take the real viewport.
  - Members are saved hosts. A member's advertised endpoint is the alias every node saved
    it under, and each node also pairs with itself under its own name, since the leader
    reaches every worker, itself included, by endpoint. The connections plugin lists the
    saved hosts through its `owner-local` `saved-hosts-state` service; `local` is never a
    member.
  - Health checks (`cluster status`/`doctor`, `pane new`, `up`) make one read-only
    `hosting-state.status` call to the saved host: `observe`, single attempt, never
    retried or replaced by a local call.
  - Remote cluster routes stay an explicit list in the cluster plugin's admission:
    the signed consensus and membership protocols, plus worker launch, input and resize
    (signed leases), execution lookup, and the observe-leased worker stream.
  - Paired-transport round trips can outlast the 150 ms consensus heartbeat, so the
    engine keeps a probe's conflict hint even after a heartbeat replaced the probe.
  - A worker verifies leases with a linearizable membership read, so workers are
    enrolled as voters; an observer-edge worker cannot run panes yet.
- **Mobile.** The mobile FFI target model (`docs/mobile-ffi-contract.md`) replaces
  `ssh`/`tls`/`iroh` with "paired host" and reuses the client crate and connector.

### 8. Operations

- **`bmux remote status` (host):** whether hosting is enabled and the source of that
  setting; endpoint id; bound addresses; relay home and latency; paired devices with
  role, name and last seen; live connections with path (direct or relay) and RTT.
- **`bmux remote doctor <host>` (client):** timings for resolve, dial, path, handshake,
  authorize and attach. It gives a distinct error for each of: offline host, revoked
  device, changed host key, relay unreachable, and incompatible version.
- **Status item.** The attached client shows the connection path and RTT.
- **Audit log** of approvals, revocations and remote connections, with no secrets.
- **Client commands without a server:** `remote list`, `rename`, `forget` and `status`.
  `forget` is local only and is not revocation.

Lane G state:

- Done: `remote status [--json]` (the `hosting-state.status` record carries the relay's
  connection and latency; the source names the CLI opt-in, `config: auto_start`,
  `config: allowed` or `config: prohibited`); `remote doctor <host> [--json]` with timed
  `resolve`, `endpoint`, `relay`, `dial`, `handshake`, `hello` and `round-trip` steps;
  `remote log [--limit N] [--json]` over the owner-local `hosting-admin-state.audit-log`
  query (paired, pair-rejected, revoked, role-changed, connected, disconnected, refused;
  bounded to 1000 entries in `trust.redb`, refusals rate-limited); a revoked device is
  closed with the `revoked` code, so it learns why; a bare `bmux connect` on a terminal
  offers the saved hosts with their online state.
- Without a server: the saved-host list is `remote hosts` (with `rename` and `forget`),
  and `remote status` and `remote log` read stored state. `remote list` (paired devices)
  still asks the server: host commands do not see `--target`, so a stored-state fallback
  would answer `bmux --target <host> remote list` with this machine's devices.
- Reconnecting overlay: decided as a one-line notice on the restored terminal
  (`attach::reconnect`), since the attachment's view is torn down while it reconnects.
- Not done: the attached client's path and RTT status item. It needs a generic way for
  a `TransportConnector`-provided client to report connection facts to the attach
  chrome; no such generic channel exists yet.

## Deletions

All of these are deleted, not deprecated:

- **Connections plugin modules:**
  - `hosted/` (the `setup`, `host`, `join`, `share`, `unshare`, `hosts` and `auth`
    commands, and the `api.bmux.run` control plane);
  - the SSH/TLS client transports in `transport.rs`;
  - the `access` and `trust` command families;
  - `iroh://` parsing;
  - `remote_admin` command code for removed commands;
  - unreachable `remote_cli.rs` branches;
  - legacy identity and preference migration;
  - `admission.rs` (challenge, ticket, lease);
  - invitation redeem in `pairing.rs`;
  - `terminal_attach.rs` and `attach_provider.rs`.
- **CLI:**
  - `packages/cli/src/runtime/remote_cli.rs`;
  - the TLS/SSH/Iroh paths in `packages/cli/src/connection.rs`;
  - the refused gateway and bridge code in `server_commands.rs`.
- **Server:**
  - the `AdmitStream` preamble and ticket consumption;
  - the serialized admission-provider bridge;
  - the lease monitor;
  - `admitted_routes`, `approve_admitted_route`, `private_admission_routes`;
  - the request-kind gate.
- **Manifest:** the matching command entries in `plugins/connections-plugin/plugin.toml`
  (about 60% of the file).

Kiosk, the local-IPC hardening, and the hosting policy remain.

## Phases and lanes

Each phase lands as reviewed commits that keep `master` working. Lanes in the same row
can run in parallel; one writer owns each file, and the coordinator owns root manifests,
`Cargo.lock`, `plugins/connections-plugin/src/lib.rs` and cross-lane wiring.

The connections plugin is rewritten by one lane: its transport, trust store and pairing
share types too closely for two writers. Each lane deletes the legacy code in the files
it owns, so the deletion sweep is folded into the owning lanes.

| Phase | Lane | Owns | Delivers |
| --- | --- | --- | --- |
| 0 | T: guardrails | `packages/cli/tests/remote_access.rs` and `packages/cli/tests/remote_access/**` (new), the `[dev-dependencies]` table of `packages/cli/Cargo.toml`, `.github/workflows/**`, `flake.nix`/`flake.lock` (cargo-deny only), `scripts/smoke-remote-pairing.py` (deleted) | The acceptance tests below, `#[ignore]`d with a reason until the lane that makes each pass removes the ignore; a local relay fixture; Linux and macOS CI jobs; a `cargo-deny` that parses the current advisory database |
| 1 | B: core ingress + authorization | `packages/server/**`, `packages/plugin/src/stream_admission.rs`, `packages/plugin-sdk/**`, `packages/plugin-schema*/**`, `packages/ipc/**`, `packages/cli/src/runtime/bootstrap.rs`, `packages/cli/src/runtime/plugin_kernel.rs`, `packages/cli/src/runtime/server_commands.rs`, every `plugins/*-api/bpdl/*.bpdl` except connections and permissions, provenance call sites in `plugins/pane-runtime-plugin/src/handlers/**`, `plugins/clients-plugin/src/identity.rs`, `plugins/cluster-plugin/src/{lib,admission}.rs` | Contracts B1–B3 below; every interface annotated with an access class; request-kind gate, admitted-route lists, ticket preamble, serialized provider bridge and lease monitor deleted; refused gateways deleted |
| 1 | A: connections | `plugins/connections-plugin/**`, `plugins/connections-plugin-api/**`, `plugins/permissions-plugin/**`, `plugins/permissions-plugin-api/**`, `docs/remote-pairing.md` | Identity, trust store, known hosts, transport and listener on B1, pairing, roles, B2 authorizer in permissions, D1 connector for saved hosts, host commands; `hosted/`, SSH/TLS transports, `access`/`trust`, `iroh://`, invitation redeem, admission, and the paired-terminal adapter deleted |
| 1 | D: client integration | `packages/client/**`, `packages/cli/src/connection.rs`, `packages/cli/src/runtime/remote_cli.rs` (deleted), `packages/cli/src/runtime/attach/**`, `packages/cli/src/runtime/attach_selected.rs`, the attach display-capture code | Contract D1; attach runs over any connector-provided client; local-only assumptions removed; legacy CLI TLS/SSH/Iroh dialing deleted |
| 2 | Coordinator | root manifests, `Cargo.lock`, cross-lane wiring | Integration, ignores removed as tests pass, full validation |
| 3 | F: cluster and mobile | `plugins/cluster-plugin*/**`, mobile FFI docs and crates | Cluster on the connection manager, cross-host cluster attach test, mobile target model |
| 3 | G: operations | connections status and doctor commands, status item | Status, doctor, path indicator, host picker, reconnect overlay, audit log |
| 4 | Acceptance | — | Security review, fuzzing, performance budgets, two-network trial |

Phase 2 is where remote access becomes usable end to end; phases 3 and 4 finish it.

### Wave 0 contracts

These signatures are fixed before the phase 1 lanes start. A lane that needs one changed
asks the coordinator; it does not change another lane's side. Names are binding; bodies
and private helpers are the owning lane's choice.

**B1. Stream ingress** (`packages/plugin/src/stream_admission.rs`, owned by B). The
ticket, authority and provider-codec types are replaced by:

```rust
/// Immutable identity of one remote stream, created only by in-process host code.
pub struct StreamAdmissionBinding { /* subject_id, connection_id, revocation */ }
impl StreamAdmissionBinding {
    /// Returns the binding and the only handle that can revoke it.
    pub fn new(subject_id: Uuid, connection_id: Uuid) -> (Arc<Self>, StreamRevocation);
    pub fn subject_id(&self) -> Uuid;
    pub fn connection_id(&self) -> Uuid;
    pub fn is_revoked(&self) -> bool;
    /// Resolves once revoked (immediately if already revoked). Cancel-safe.
    pub fn revoked(&self) -> impl Future<Output = ()> + Send + '_;
}
/// Cloneable; revoking is idempotent and wakes every `revoked()` waiter.
#[derive(Clone)] pub struct StreamRevocation { /* … */ }
impl StreamRevocation { pub fn revoke(&self); }

/// Registered in `PluginStateRegistry` by the server before plugins activate.
#[derive(Clone)] pub struct StreamIngressHandle { /* … */ }
impl StreamIngressHandle {
    /// Serve `stream` as one connection whose provenance is `Admitted(binding)`.
    /// The server applies its normal connection limits; a full server fails
    /// closed. The connection ends when the stream ends or the binding is revoked.
    pub fn accept(&self, stream: bmux_ipc::transport::ErasedIpcStream,
                  binding: Arc<StreamAdmissionBinding>) -> Result<(), StreamIngressError>;
}
```

`StreamProvenance::{Local, Admitted(Arc<StreamAdmissionBinding>), Invalid}` and
`StreamProvenanceRegistry` keep their names and meaning; a revoked binding looks up as
`Invalid`. The `Hello` principal on an admitted stream must equal `subject_id`, as today.
`Request::AdmitStream` is deleted.

**B2. Route access classes** (`packages/plugin-sdk`, BPDL; owned by B).

```rust
pub enum RouteAccess { Observe, Input, Manage, OwnerLocal }
pub trait RemoteAccessAuthorizer: Send + Sync {
    /// Called for every request on an admitted stream; must not block.
    fn allows(&self, subject_id: Uuid, access: RouteAccess) -> bool;
}
/// Registered in `PluginStateRegistry` by the plugin that owns roles (permissions).
#[derive(Clone)] pub struct RemoteAccessAuthorizerHandle(Arc<dyn RemoteAccessAuthorizer>);
```

In BPDL, `@access(observe|input|manage|owner-local)` on an interface sets its class, and
the same annotation on an operation overrides it. The class travels in the generated
declared contract, so the server knows each route's class. A route with no class, or
with no authorizer registered, is denied on admitted streams. `OwnerLocal` is denied on
admitted streams whatever the authorizer says. Local streams are not checked.

**B3. Kernel requests on admitted streams.** All requests are accepted. Kernel-level
requests that are not service routes take fixed classes. `Hello`, `Ping`,
`SubscribeEvents`, `PollEvents` and `EnableEventPush` are `Observe`. `EmitOnPluginBus`
is `Input`. Anything that stops or reconfigures the server is `OwnerLocal`.

**D1. Transport connectors** (`packages/client`, owned by D).

```rust
pub trait TransportConnector: fmt::Debug + Send + Sync + 'static {
    /// Whether this connector owns `target` (for example a saved host alias).
    fn supports(&self, target: &str) -> bool;
    /// Open one fully handshaken client (Hello done) for `target`.
    fn connect<'a>(&'a self, target: &'a str, client_name: &'static str)
        -> Pin<Box<dyn Future<Output = Result<BmuxClient, ClientError>> + Send + 'a>>;
}
```

The CLI consults registered connectors before its local-socket default.
`AttachEndpointConnector` is implemented on the same registry. Connector registration
happens where attach providers are registered, and core never parses a connector's
targets. A connections-plugin frontend registers the saved-host connector.

**A-internal.** The trust store (device key → principal, state, revision) publishes
changes that the listener's live-connection registry turns into `StreamRevocation::revoke`
calls and QUIC connection closes. Roles live in the permissions plugin, which implements
B2 from an in-memory map persisted through its storage. Connections sets roles through a
typed permissions BPDL command. `full` allows `Observe`, `Input` and `Manage`;
`view-only` allows `Observe`.

**Hosting scope (v1).** One host key per machine. Only one runtime hosts at a time; a
host-key lock refuses a second listener. Reaching other runtimes on the same machine
through the hosting one is later work, and it never passes a local socket's authority to
a remote stream.

## Tests and acceptance gates

Written in phase 0. They fail on today's code and define done:

1. **Idle survival.** Attach, idle 5 minutes, type; the output arrives.
2. **Parity.** Over a paired loopback connection: list sessions, attach to an existing
   session, switch tabs, split, use mouse selection, scroll back, view an image; screens
   match a local attach of the same session.
3. **Revocation while attached.** The viewer is closed within 1 s; a reconnect is refused.
   Role change to `view-only` while attached: input is refused and output continues.
4. **Reconnect.** Drop the path (a UDP proxy that blackholes traffic); the client resumes
   the same session after the path returns, without replaying input.
5. **Relay only.** Direct addresses disabled, local relay server; pairing, attach and
   input all work.
6. **Pre-auth surface.** Unknown device key on `bmux/session/1`: closed during the
   handshake. `bmux/pair/1` with no open pairing session: refused. Malformed pairing
   frames: bounded and refused (also fuzzed).
7. **Daemonless client.** No socket, server or runtime directory is created on the
   connecting side.
8. **Owner-local routes.** Every `owner-local` route is refused from a `full` remote
   principal.
9. **Cluster.** Two runtimes as cluster nodes over paired transport; cross-host attach
   shows the worker's pane.

Budgets, measured with the existing perf tooling:

- Zero bytes on the wire while the screen is idle.
- Keystroke to echo within 1 RTT plus server processing.
- Output throughput (for example `cat` of a large file) costs at most screen size × frame
  rate on the wire, not output volume.
- One Iroh endpoint per process, regardless of the number of clients.

Release acceptance: desktop (Linux) and Mac on two different networks, covering direct,
relay-only, a Wi-Fi switch, a host restart and revocation; native macOS CI green; the
required checks from `AGENTS.md` all pass.

## Open items

- Whether to show the reconnecting overlay through the existing presentation layer or a
  dedicated status surface. Decided in lane G: a one-line notice on the restored
  terminal for now (see Operations).
- Exact BPDL syntax for access classes (decided in lane B against `docs/bpdl-spec.md`).
- Default pairing link lifetime (10 minutes proposed).
- Usage policy of the public Iroh relays for this traffic, and a documented self-hosted
  relay recipe.
