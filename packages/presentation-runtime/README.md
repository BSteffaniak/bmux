# bmux_presentation_runtime

Application-owned authoritative presentation engine, shared by standalone and
BMUX-managed deployments. This crate adds no presentation domain behavior to core.
Plugin implementations call `Host` and expose generated BPDL operations; the
`protocol` messages here are exclusively the standalone TLS transport contract.

## Stable integration API

- `Host::new(Deck, Vec<Asset>, HostConfig)` validates a bundle and starts a new run.
- `Credential::generate(Scopes, Duration)`, `Host::add_credential`, and
  `Host::revoke_credential` establish private run-local principals. Store secrets
  privately. Reconnect always authenticates again; a participant ID is not a secret.
- `Host::authenticate(version, secret, mode)` creates a participant;
  `disconnect` releases connection resources. Independent views retain state and
  committed surfaces per private credential across reconnect, with one connected
  independent view per credential. Issue separate credentials for distinct views.
  `next_action_id` returns the next
  credential sequence number. Multiple devices sharing a credential share ordering.
- `snapshot` explicitly bootstraps/resynchronizes any authorized participant,
  including submit-only. `poll` returns unchanged cursors or complete bounded
  snapshots; submit-only has no ongoing polling subscription. No streaming API is
  assumed. Public read-only audience access requires `public_audience: true`.
- `submit(Submission { action_id, expected, action })` enforces mode, scope,
  run/deck/state cursor, rate, payload bounds, and committed surface geometry.
  Private navigation/demo/effect scopes are independent of speaker-note access.
- `asset` distributes only audience-referenced opaque resources in bounded chunks.
- `reload` validates/prepares a replacement before atomically changing deck/assets,
  reconciling slide identities/reveal steps, and invalidating mirror geometry.
  Use `reload_with_owner(deck, assets, Box<dyn Owner>)` when the owner captures
  the deck/assets (for example `DemoOwner`). It prepares every shared, connected
  independent and retained independent scene against the replacement owner before
  swapping anything; any failure preserves the prior owner and complete host state.
- `Host::set_owner(Box<dyn Owner>) -> Result<(), HostError>` prepares and validates
  initial shared and connected independent scenes before installing the owner.
  `Owner::scene(&mut self, OwnerContext) -> Result<Vec<MirrorFrame>, String>` defaults
  to empty and is projection-only: it must never invoke effects. The host calls it
  on independent join, navigation and reload, not on polling or viewer resize.
  A reload owner must already understand the replacement deck identities; otherwise
  projection fails and the host retains the old bundle. Callback diagnostics are
  not exported. `MirrorComponent::new` validates a borrowed scene and implements
  the canonical layout/paint/event/revision lifecycle; map input using its exact
  successfully committed `LayoutNode` with `input_point`.
- `Owner` is trusted linked Rust. `OwnerContext.instance` is `shared` for shared
  ordering and credential-local for independent instances across reconnect. `OwnerUpdate` returns
  a bounded outcome and complete committed public surface batch. Never render
  private notes into audience surfaces. Generic viewers never activate Rust code.
- `commit_surfaces` publishes owner-committed structured BMUX scene captures.
  `MirrorFrame::capture` accepts a BMUX `Frame` plus reconciled `ImageScene`;
  `MirrorFrame::paint` reconstructs through `PaintCx`, not ANSI. `placement` and
  `CanvasPlacement::input_point` clip/letterbox the fixed owner canvas. Publish
  input geometry only after successful viewer presentation. Input includes the
  run/deck/state cursor and committed surface revision.
- `SourceLoader::{load_markdown, prepare}` accepts bounded local Markdown or
  linked decks, reads images beneath an explicit canonical root, normalizes image
  payloads, and invokes `IsolatedMermaidRenderer` (never the synchronous backend).
  Select a pinned/packaged worker and call source preparation outside the live
  host lock. The renderer owns worker timeout/memory/concurrency limits.
- `transport::{serve_tls, serve_local}` accept owned Tokio listeners, shared host,
  bounded network settings, and watch-channel shutdown. TLS config helpers parse
  bounded explicit PEM chains/keys/roots. Local plaintext requires loopback on
  both sides; remote TLS verifies certificate roots and server name.
- `transport::Client::{connect_tls, connect_local, authenticate, request}` provides
  one bounded in-flight exchange, no automatic replay, and poisons interrupted
  connections. Snapshot decode validates mirror scenes before exposing them.

## Delivery and privacy semantics

Action IDs start at 1 and increase per credential, surviving reconnect within a
live run. Retained identical submissions return their original success/error;
changed retained payloads return `ActionConflict`. IDs below the high-water mark
that have been evicted return `OutcomeEvicted`, never execute again. After eviction
clients must fetch authoritative state and explicitly reconcile external outcomes;
this is not durable exactly-once execution. A new host run has fresh identity and
requires explicit new actions. Never replay uncertain effects into a new run.
Owner calls consume the state revision before execution, even when their outcome
is uncertain. Polling/snapshots never invoke effects. The owner must return only
public, bounded outcomes; private external diagnostics should stay owner-local.

The audience projection strips notes, source locations, custom configuration,
theme/extra metadata and Markdown link/image destinations/raw HTML. Markdown text
is deliberately projected to plain portable text; links are not made clickable.
Image destinations become random resource IDs; unreferenced assets are not served.
Visible authored labels, code, Mermaid content, titles, authors, IDs, and custom
names are public content: authors must not put secrets there. Custom configuration
is never activated from an audience deck. Notes are a separately scoped field.

Limits cover messages, decks/notes, assets/cache bytes, frame/canvas/image decoded
sizes, surfaces, principals, participants, action retention, action size, request
rates, connections, handshake/read/write deadlines. Networking retains one request
per connection and uses bounded admission rather than unbounded queues. Owner
callbacks are synchronous trusted application code and must not block or spawn
unbounded work; asynchronous demo lifecycle management belongs to the application.

## Validation/handoff

Behavior tests cover auth/modes, private wire projection, shared/independent state,
reconnect action dedup and uncertainty, eviction, atomic reload, malformed mirrors,
all style bits, clipped placement, real loopback TCP and TLS certificate validation.
No cargo/build/fmt/test commands were run in the initial lane: the coordinator owns
workspace manifests, lockfile, target serialization, formatting and validation.
The `bmux present` command/plugin adapters belong to their assigned integration lane.
