# Legacy Attach Presentation Inventory

This inventory records the production ownership that the plugin-owned presentation migration must remove or deliberately preserve. It is intentionally a migration map, not a new architecture contract.

## Configuration and projection

| Concern | Production owners | Migration obligation |
| --- | --- | --- |
| `StatusBarConfig` | Removed from `packages/config`; `[status_bar]` is rejected with a diagnostic pointing to the tab-strip/sidebar plugin settings. The compatibility CLI status module and simulation tab fixtures are deleted. | Tab-strip/sidebar settings are plugin-owned. Local notifications remain attach-client state until their generic presentation companion is complete; they no longer preserve status-row geometry or legacy config. |
| `StatusPosition` | Removed from `packages/config`; legacy `appearance.status_position` is rejected during file loading with a diagnostic pointing to `plugins.settings.\"bmux.tab_strip\".placement`. | Generic plugin layout is authoritative for top/bottom placement; no attach runtime/state, simulation, playbook, layer, or damage path reserves a status row. |
| `AttachTab` and tab projection | `packages/cli/src/status.rs`; constructed by attach runtime from windows snapshots or raw contexts; mirrored by attach state/simulation | Move projection/rendering into the tab-strip plugin and leave only baseline terminal attach behavior in core. |
| Windows state consumption | `packages/cli/src/runtime/attach/runtime.rs`, state, simulation, bootstrap, and playbook support import `bmux_windows_plugin_api`; runtime decodes `bmux.windows/windows-list` and projects it into `AttachTab` | Presentation consumption must move to the tab-strip companion using generated windows contracts. CLI bootstrap/runtime must stop interpreting window state for presentation. Other domain-owned plugin/mobile consumers are not part of this removal. |

The generic four-edge viewport migration already removed production
`status_top_inset` and `status_bottom_inset` fields. Those names now occur only
in architecture guardrails. Current authoritative geometry uses neutral
`top_inset`, `right_inset`, `bottom_inset`, and `left_inset` fields through the
pane-runtime attach contracts and implementation.

## Interaction and overlays

| Concern | Production owners | Migration obligation |
| --- | --- | --- |
| Hover | `AttachViewState.hovered_tab_context_id`, attach mouse routing/simulation, and `packages/cli/src/status.rs` hover styling | Move semantic hover state and repaint publication into the tab-strip companion. |
| Drag/reorder | `AttachMouseTabDrag`, runtime pointer handling, status hitboxes, simulation, and playbook coverage | Use generic committed-region input and pointer capture; invoke generated windows reorder commands. |
| Rename | Attach prompt state/runtime actions, tab edit projection in `status.rs`, simulation, and playbooks | Move workflow ownership to the tab-strip plugin while retaining the existing prompt behavior or an equivalent plugin-owned editor. |
| Tab menu | `AttachTabMenu`, `AttachTabMenuAction`, retained menu surface construction, runtime/state/simulation, and playbooks | Move menu model/actions/placement to the tab-strip plugin and target its resolved surface allocation. |
| Click/hit testing | `AttachStatusTabHitbox` emitted by `status.rs`, then interpreted by attach runtime | Replace hardcoded status-row hitboxes with committed `PluginSurfaceRegion` routing. |

## Damage and rendering

- `packages/cli/src/status.rs` builds the complete status line, tab spans,
  overflow selection, and `AttachStatusTabHitbox` values.
- `packages/cli/src/runtime/attach/runtime.rs` builds
  `retained_status_surface`, checks `FrameDamage::status_damaged`, composes the
  status surface into retained frame planning, emits status trace events, and
  handles retained tab-menu surfaces.
- `packages/attach_pipeline` still contains generic/legacy status damage inputs
  used by the current attach renderer. These must disappear only after the
  presentation plugins own rendering and semantic input end to end.
- Attach state/simulation and playbook engine mirror legacy behavior and must be
  migrated with the production runtime rather than left as a second model.

