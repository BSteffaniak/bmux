# Live components and retained embedding

A live component embeds output from an independently owned producer inside an
ordinary BMUX component tree. The producer owns execution and its retained state;
the embedding owns placement and presentation. Hiding a view is not an implicit
execution-lifetime operation.

## Ownership and composition

- Applications and plugins own start, stop, authorization, file access, input
  delivery, and reload policy. Plugins use declared typed service contracts, not
  another plugin's private runtime implementation.
- `bmux_tui::embedded_surface::RetainedSurface` retains an immutable origin-zero
  `Arc<Buffer>` and a complete reconciled `Arc<ImageScene>`, tagged with
  `SurfaceIdentity { owner, generation, revision }`. `SurfaceLimits` bounds
  ingestion. It is a derived raster projection, not execution state.
- `EmbeddedSurface` participates in the normal `Component` layout/paint lifecycle.
  A smaller viewport center-crops the fixed raster; a larger one centers it.
  Cells and images pass through the same `PaintCx` transforms and clipping as
  other components. This is not reflow of the producer's terminal.
- The retained compositor remains the sole authority for geometry, damage,
  occlusion, hits, focus, and routing. An embedding does not create a competing
  browser scene or execute producer callbacks while painting.

The host retains the layout actually displayed. `EmbeddedSurface::input_point`
accepts that committed layout and returns source coordinates tagged with the
surface identity; a different snapshot's layout is rejected. The host still
chooses the intended input target and validates that identity at delivery.
`EmbeddedSurface::event` itself is inert. Raster embedding does not reconstruct
logical selection, semantics, or application controls from cell glyphs.

Overlay placement and modal interaction also belong to the shared composition
path. `composition::Overlay::new(background, foreground).id(...).modal(scope)`
centers an opaque foreground and excludes background input when modal. Stage the
same outer-to-inner scope chain with
`PreparedPresentation::set_modal_scopes`; interaction authority changes only when
output commits. Background input must not be broadcast around an active modal,
and closing it restores only an eligible committed focus target. Overlay hiding
and producer stopping are separate application decisions.

## Embedding a retained producer

The minimal integration sequence is:

1. Acquire a complete retained raster and reconciled image scene outside painting.
2. Construct `RetainedSurface::new(identity, cells, images, limits)`, handling
   malformed or over-budget output explicitly.
3. Build `EmbeddedSurface::new(&surface)` into the ordinary component tree.
4. Prepare and deliver output. Only on success retain the corresponding layout
   and interaction state as committed.
5. Map pointer coordinates through that committed layout, then route one event
   to the identity-checked owner. Keyboard/paste routing remains host policy.
6. On replacement, reject stale generation/revision input. On removal, release
   the view's retained references; apply execution stop policy separately.

Presenter extensions can use `CustomComponentFactory` or an explicitly supplied
`LinkedRegistry`. Building a component is deterministic/read-only; it is not a
place to launch a process. Linked factories have explicit state versions and
compatibility keys. Reload candidates use detached state, not the live mutex.

## The same output in terminal and browser

Browser support is a directional projection of prepared presentation, not a
second live-component implementation. With the web host's `presentation` feature,
`BrowserPublisher::publish_prepared(generation, source_revision, &prepared)`
produces full bytes, an optional exact-predecessor delta, and the complete resource
manifest. Caller-owned source identity is distinct from the host's monotonically
increasing transport revision.

Publication commits representations/resources and view generation together.
Rejection keeps the previous complete publication and reports bounded status.
Within one generation source revisions must advance. Optional delta or diagnostic
JSON failure cannot veto an otherwise admissible full publication; unavailable
JSON remains explicit rather than pretending an empty document is complete.

The feed is latest visual state, not an execution-output journal. Slow observers
may miss intermediate visuals. A delta is used only for its exact predecessor;
a missing or mismatched base receives full repair. This does **not** imply that
execution logs, stream watermarks, or logical source text can be dropped or
reconstructed from the final raster. Browser metadata must explicitly report
unavailable producer capabilities.

Opaque image IDs are SHA-256 content addresses. Admission checks new IDs and
requires exact MIME/content equality for retained reuse. Shared immutable byte
allocations can reuse the original admission proof without rehashing. Evicted IDs
are checked anew. Size/count limits remain in force, including conservative
accounting across the latest and previous manifests. Browser decode/cache limits
are separate; a host-admissible publication is not a promise that every browser
backend can accept it. Rejections must remain visible and preserve committed output.