## Status module ownership inventory

The legacy status line combines independently owned facts. Migration must not
turn those facts into generic core status state:

| Module | Current projection source | Canonical owner and migration boundary |
| --- | --- | --- |
| Interaction mode | Attach input processor and `AttachViewState.active_mode_id/label`; overlay, scroll, zoom, and prompt state select derived labels | Attach client owns local input/overlay mode. A presentation companion may receive a neutral local view fact; no server plugin should become authoritative for terminal input mode. |
| Role | `AttachOpenInfo.can_write` and retarget/follow attach results | Permissions/attach authorization remains authoritative. Presentation consumes the resulting typed authorization fact; it must not decide access. Missing permissions retains permissive baseline behavior. |
| Follow state | Typed `bmux.clients` events plus local followed-client selection | `bmux.clients` owns follower/leader relationships. Presentation consumes its generated contract rather than duplicating follow state in core or a presentation plugin. |
| Hints | Prompt/help/scroll state, transient message state, floating-pane summary, and configured keymap bindings | The workflow owner produces semantic/local hint text. Presentation only places/styles it. Pane/floating facts must come through the foundational typed contract when extracted. |
| Transient messages | `AttachViewState.transient_status` with attach-local TTL, written by command, error, clipboard, follow, and workflow handlers | The attach workflow that performs an action owns its ephemeral result. Keep a neutral client-local notification stream/slot; do not make tab/sidebar or core server state authoritative. |
| Session label/count | Cached typed sessions catalog | `bmux.sessions` is authoritative; consume generated session state/services. |
| Context label | Cached typed contexts catalog and context/session bindings | `bmux.contexts` is authoritative; consume generated context state/services. |
| Tab position/count | Derived from the ordered tab projection and active item | The tab-strip plugin derives this from authoritative `bmux.windows` ordered state. It is tab-strip presentation, not a generic status fact. |

Current support remains intentional during migration: mode, role, follow, hints,
transient messages, optional session/context labels, and tab position/count all
have existing configuration or runtime behavior. Final placement is split by
ownership: tab position/count stays with the tab-strip plugin; window/pane live
facts go through Phase 10's typed domain producers; local mode/hints/messages
belong in an attach-client presentation companion; sessions, contexts, clients,
and permissions are consumed through their generated foundational contracts.
No module may retain a hidden status-row geometry path after extraction.


- Status can be top, bottom, or disabled.
- Tabs support configurable scope/order, templates, index visibility, Unicode
  width limits, active/inactive/hover styling, narrow-width overflow, and active
  item visibility.
- Pointer workflows include click switching, hover, drag reorder, rename, and a
  tab action menu.
- Remaining status modules include interaction mode, role, follow state, hints,
  transient messages, session/context labels, and tab position/count. Their
  canonical owners and final presentation placement are resolved separately in
  Phase 8.
- Resize, reconnect, and retarget currently rebuild geometry and presentation;
  migration must preserve those lifecycle results through retained owner
  snapshots and generic layout.

## Attach geometry path

The authoritative geometry path is:

1. Attach startup reads `terminal.geometry()`, resolves all retained plugin
   layout requests plus the temporary legacy status reservation through
   `resolved_attach_viewport_insets`, and sends one four-edge
   `attach_set_viewport_with_insets` command before initial hydration.
2. Pane-runtime stores the neutral four-edge viewport and derives the scene root
   and PTY dimensions from the remaining content rectangle.
3. Snapshot hydration returns the pane scene/content rectangles to the client.
4. Normal frame planning resolves the same plugin layout snapshot, lowers
   matching surface revisions into `RetainedSurface`, composes pane and plugin
   surfaces in `RetainedCompositor`, and derives precise damage/repaint output.
5. The terminal renderer lowers only the repaint plan and applies capability
   fallbacks before writing and flushing one frame.

All geometry-changing lifecycle paths use that same resolver and neutral inset
command:

- terminal resize calls `update_attach_viewport_with_geometry` before redraw;
- layout-registry revision wakes recompute and publish viewport geometry before
  marking layout dirty;
- context retarget sends geometry and resolved insets atomically in
  `retarget_attach_context_with_insets` before hydration;
- session retarget, follow-target changes, reconnect/startup, profile changes,
  and explicit refresh call `update_attach_viewport` before hydration or the
  next frame;
- retained owner state survives compositor replacement and is hydrated from the
  current matching layout/surface revisions.

Attach providers converge on the streaming attach runtime before this path; no
second backend-specific presentation geometry implementation exists. Legacy
status top/bottom reservation remains an additive input to the resolver until
Phase 8 removes it, rather than a competing pane-runtime geometry model.

## Performance baseline evidence

The repository exposes the required frame metrics through
`AttachFrameRenderStats`, `attach_frame_trace_payload`, performance telemetry,
playbook render summaries, and the manifest-driven perf runner. Canonical fields
include frame bytes, damage rectangles/cells, full-surface/full-frame fallbacks,
frame render and terminal write latency, retained scene counters, and wake/update
counts.

A repaired local production `attach-tab-switch` run on 2026-08-21 (aarch64
macOS, debug `bmux`, five measured iterations after one warmup, four windows and
four warm switches) recorded command p99 4.705 ms, production pipeline p99
3.879 ms, and retarget p99 0.888 ms, all below the manifest's 8 ms normal SLO.
The ephemeral artifact is `/tmp/bmux-attach-tab-switch-baseline.json`.

This navigation baseline is complemented by a repeatable release-mode status
projection fixture (20,000 iterations, 240 columns): one window 3,781 ns,
64 windows 27,444 ns, identical idle projection 27,409 ns, hover 27,506 ns,
reorder 27,890 ns, and rename 26,513 ns average.

`render_assert_single_line_output.dsl` provides reproducible frame/output
samples at 80x24: startup emitted one full frame, 22 rows, 1,716 cells, and
3,941 bytes with no full-surface fallback; the subsequent shell update emitted
one 99-cell damage rectangle, three rows/two segments, 300 cells, and 330 bytes
with no full-frame/full-surface fallback. `render_assert_status_only.dsl`
confirms an unchanged status action emits zero frames, cells, damage, or bytes.
Frame render latency, terminal-write latency, and wake/update frequency are
recorded by `attach_frame_trace_payload`/`attach.window` using the same runtime
path; projection and production navigation timings above provide the current
presentation-update CPU and command baselines.

## Accepted baseline budgets

Until final product comparison deliberately revises them, completion claims use
these local Phase 0 budgets:

- production tab navigation command and retarget p99: **8 ms** each (the existing
  manifest SLO; measured 4.705 ms and 0.888 ms respectively);
- presentation projection CPU: no more than **35 µs average** for 64 visible
  items or hover/reorder/rename projection (measured worst 27.890 µs, leaving
  roughly 25% local variance headroom);
- retained reconciliation CPU: no more than **70 µs average** for full
  200-surface replacement and **35 µs** for a 40-item group update (measured
  53.091 µs and 26.165 µs);
- generic pointer routing: no more than **1 µs average** over 200 regions
  (measured 571 ns);
- equivalent incremental shell output: no more than the baseline **330 bytes**,
  **one 99-cell damage rectangle**, or an undocumented full-frame/full-surface
  fallback; idle/no-op presentation work must emit **zero terminal bytes**.

Final p95 frame/render latency and terminal-write latency are compared from the
canonical attach telemetry on the same host/profile; the migration may not
claim success from projection microbenchmarks alone.


Current CLI unit and playbook suites already cover tab templating, styling,
overflow, hitbox bounds, click, hover, drag, rename, menu behavior, and resize.
These tests are the compatibility baseline to migrate; plugin-specific tests and
the final combination matrix must supplement rather than silently delete them.