See [web host](../packages/web-host/README.md) for hosting/assets/security and
[web renderer](../packages/web-renderer/README.md) for wire/fidelity contracts.

## Playbook demo authoring and execution

A playbook demonstration is one application of the general embedding mechanism,
not a browser-specific schema. The implemented presenter registration is
`playbook-demo`:

```markdown
<!-- bmux:block-id live-demo -->
<!-- bmux:component playbook-demo {"playbook":"live-demo.dsl","title":"Actual BMUX session","on_leave":"stop"} -->
```

Parsing remains inert. It neither reads the playbook nor starts an execution.
The presenter explicitly enables executable-input approval with
`--allow-playbooks`. Use only trusted playbooks: isolated BMUX paths are not a
security sandbox for arbitrary shell actions. The source path must be relative,
without traversal, beneath the canonical deck directory. The only accepted
`on_leave` value is `stop`; omission uses the same stop-on-leave policy.

The supplied example can be opened from the repository root with a bundled native
BMUX executable that supports both managed worker and retained observer protocols:

```sh
bmux present play plugins/presenter-plugin/examples/live-demo.md --allow-playbooks --presenter --no-reload
```

Tab focuses the demo; `o` opens its overlay without starting work. Controls are:

| Key | Action |
| --- | --- |
| `r` | Prepare/start the real execution |
| `p` / `n` / `c` | Pause / execute one action / resume at script boundaries |
| `s` | Request stop |
| `R` | Restart with a new generation after cleanup acknowledgment |
| Escape | Hide the overlay without stopping execution |
| `q` / Ctrl-C | Exit the local presenter and request owned cleanup |

A step includes the entire action, including its waits; it is not arbitrary
seek/replay. Leaving the slide requests stop. Retained final output remains
viewable after execution ends. Execution admission is not completion or cleanup
acknowledgment. An unresolved cleanup diagnostic must not be interpreted as a
successful stop.

Add `--web` to mirror a read-only browser audience. The browser cannot start,
control, or navigate a run. `--web-only` is also read-only; it supplies no local
keyboard route to start an initially idle demo. The retained execution observer
uses a fixed 80×24 viewport. Resizing the embedding clips/centers that raster;
it does not implicitly resize the independently owned execution.

Managed hosting has explicit approval and presenter credentials:

```sh
bmux present host plugins/presenter-plugin/examples/live-demo.md --managed --allow-playbooks --presenter-credential PRIVATE_FILE --public-read --no-reload
bmux present join bmux:HOST_ID --credential PRIVATE_FILE --presenter --mode shared
```

The credential file is private; `--public-read` exposes audience output, not effects
authority. Managed execution controls require presenter effects authority.
`present check`, audience authentication, and merely opening an overlay do not
authorize execution. See the [presenter plugin](../plugins/presenter-plugin/README.md)
for transport and credential options.

### Reusable executor

`bmux_playbook` exposes the actual action dispatcher and sandbox/recording
lifecycle behind its default-off `runtime` feature. CLI and managed external
workers call that same executor. Non-CLI callers supply a required `HostAdapter`
for pane transport, keyboard processing, attach/simulation primitives, terminal
acquisition and executable selection; adapters do not receive whole playbooks or
dispatch actions. Generic CLI input and pane/scrollback implementations remain
CLI-owned. Parser/source consumers do not enable native execution dependencies.
See [playbook API](../packages/playbook/README.md).

### Execution and display boundaries

`bmux.playbook` exposes generated typed lifecycle contracts. The implementation
owns the selected native worker process, invokes the existing playbook action
executor, and negotiates `bmux-playbook-worker-v1` before launching the deck.
Its private retained control/status files carry generation-fenced lifecycle state;
stdout and bounded stderr diagnostics are not terminal output feeds.

Each attempt has private paths and a short private runtime directory for Unix
sockets. The worker environment is explicitly selected; custom sandbox config and
`BMUX_`/`XDG_` runtime-selector overrides in playbook configuration are rejected.
Source/include closure is frozen per attempt with no-follow directory-relative
opens: at most 128 files, 1 MiB per file, 4 MiB total, and bounded include depth.
Restart uses the frozen closure. The protocol probe checks compatibility, not
executable content identity; the operator must select an immutable native artifact.

The presenter owns a bounded asynchronous controller, shared by local and managed
owners: eight queued controls, latest-value lifecycle/capture channels, and at
most sixteen demo entries per prepared deck. Synchronous owner update turns consume
ready snapshots; paint and snapshot/poll do not execute runner IO. The browser
publication loop also observes live updates independently of resize/reload.

Display uses a separately owned output-only observer of the attempt's normal
retained attachment. Its private `BMAO` stream carries full self-contained browser
wire frames, bounded to 16 MiB, with session/epoch/revision checks. The presenter
projects the accepted raster through `RetainedSurface` and the ordinary overlay;
it does not decode worker stdout, run a second terminal emulator, or introduce a
playbook-specific browser schema. `MirrorFrame` remains an existing wire adapter
over the same neutral embedding primitive.

Cleanup acknowledgment requires sandbox child reaping, worker exit, and owned
runtime cleanup. Emergency termination is explicitly not acknowledgment and blocks
restart. Local terminal restoration happens before the bounded cleanup wait;
remaining uncertainty is reported rather than silently treated as completion.

### Current limitations

- Approval binds the bounded include closure by canonical relative names and bytes.
  Preparation verifies and freezes the same bytes before admitting execution;
  changed executable input requires explicit approval again.
- Reload preparation is effect-free and detached. At commit, stable block IDs with
  the same playbook path and unchanged approved source closure retain live
  controllers, generation, overlay state, and retained output. Titles and slide
  placement may change. New or changed executable bytes require fresh explicit
  approval; a failed candidate leaves the old deck authoritative. Removed or
  reconfigured active controllers request cancellation only at precommit and defer
  replacement until settlement; retry reload after settlement.
- The embedded raster does not export the producer's logical selection/semantics
  into the outer presenter scene. Do not infer source text from cells or claim
  complete logical metadata for the embedded region.
- Controls operate the script lifecycle, not arbitrary terminal keyboard/paste
  input. The retained observer is output-only and Unix-specific.
- The execution service retains at most sixteen attempts, including previous
  generations; it does not reclaim capacity or persist active ownership across
  host crashes. Generations are not principal authentication. Multi-tenant remote
  execution needs additional per-owner authorization policy.
- Private observer paths still depend on the selected temporary directory fitting
  Unix socket limits. This document does not promise arbitrary-depth temporary
  path support.

These are implementation boundaries, not benchmark or runtime-acceptance claims.

## Measuring publication work

The finite `live-publication-performance` example measures real prepared-scene
publication and full/delta encoding separately, with image/no-image,
steady/changing, first-admission/reuse, and fast/slow-consumer cases:

```sh
bash scripts/dev.sh cargo run -p bmux_web_host --release --features presentation --example live-publication-performance -- /absolute/target/web-assets 128
python3 scripts/live-components-performance.py --binary /absolute/live-publication-performance --assets /absolute/target/web-assets --browser /absolute/chromium --samples 32
```

The native harness counts successful allocation/reallocation calls, requested
allocation bytes, outstanding requested heap and fixture-drop release. These are
not RSS or publisher-exclusive memory. Preparation, HTTP and decoding are outside
the synchronous measurement scope. Exact predecessor delta reconstruction,
missed-base full repair, immutable-source retention and generation reset are
asserted without timing thresholds. The browser driver correlates actual source
revisions and Linux monotonic timestamps with Canvas API commit observations;
clock-mapping uncertainty is reported. This is not physical paint latency.

A coordinator debug-profile observation (80×24, 32 native samples, 8 browser
samples per case, Linux Chromium 149) completed all cases and repair assertions.
Browser source-to-Canvas commit p50/p95 was 21.6/22.4 ms without images and no
content change, 22.2/32.4 ms without images with changes, 72.7/75.8 ms with images
and no content change, and 79.2/82.1 ms with image changes. Clock calibration
half-RTT uncertainty was 0.11–0.16 ms. These small debug samples are evidence,
not release baselines, performance guarantees, or evidence of negligible cost.
Native publication still allocates and encodes; immutable resource admission
reuse does not make whole-publication work allocation-free. Re-run the commands
above in the intended optimized environment before making comparative claims.

An ignored source test measures resource admission only, comparing an unseen
1 MiB payload, retained shared bytes, and retained equal bytes in a distinct
allocation. It does not measure browser rendering, encoding, network delivery,
or end-to-end latency. Run in the pinned development environment:

```sh
bash scripts/dev.sh cargo test -p bmux_web_host --release resource_admission_measurement -- --ignored --nocapture
```

Focused source regressions cover unchanged reuse, changed MIME/content rejection,
eviction/revalidation, unchanged retention budgets, and latest-full repair after
a publication burst:

```sh
bash scripts/dev.sh cargo test -p bmux_web_host --features presentation retained_
```

No performance result is implied by the existence of this harness.
