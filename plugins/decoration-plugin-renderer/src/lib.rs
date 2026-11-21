//! Client-side render extension for the bmux decoration plugin.
//!
//! The decoration plugin runs in the bmux server process. Every time
//! its internal state changes it publishes a
//! [`bmux_scene_protocol::scene_protocol::DecorationScene`] on the
//! typed plugin event bus; the server relays the event to streaming
//! clients as a `ServerEvent::PluginBusEvent` over the attach IPC
//! stream.
//!
//! This crate consumes those relayed scenes on the client side:
//!
//! 1. [`renderer_for_bus`] creates an
//!    [`bmux_plugin::AttachRenderExtension`] subscribed to the retained
//!    `bmux.scene/scene-protocol` state on the attach runtime's own
//!    presentation bus (there is no process-wide bus).
//! 2. The retained scene state seeds the extension's cache immediately, and
//!    every subsequent scene replacement is drained by
//!    [`AttachRenderExtension::refresh_state`] before the next frame renders
//!    (revision-guarded so stale wire events can't downgrade).
//! 3. Each surface's layers are published as retained scenes, with a revision
//!    hashed from the scene's inputs, for the attach renderer to diff and emit.
//!
//! The CLI's streaming loop is responsible for decoding the IPC
//! `PluginBusEvent` payloads and re-emitting them onto the local
//! event bus; this crate subscribes locally and has no direct IPC
//! awareness.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bmux_plugin::{
    AttachInputEndpoint, AttachInputHook, AttachInputHookFilter, AttachRenderExtension,
    AttachVisualAdapterRequest, AttachVisualFrameView, AttachVisualProjectionResult,
    AttachVisualProjectionUpdate, BorderGlyphs as RenderBorderGlyphs, ExtensionRect, RenderCell,
    RenderColor, RenderExtensionContext, RenderExtensionLayer, RenderLayerItem, RenderLayerScene,
    RenderNamedColor, RenderOp, RenderSceneItem, RenderSceneItemKey, RenderStyle,
    TerminalRenderCapabilities, render_single_display_cell_char, render_text_width_u16,
};
use bmux_scene_protocol::glyphs::border_glyphs_corners_or_custom;
use bmux_scene_protocol::scene_protocol::{
    BorderGlyphs as SceneBorderGlyphs, Cell as SceneCell, Color as SceneColor, DecorationScene,
    GradientAxis, NamedColor, PaintCommand, Rect as SceneRect, STATE_KIND as SCENE_STATE_KIND,
    Style as SceneStyle, SurfaceDecoration, VisualAdapterRequest,
};
use bmux_scene_protocol_render::capabilities::{SceneRenderCapabilities, capability_query_matches};
use bmux_scene_protocol_render::paint::interpolate_style;
use uuid::Uuid;

mod raster_border;

const VISUAL_REQUEST_BUDGET: Duration = Duration::from_millis(4);
const SLOW_VISUAL_PROJECTION: Duration = Duration::from_millis(2);
const VISUAL_STATS_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Shared cache of the decoration plugin's latest scene. Stored
/// under `Arc<Mutex<_>>` so the render extension can refresh/read/write
/// without unwrapping poisoned locks at every call site.
#[derive(Default)]
struct DecorationRendererCache {
    revision: u64,
    surfaces: BTreeMap<Uuid, SurfaceDecoration>,
    /// Each surface's layer revisions ([`surface_layer_revision`], before-content then
    /// after-content), hashed once per scene rather than at every frame's revision query.
    layer_revisions: BTreeMap<Uuid, [u64; 2]>,
    scene_rx: Option<tokio::sync::watch::Receiver<Arc<DecorationScene>>>,
    visual_last_at: BTreeMap<String, Instant>,
    visual_last_revision: BTreeMap<String, u64>,
    visual_last_payload_hash: BTreeMap<String, u64>,
    visual_adapter_cache: BTreeMap<String, Box<dyn std::any::Any + Send>>,
    visual_stats: BTreeMap<String, VisualProjectionStats>,
    /// The attach runtime's visual adapters this renderer may project with.
    visual_adapters: Arc<bmux_plugin::VisualAdapterRegistry>,
}

#[derive(Clone, Debug, Default)]
struct VisualProjectionStats {
    projections: u64,
    unchanged: u64,
    updated: u64,
    sent_updates: u64,
    duplicate_suppressed: u64,
    oversized: u64,
    errors: u64,
    budget_skips: u64,
    payload_bytes: u64,
    slow_projections: u64,
    total_projection: Duration,
    max_projection: Duration,
    last_summary_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VisualProjectionTelemetry {
    Unchanged,
    Updated { payload_len: usize, sent: bool },
    Oversized,
    Error,
}

struct VisualProjectionContext<'a> {
    request: &'a AttachVisualAdapterRequest,
    revision_key: String,
    surface_id: Uuid,
    pane_id: Uuid,
    content_revision: u64,
    projection_elapsed: Duration,
}

impl DecorationRendererCache {
    fn replace_if_newer(&mut self, scene: DecorationScene) -> bool {
        if scene.revision < self.revision {
            return false;
        }
        self.revision = scene.revision;
        self.surfaces = scene.surfaces;
        self.layer_revisions = self
            .surfaces
            .iter()
            .map(|(id, surface)| {
                (
                    *id,
                    [
                        surface_layer_revision(surface, RenderExtensionLayer::BeforePaneContent),
                        surface_layer_revision(surface, RenderExtensionLayer::AfterPaneContent),
                    ],
                )
            })
            .collect();
        true
    }

    /// The revision of a surface's layer: a hash of everything its scene is built from besides
    /// the rect and context the host asks with, so it changes whenever the scene can.
    fn layer_revision(&self, surface_id: &Uuid, layer: RenderExtensionLayer) -> Option<u64> {
        let surface = self.surfaces.get(surface_id)?;
        let index = match layer {
            RenderExtensionLayer::BeforePaneContent => 0,
            RenderExtensionLayer::AfterPaneContent => 1,
        };
        Some(self.layer_revisions.get(surface_id).map_or_else(
            || surface_layer_revision(surface, layer),
            |revisions| revisions[index],
        ))
    }

    fn set_scene_receiver(&mut self, rx: tokio::sync::watch::Receiver<Arc<DecorationScene>>) {
        self.scene_rx = Some(rx);
    }

    fn refresh_from_state_channel(&mut self) {
        let Some(rx) = self.scene_rx.as_mut() else {
            return;
        };
        if let Ok(true) = rx.has_changed() {
            let scene = rx.borrow_and_update().as_ref().clone();
            self.replace_if_newer(scene);
        }
    }

    fn surface(&self, surface_id: &Uuid) -> Option<&SurfaceDecoration> {
        self.surfaces.get(surface_id)
    }

    fn forget_surface(&mut self, surface_id: &Uuid) {
        self.surfaces.remove(surface_id);
        self.layer_revisions.remove(surface_id);
        let suffix = format!(":{surface_id}");
        self.visual_last_revision
            .retain(|key, _| !key.ends_with(&suffix));
        self.visual_last_payload_hash
            .retain(|key, _| !key.ends_with(&suffix));
        self.visual_adapter_cache
            .retain(|key, _| !key.ends_with(&suffix));
        self.visual_stats.retain(|key, _| !key.ends_with(&suffix));
    }
}

/// Render extension that applies the decoration plugin's
/// per-surface paint commands to the attach render stream.
struct DecorationRenderExtension {
    name: String,
    cache: Arc<Mutex<DecorationRendererCache>>,
}

impl AttachRenderExtension for DecorationRenderExtension {
    fn name(&self) -> &str {
        &self.name
    }

    fn surface_chrome_with_context(
        &self,
        _surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        context: &RenderExtensionContext,
    ) -> bmux_plugin::RenderSurfaceChrome {
        if context.surface_role == bmux_plugin::RenderSurfaceRole::Overlay {
            bmux_plugin::RenderSurfaceChrome::Extension
        } else {
            bmux_plugin::RenderSurfaceChrome::Fallback
        }
    }

    fn refresh_state(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.refresh_from_state_channel();
        }
    }

    fn render_layer_revision(&self, surface_id: Uuid, layer: RenderExtensionLayer) -> Option<u64> {
        let cache = self.cache.lock().ok()?;
        cache.layer_revision(&surface_id, layer)
    }

    fn render_layer_scene_with_context(
        &self,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Option<RenderLayerScene> {
        let Ok(cache) = self.cache.lock() else {
            return Some(RenderLayerScene::new(None, Vec::new()));
        };
        let Some(surface) = cache.surface(&surface_id) else {
            if context.surface_role == bmux_plugin::RenderSurfaceRole::Overlay
                && layer == RenderExtensionLayer::AfterPaneContent
            {
                return Some(RenderLayerScene::new(
                    None,
                    vec![bmux_plugin::RenderSceneItem::border(
                        bmux_plugin::RenderSceneItemKey::new("overlay-border"),
                        0,
                        *surface_rect,
                        bmux_plugin::BorderGlyphs::rounded(),
                        bmux_plugin::RenderStyle::new()
                            .named_foreground(bmux_plugin::RenderNamedColor::BrightCyan),
                    )],
                ));
            }
            return Some(RenderLayerScene::new(None, Vec::new()));
        };
        let scene_capabilities = scene_capabilities_from_terminal(context.capabilities);
        Some(render_scene_for_surface_layer_with_capabilities(
            surface_id,
            surface,
            layer,
            context.capabilities,
            scene_capabilities,
            &context.opaque_occluders,
        ))
    }

    fn content_rect_override(&self, surface_id: Uuid) -> Option<ExtensionRect> {
        let cache = self.cache.lock().ok()?;
        let surface = cache.surface(&surface_id)?;
        Some(extension_rect_from_scene(&surface.content_rect))
    }

    fn input_codec(
        &self,
        endpoint: &bmux_plugin::AttachInputEndpoint,
    ) -> Option<Arc<dyn bmux_plugin::AttachInputCodec>> {
        let codec = bmux_decoration_plugin::input_codec();
        (codec.endpoint() == *endpoint).then_some(codec)
    }

    fn input_hooks(&self) -> Vec<AttachInputHook> {
        let Ok(cache) = self.cache.lock() else {
            return Vec::new();
        };
        cache
            .scene_rx
            .as_ref()
            .map(|rx| {
                rx.borrow()
                    .input_hooks
                    .iter()
                    .map(scene_input_hook_to_attach)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn visual_adapter_requests(&self) -> Vec<AttachVisualAdapterRequest> {
        let Ok(cache) = self.cache.lock() else {
            return Vec::new();
        };
        cache
            .scene_rx
            .as_ref()
            .map(|rx| {
                rx.borrow()
                    .visual_adapters
                    .iter()
                    .map(scene_visual_adapter_request_to_attach)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn observe_visual_frame(
        &self,
        frame: &dyn AttachVisualFrameView,
        updates: &mut Vec<AttachVisualProjectionUpdate>,
    ) {
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        let requests = cache
            .scene_rx
            .as_ref()
            .map(|rx| {
                rx.borrow()
                    .visual_adapters
                    .iter()
                    .map(scene_visual_adapter_request_to_attach)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let now = Instant::now();
        for request in requests {
            observe_visual_request(&mut cache, frame, &request, now, updates);
        }
    }

    fn surface_removed(&self, surface_id: Uuid) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.forget_surface(&surface_id);
        }
    }
}

fn layer_paint_commands(
    surface: &SurfaceDecoration,
    layer: RenderExtensionLayer,
) -> &[PaintCommand] {
    match layer {
        RenderExtensionLayer::BeforePaneContent => &surface.before_content_paint_commands,
        RenderExtensionLayer::AfterPaneContent => &surface.paint_commands,
    }
}

fn extension_rect_from_scene(rect: &SceneRect) -> ExtensionRect {
    ExtensionRect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    }
}

const fn scene_capabilities_from_terminal(
    capabilities: TerminalRenderCapabilities,
) -> SceneRenderCapabilities {
    SceneRenderCapabilities {
        truecolor: capabilities.truecolor,
        unicode_box_drawing: capabilities.unicode_box_drawing,
        unicode_block_elements: capabilities.unicode_block_elements,
        graphics_kitty: capabilities.kitty_graphics,
        graphics_sixel: capabilities.sixel,
        graphics_iterm2: capabilities.iterm2_inline_images,
        graphics_alpha: capabilities.graphics_alpha,
        cell_pixels: capabilities.has_cell_pixels(),
    }
}

fn observe_visual_request(
    cache: &mut DecorationRendererCache,
    frame: &dyn AttachVisualFrameView,
    request: &AttachVisualAdapterRequest,
    now: Instant,
    updates: &mut Vec<AttachVisualProjectionUpdate>,
) {
    let min_interval = Duration::from_millis(request.min_interval_ms());
    if !min_interval.is_zero()
        && cache
            .visual_last_at
            .get(&request.id)
            .is_some_and(|last| now.duration_since(*last) < min_interval)
    {
        return;
    }
    let Some(adapter) = cache.visual_adapters.get(&request.adapter) else {
        return;
    };
    let request_started = Instant::now();
    let mut projected = false;
    for index in 0..frame.surface_count() {
        if request_started.elapsed() > VISUAL_REQUEST_BUDGET {
            let skipped = frame.surface_count().saturating_sub(index);
            record_visual_budget_skip(cache, request, skipped);
            tracing::warn!(
                request_id = %request.id,
                adapter = %request.adapter,
                budget_ms = VISUAL_REQUEST_BUDGET.as_millis(),
                skipped_surfaces = skipped,
                "visual adapter request exceeded frame budget; remaining surfaces deferred",
            );
            break;
        }
        let Some(surface) = frame.surface(index) else {
            continue;
        };
        if request.scope == "focused-pane" && !surface.focused() {
            continue;
        }
        projected |= observe_visual_surface(cache, surface, request, adapter.as_ref(), updates);
    }
    if projected {
        cache.visual_last_at.insert(request.id.clone(), now);
    }
}

fn observe_visual_surface(
    cache: &mut DecorationRendererCache,
    surface: &dyn bmux_plugin::AttachVisualSurfaceView,
    request: &AttachVisualAdapterRequest,
    adapter: &dyn bmux_plugin::AttachVisualAdapter,
    updates: &mut Vec<AttachVisualProjectionUpdate>,
) -> bool {
    let revision_key = visual_projection_cache_key(request, surface.surface_id());
    let content_revision = surface.content_revision();
    if request.dirty_only
        && cache
            .visual_last_revision
            .get(&revision_key)
            .is_some_and(|revision| *revision == content_revision)
    {
        return false;
    }
    if !cache.visual_adapter_cache.contains_key(&revision_key)
        && let Some(adapter_cache) = adapter.new_cache(request)
    {
        cache
            .visual_adapter_cache
            .insert(revision_key.clone(), adapter_cache);
    }
    let mut scratch = Vec::new();
    let projection_started = Instant::now();
    let result = {
        let adapter_cache = cache
            .visual_adapter_cache
            .get_mut(&revision_key)
            .map(|adapter_cache| adapter_cache.as_mut() as &mut dyn std::any::Any);
        adapter.project_incremental_cached(surface, request, adapter_cache, &mut scratch)
    };
    let projection_elapsed = projection_started.elapsed();
    if projection_elapsed > SLOW_VISUAL_PROJECTION {
        tracing::debug!(
            request_id = %request.id,
            adapter = %request.adapter,
            surface_id = %surface.surface_id(),
            elapsed_ms = projection_elapsed.as_millis(),
            "slow visual adapter projection",
        );
    }
    let context = VisualProjectionContext {
        request,
        revision_key,
        surface_id: surface.surface_id(),
        pane_id: surface.pane_id(),
        content_revision,
        projection_elapsed,
    };
    handle_visual_projection_result(cache, context, result, updates)
}

fn handle_visual_projection_result(
    cache: &mut DecorationRendererCache,
    context: VisualProjectionContext<'_>,
    result: Result<AttachVisualProjectionResult, String>,
    updates: &mut Vec<AttachVisualProjectionUpdate>,
) -> bool {
    let VisualProjectionContext {
        request,
        revision_key,
        surface_id,
        pane_id,
        content_revision,
        projection_elapsed,
    } = context;
    match result {
        Ok(AttachVisualProjectionResult::Unchanged) => {
            record_visual_projection_stats(
                cache,
                &revision_key,
                request,
                surface_id,
                projection_elapsed,
                VisualProjectionTelemetry::Unchanged,
            );
            cache
                .visual_last_revision
                .insert(revision_key, content_revision);
            true
        }
        Ok(AttachVisualProjectionResult::Updated(output))
            if output.payload.len() <= request.max_bytes as usize =>
        {
            let payload_len = output.payload.len();
            cache
                .visual_last_revision
                .insert(revision_key.clone(), content_revision);
            let sent = maybe_push_visual_projection_update(
                cache,
                request,
                surface_id,
                pane_id,
                revision_key.clone(),
                output,
                updates,
            );
            record_visual_projection_stats(
                cache,
                &revision_key,
                request,
                surface_id,
                projection_elapsed,
                VisualProjectionTelemetry::Updated { payload_len, sent },
            );
            true
        }
        Ok(AttachVisualProjectionResult::Updated(_)) => {
            record_visual_projection_stats(
                cache,
                &revision_key,
                request,
                surface_id,
                projection_elapsed,
                VisualProjectionTelemetry::Oversized,
            );
            tracing::warn!(
                request_id = %request.id,
                adapter = %request.adapter,
                max_bytes = request.max_bytes,
                "visual adapter output exceeded request limit",
            );
            false
        }
        Err(error) => {
            record_visual_projection_stats(
                cache,
                &revision_key,
                request,
                surface_id,
                projection_elapsed,
                VisualProjectionTelemetry::Error,
            );
            tracing::warn!(
                request_id = %request.id,
                adapter = %request.adapter,
                %error,
                "visual adapter projection failed",
            );
            false
        }
    }
}

fn maybe_push_visual_projection_update(
    cache: &mut DecorationRendererCache,
    request: &AttachVisualAdapterRequest,
    surface_id: Uuid,
    pane_id: Uuid,
    revision_key: String,
    output: bmux_plugin::AttachVisualAdapterOutput,
    updates: &mut Vec<AttachVisualProjectionUpdate>,
) -> bool {
    let payload_hash = hash_visual_payload(&output.payload);
    if cache
        .visual_last_payload_hash
        .get(&revision_key)
        .is_some_and(|hash| *hash == payload_hash)
    {
        return false;
    }
    cache
        .visual_last_payload_hash
        .insert(revision_key, payload_hash);
    updates.push(AttachVisualProjectionUpdate {
        request_id: request.id.clone(),
        event_kind: request.event_kind.clone(),
        surface_id,
        pane_id,
        encoding: output.encoding,
        payload: output.payload,
    });
    true
}

fn record_visual_budget_skip(
    cache: &mut DecorationRendererCache,
    request: &AttachVisualAdapterRequest,
    skipped_surfaces: usize,
) {
    let key = visual_request_stats_key(request);
    let stats = cache.visual_stats.entry(key.clone()).or_default();
    stats.budget_skips = stats
        .budget_skips
        .saturating_add(u64::try_from(skipped_surfaces).unwrap_or(u64::MAX));
    maybe_log_visual_stats(&key, request, None, stats);
}

fn record_visual_projection_stats(
    cache: &mut DecorationRendererCache,
    revision_key: &str,
    request: &AttachVisualAdapterRequest,
    surface_id: Uuid,
    elapsed: Duration,
    telemetry: VisualProjectionTelemetry,
) {
    let stats = cache
        .visual_stats
        .entry(revision_key.to_string())
        .or_default();
    stats.projections = stats.projections.saturating_add(1);
    stats.total_projection += elapsed;
    stats.max_projection = stats.max_projection.max(elapsed);
    if elapsed > SLOW_VISUAL_PROJECTION {
        stats.slow_projections = stats.slow_projections.saturating_add(1);
    }
    match telemetry {
        VisualProjectionTelemetry::Unchanged => {
            stats.unchanged = stats.unchanged.saturating_add(1);
        }
        VisualProjectionTelemetry::Updated { payload_len, sent } => {
            stats.updated = stats.updated.saturating_add(1);
            stats.payload_bytes = stats
                .payload_bytes
                .saturating_add(u64::try_from(payload_len).unwrap_or(u64::MAX));
            if sent {
                stats.sent_updates = stats.sent_updates.saturating_add(1);
            } else {
                stats.duplicate_suppressed = stats.duplicate_suppressed.saturating_add(1);
            }
        }
        VisualProjectionTelemetry::Oversized => {
            stats.oversized = stats.oversized.saturating_add(1);
        }
        VisualProjectionTelemetry::Error => {
            stats.errors = stats.errors.saturating_add(1);
        }
    }
    maybe_log_visual_stats(revision_key, request, Some(surface_id), stats);
}

fn maybe_log_visual_stats(
    stats_key: &str,
    request: &AttachVisualAdapterRequest,
    surface_id: Option<Uuid>,
    stats: &mut VisualProjectionStats,
) {
    let now = Instant::now();
    if let Some(last_summary_at) = stats.last_summary_at {
        if now.duration_since(last_summary_at) < VISUAL_STATS_LOG_INTERVAL {
            return;
        }
    } else {
        stats.last_summary_at = Some(now);
        return;
    }
    let average_projection_us = if stats.projections == 0 {
        0
    } else {
        u64::try_from(stats.total_projection.as_micros() / u128::from(stats.projections))
            .unwrap_or(u64::MAX)
    };
    let surface_id = surface_id.map_or_else(|| "request".to_string(), |id| id.to_string());
    tracing::debug!(
        request_id = %request.id,
        adapter = %request.adapter,
        surface_id = %surface_id,
        stats_key,
        projections = stats.projections,
        unchanged = stats.unchanged,
        updated = stats.updated,
        sent_updates = stats.sent_updates,
        duplicate_suppressed = stats.duplicate_suppressed,
        oversized = stats.oversized,
        errors = stats.errors,
        budget_skips = stats.budget_skips,
        payload_bytes = stats.payload_bytes,
        slow_projections = stats.slow_projections,
        avg_projection_us = average_projection_us,
        max_projection_us = stats.max_projection.as_micros(),
        "visual adapter projection stats",
    );
    stats.last_summary_at = Some(now);
}

fn hash_visual_payload(payload: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    payload.hash(&mut hasher);
    hasher.finish()
}

fn visual_request_stats_key(request: &AttachVisualAdapterRequest) -> String {
    format!("{}:{}:request", request.id, request.adapter)
}

fn visual_projection_cache_key(request: &AttachVisualAdapterRequest, surface_id: Uuid) -> String {
    let mut settings_hash = std::collections::hash_map::DefaultHasher::new();
    request.area.hash(&mut settings_hash);
    request.settings.hash(&mut settings_hash);
    format!(
        "{}:{}:{}:{}",
        request.id,
        request.adapter,
        settings_hash.finish(),
        surface_id
    )
}

fn scene_visual_adapter_request_to_attach(
    request: &VisualAdapterRequest,
) -> AttachVisualAdapterRequest {
    AttachVisualAdapterRequest {
        id: request.id.clone(),
        adapter: request.adapter.clone(),
        owner_plugin_id: request.owner_plugin_id.clone(),
        event_kind: request.event_kind.clone(),
        scope: request.scope.clone(),
        area: request.area.clone(),
        max_hz: request.max_hz,
        dirty_only: request.dirty_only,
        max_bytes: request.max_bytes,
        settings: request.settings.clone(),
    }
}

fn scene_input_hook_to_attach(
    hook: &bmux_scene_protocol::scene_protocol::InputHook,
) -> AttachInputHook {
    AttachInputHook {
        id: hook.id.clone(),
        owner_plugin_id: hook.owner_plugin_id.clone(),
        priority: hook.priority,
        endpoint: AttachInputEndpoint {
            capability: hook.endpoint.capability.clone(),
            interface_id: hook.endpoint.interface_id.clone(),
            operation: hook.endpoint.operation.clone(),
        },
        filter: AttachInputHookFilter {
            mouse_phases: hook.filter.mouse_phases.clone(),
            keys: hook.filter.keys.clone(),
            scope: hook.filter.scope.clone(),
            min_interval_ms: hook.filter.min_interval_ms,
        },
    }
}

fn surface_layer_revision(surface: &SurfaceDecoration, layer: RenderExtensionLayer) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_revision_part(&mut hasher, &surface.rect);
    hash_revision_part(&mut hasher, &surface.content_rect);
    match layer {
        RenderExtensionLayer::BeforePaneContent => {
            hash_revision_part(&mut hasher, &surface.before_content_paint_commands);
        }
        RenderExtensionLayer::AfterPaneContent => {
            hash_revision_part(&mut hasher, &surface.paint_commands);
        }
    }
    hasher.finish()
}

fn hash_revision_part(hasher: &mut impl Hasher, value: &impl serde::Serialize) {
    if let Ok(bytes) = serde_json::to_vec(value) {
        bytes.hash(hasher);
    }
}

fn paint_command_sort_key(index: usize, command: &PaintCommand) -> (i16, usize) {
    (paint_command_z(command), index)
}

#[derive(Clone, Debug)]
struct PaintCommandRegions {
    sort_key: (i16, usize),
    regions: Vec<ExtensionRect>,
}

fn collect_terminal_cell_regions(commands: &[PaintCommand]) -> Vec<PaintCommandRegions> {
    commands
        .iter()
        .enumerate()
        .filter(|(_, command)| paint_command_paints_terminal_cells(command))
        .map(|(index, command)| PaintCommandRegions {
            sort_key: paint_command_sort_key(index, command),
            regions: paint_command_damage(command).collect(),
        })
        .collect()
}

fn paint_command_paints_terminal_cells(command: &PaintCommand) -> bool {
    match command {
        PaintCommand::Text { text, .. } | PaintCommand::GradientRun { text, .. } => {
            !text.is_empty()
        }
        PaintCommand::FilledRect { rect, glyph, .. } => {
            rect.w > 0 && rect.h > 0 && !glyph.is_empty()
        }
        PaintCommand::CellGrid { cols, cells, .. } => {
            *cols > 0 && cells.iter().any(|cell| !cell.glyph.is_empty())
        }
        PaintCommand::BoxBorder { rect, glyphs, .. } => {
            rect.w >= 2 && rect.h >= 2 && !matches!(glyphs, SceneBorderGlyphs::None)
        }
        PaintCommand::SemanticBorder { .. } => false,
    }
}

fn collect_paint_command_opaque_regions(commands: &[PaintCommand]) -> Vec<PaintCommandRegions> {
    commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let regions = paint_command_opaque_regions(command);
            (!regions.is_empty()).then(|| PaintCommandRegions {
                sort_key: paint_command_sort_key(index, command),
                regions,
            })
        })
        .collect()
}

fn command_regions_after(
    commands: &[PaintCommandRegions],
    command_index: usize,
    command: &PaintCommand,
) -> Vec<ExtensionRect> {
    let command_key = paint_command_sort_key(command_index, command);
    commands
        .iter()
        .filter(|candidate| candidate.sort_key > command_key)
        .flat_map(|candidate| candidate.regions.iter().copied())
        .collect()
}

fn paint_command_opaque_regions(command: &PaintCommand) -> Vec<ExtensionRect> {
    match command {
        PaintCommand::Text {
            col,
            row,
            text,
            style,
            ..
        } if style_paints_opaque_background(style) && !text.is_empty() => {
            vec![ExtensionRect::new(
                *col,
                *row,
                render_text_width_u16(text),
                1,
            )]
        }
        PaintCommand::GradientRun {
            col,
            row,
            text,
            from_style,
            to_style,
            ..
        } if style_paints_opaque_background(from_style)
            && style_paints_opaque_background(to_style)
            && !text.is_empty() =>
        {
            vec![ExtensionRect::new(
                *col,
                *row,
                render_text_width_u16(text),
                1,
            )]
        }
        PaintCommand::FilledRect { rect, style, .. }
            if style_paints_opaque_background(style) && rect.w > 0 && rect.h > 0 =>
        {
            vec![render_rect_from_scene(rect)]
        }
        PaintCommand::CellGrid {
            origin_col,
            origin_row,
            cols,
            cells,
            ..
        } if *cols > 0 => cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| style_paints_opaque_background(&cell.style))
            .filter_map(|(index, _)| {
                let index = u16::try_from(index).ok()?;
                Some(ExtensionRect::new(
                    origin_col.saturating_add(index % *cols),
                    origin_row.saturating_add(index / *cols),
                    1,
                    1,
                ))
            })
            .collect(),
        PaintCommand::BoxBorder { rect, style, .. }
            if style_paints_opaque_background(style) && rect.w >= 2 && rect.h >= 2 =>
        {
            border_cell_regions(rect)
        }
        PaintCommand::Text { .. }
        | PaintCommand::FilledRect { .. }
        | PaintCommand::GradientRun { .. }
        | PaintCommand::CellGrid { .. }
        | PaintCommand::BoxBorder { .. }
        | PaintCommand::SemanticBorder { .. } => Vec::new(),
    }
}

const fn style_paints_opaque_background(style: &SceneStyle) -> bool {
    style.bg.is_some() || style.reverse
}

fn border_cell_regions(rect: &SceneRect) -> Vec<ExtensionRect> {
    vec![
        ExtensionRect::new(rect.x, rect.y, rect.w, 1),
        ExtensionRect::new(
            rect.x,
            rect.y.saturating_add(rect.h.saturating_sub(1)),
            rect.w,
            1,
        ),
        ExtensionRect::new(rect.x, rect.y, 1, rect.h),
        ExtensionRect::new(
            rect.x.saturating_add(rect.w.saturating_sub(1)),
            rect.y,
            1,
            rect.h,
        ),
    ]
}

const fn render_rect_from_scene(rect: &SceneRect) -> ExtensionRect {
    ExtensionRect::new(rect.x, rect.y, rect.w, rect.h)
}

fn cell_occluded(col: u16, row: u16, occluders: &[ExtensionRect]) -> bool {
    let cell = ExtensionRect::new(col, row, 1, 1);
    occluders.iter().any(|occluder| occluder.intersects(cell))
}

#[must_use]
pub fn render_ops_for_paint_commands_with_capabilities(
    paint_commands: &[PaintCommand],
    capabilities: SceneRenderCapabilities,
) -> Option<Vec<RenderOp>> {
    let mut ordered: Vec<(usize, &PaintCommand)> = paint_commands.iter().enumerate().collect();
    ordered.sort_by_key(|(index, command)| paint_command_sort_key(*index, command));

    let terminal_cell_regions = collect_terminal_cell_regions(paint_commands);
    let mut ops = Vec::new();
    for (command_index, command) in ordered {
        let occluders = command_regions_after(&terminal_cell_regions, command_index, command);
        push_render_ops_for_command_with_occluders(&mut ops, command, capabilities, &occluders)?;
    }
    Some(ops)
}

fn render_scene_for_surface_layer_with_capabilities(
    surface_id: Uuid,
    surface: &SurfaceDecoration,
    layer: RenderExtensionLayer,
    terminal_capabilities: TerminalRenderCapabilities,
    scene_capabilities: SceneRenderCapabilities,
    global_occluders: &[ExtensionRect],
) -> RenderLayerScene {
    let commands = layer_paint_commands(surface, layer);
    let mut ordered: Vec<(usize, &PaintCommand)> = commands.iter().enumerate().collect();
    ordered.sort_by_key(|(index, command)| paint_command_sort_key(*index, command));

    let layer_key_prefix = match layer {
        RenderExtensionLayer::BeforePaneContent => "before",
        RenderExtensionLayer::AfterPaneContent => "after",
    };
    let terminal_cell_regions = collect_terminal_cell_regions(commands);
    let opaque_commands = collect_paint_command_opaque_regions(commands);
    let context = RenderSceneCommandContext {
        surface_id,
        terminal_cell_regions: &terminal_cell_regions,
        opaque_commands: &opaque_commands,
        layer_key_prefix,
        terminal_capabilities,
        scene_capabilities,
        global_occluders,
    };
    let mut items = Vec::new();
    for (command_index, command) in ordered {
        push_render_scene_items_for_command(&mut items, context, command_index, command);
    }
    RenderLayerScene::new(Some(surface_layer_revision(surface, layer)), items)
}

#[derive(Clone, Copy)]
struct RenderSceneCommandContext<'a> {
    surface_id: Uuid,
    terminal_cell_regions: &'a [PaintCommandRegions],
    opaque_commands: &'a [PaintCommandRegions],
    layer_key_prefix: &'a str,
    terminal_capabilities: TerminalRenderCapabilities,
    scene_capabilities: SceneRenderCapabilities,
    global_occluders: &'a [ExtensionRect],
}

fn push_render_scene_items_for_command(
    items: &mut Vec<RenderSceneItem>,
    context: RenderSceneCommandContext<'_>,
    command_index: usize,
    command: &PaintCommand,
) {
    let z = paint_command_z(command);
    let key_prefix = format!("{}:cmd-{command_index:06}", context.layer_key_prefix);
    let mut terminal_cell_occluders =
        command_regions_after(context.terminal_cell_regions, command_index, command);
    terminal_cell_occluders.extend_from_slice(context.global_occluders);
    let mut graphic_occluders =
        command_regions_after(context.opaque_commands, command_index, command);
    graphic_occluders.extend_from_slice(context.global_occluders);
    if let Some(graphics) = raster_border::semantic_border_graphic_items_with_occlusion(
        context.surface_id,
        u64::try_from(command_index).unwrap_or(u64::MAX),
        command,
        context.terminal_capabilities,
        context.scene_capabilities,
        &graphic_occluders,
    ) {
        for (graphic_index, item) in graphics.into_iter().enumerate() {
            let RenderLayerItem::Graphic(graphic) = item else {
                continue;
            };
            items.push(RenderSceneItem::terminal_graphic(
                RenderSceneItemKey::new(format!("{key_prefix}:graphic-{graphic_index:02}")),
                raster_border::semantic_border_terminal_graphic_z(z),
                graphic,
            ));
        }
        return;
    }

    let mut ops = Vec::new();
    let Some(()) = push_render_ops_for_command_with_occluders(
        &mut ops,
        command,
        context.scene_capabilities,
        &terminal_cell_occluders,
    ) else {
        return;
    };
    for (op_index, op) in ops.into_iter().enumerate() {
        items.push(RenderSceneItem::op(
            RenderSceneItemKey::new(format!("{key_prefix}:op-{op_index:02}")),
            z,
            op,
        ));
    }
}

fn push_render_ops_for_command_with_occluders(
    ops: &mut Vec<RenderOp>,
    command: &PaintCommand,
    capabilities: SceneRenderCapabilities,
    occluders: &[ExtensionRect],
) -> Option<()> {
    match command {
        PaintCommand::Text {
            col,
            row,
            text,
            style,
            ..
        } => {
            if text.is_empty() {
                return Some(());
            }
            push_text_run_render_ops(
                ops,
                *col,
                *row,
                text,
                render_style_from_scene(style),
                occluders,
            );
        }
        PaintCommand::FilledRect {
            rect, glyph, style, ..
        } => push_filled_rect_command_ops(ops, rect, glyph, style, occluders)?,
        PaintCommand::GradientRun {
            col,
            row,
            text,
            axis,
            from_style,
            to_style,
            ..
        } => {
            let mut gradient_ops = Vec::new();
            push_gradient_run_ops(
                &mut gradient_ops,
                *col,
                *row,
                text,
                *axis,
                from_style,
                to_style,
            );
            push_clipped_render_ops(ops, gradient_ops, occluders);
        }
        PaintCommand::CellGrid {
            origin_col,
            origin_row,
            cols,
            cells,
            ..
        } => push_cell_grid_command_ops(ops, *origin_col, *origin_row, *cols, cells, occluders)?,
        PaintCommand::BoxBorder {
            rect,
            glyphs,
            style,
            ..
        } => {
            push_border_render_op(ops, rect, glyphs, style, occluders)?;
        }
        PaintCommand::SemanticBorder {
            rect,
            style,
            fallback_glyphs,
            when,
            ..
        } => {
            if !capability_query_matches(when.as_ref(), capabilities) {
                return Some(());
            }
            push_border_render_op(ops, rect, fallback_glyphs, style, occluders)?;
        }
    }
    Some(())
}

fn push_filled_rect_command_ops(
    ops: &mut Vec<RenderOp>,
    rect: &SceneRect,
    glyph: &str,
    style: &SceneStyle,
    occluders: &[ExtensionRect],
) -> Option<()> {
    if rect.w == 0 || rect.h == 0 || glyph.is_empty() {
        return Some(());
    }
    let style = render_style_from_scene(style);
    let rect = extension_rect_from_scene(rect);
    if occluders.is_empty() {
        if glyph == " " {
            ops.push(RenderOp::ClearRect { rect, style });
        } else {
            ops.push(RenderOp::FillRect {
                rect,
                ch: render_single_display_cell_char(glyph)?,
                style,
            });
        }
    } else {
        let ch = if glyph == " " {
            ' '
        } else {
            render_single_display_cell_char(glyph)?
        };
        push_fill_rect_render_ops(ops, rect, ch, style, occluders);
    }
    Some(())
}

fn push_cell_grid_command_ops(
    ops: &mut Vec<RenderOp>,
    origin_col: u16,
    origin_row: u16,
    cols: u16,
    cells: &[SceneCell],
    occluders: &[ExtensionRect],
) -> Option<()> {
    if cols == 0 || cells.is_empty() {
        return Some(());
    }
    let mut rows = render_cell_grid_rows(cols, cells)?;
    if !occluders.is_empty() {
        clip_cell_grid_rows_to_occluders(&mut rows, origin_col, origin_row, occluders);
    }
    ops.push(RenderOp::CellGrid {
        x: origin_col,
        y: origin_row,
        rows,
    });
    Some(())
}

fn clip_cell_grid_rows_to_occluders(
    rows: &mut [Vec<RenderCell>],
    origin_col: u16,
    origin_row: u16,
    occluders: &[ExtensionRect],
) {
    for (row_offset, row) in rows.iter_mut().enumerate() {
        let Ok(row_offset) = u16::try_from(row_offset) else {
            break;
        };
        for (col_offset, cell) in row.iter_mut().enumerate() {
            let Ok(col_offset) = u16::try_from(col_offset) else {
                break;
            };
            let col = origin_col.saturating_add(col_offset);
            let row = origin_row.saturating_add(row_offset);
            if cell_occluded(col, row, occluders) {
                cell.ch = None;
            }
        }
    }
}

fn push_text_run_render_ops(
    ops: &mut Vec<RenderOp>,
    x: u16,
    y: u16,
    text: &str,
    style: RenderStyle,
    occluders: &[ExtensionRect],
) {
    if occluders.is_empty() {
        ops.push(RenderOp::TextRun {
            x,
            y,
            text: text.to_string(),
            style,
        });
        return;
    }
    let mut run = String::new();
    let mut run_x = x;
    let mut col = x;
    for ch in text.chars() {
        let width = render_text_width_u16(ch.encode_utf8(&mut [0; 4])).max(1);
        let occluded =
            (0..width).any(|offset| cell_occluded(col.saturating_add(offset), y, occluders));
        if occluded {
            flush_border_text_run(ops, &mut run, run_x, y, style);
            col = col.saturating_add(width);
            run_x = col;
        } else {
            if run.is_empty() {
                run_x = col;
            }
            run.push(ch);
            col = col.saturating_add(width);
        }
    }
    flush_border_text_run(ops, &mut run, run_x, y, style);
}

fn push_fill_rect_render_ops(
    ops: &mut Vec<RenderOp>,
    rect: ExtensionRect,
    ch: char,
    style: RenderStyle,
    occluders: &[ExtensionRect],
) {
    for row in rect.y..rect.bottom() {
        let mut run = String::new();
        let mut run_x = rect.x;
        for col in rect.x..rect.right() {
            if cell_occluded(col, row, occluders) {
                flush_border_text_run(ops, &mut run, run_x, row, style);
                run_x = col.saturating_add(1);
            } else {
                if run.is_empty() {
                    run_x = col;
                }
                run.push(ch);
            }
        }
        flush_border_text_run(ops, &mut run, run_x, row, style);
    }
}

fn push_clipped_render_ops(
    ops: &mut Vec<RenderOp>,
    source_ops: Vec<RenderOp>,
    occluders: &[ExtensionRect],
) {
    if occluders.is_empty() {
        ops.extend(source_ops);
        return;
    }
    for op in source_ops {
        match op {
            RenderOp::TextRun { x, y, text, style } => {
                push_text_run_render_ops(ops, x, y, &text, style, occluders);
            }
            RenderOp::FillRect { rect, ch, style } => {
                push_fill_rect_render_ops(ops, rect, ch, style, occluders);
            }
            RenderOp::ClearRect { rect, style } => {
                push_fill_rect_render_ops(ops, rect, ' ', style, occluders);
            }
            RenderOp::EraseRowSegment { x, y, width, style } => {
                push_fill_rect_render_ops(
                    ops,
                    ExtensionRect::new(x, y, width, 1),
                    ' ',
                    style,
                    occluders,
                );
            }
            RenderOp::CellGrid { x, y, mut rows } => {
                clip_cell_grid_rows_to_occluders(&mut rows, x, y, occluders);
                ops.push(RenderOp::CellGrid { x, y, rows });
            }
            RenderOp::StyledText { .. } | RenderOp::Border { .. } => ops.push(op),
        }
    }
}

fn push_border_render_op(
    ops: &mut Vec<RenderOp>,
    rect: &SceneRect,
    glyphs: &SceneBorderGlyphs,
    style: &SceneStyle,
    occluders: &[ExtensionRect],
) -> Option<()> {
    if rect.w < 2 || rect.h < 2 || matches!(glyphs, SceneBorderGlyphs::None) {
        return Some(());
    }
    let rect = extension_rect_from_scene(rect);
    let glyphs = render_border_glyphs(glyphs)?;
    let style = render_style_from_scene(style);
    if occluders.is_empty() {
        ops.push(RenderOp::Border {
            rect,
            glyphs,
            style,
        });
        return Some(());
    }
    push_occluded_border_render_ops(ops, rect, glyphs, style, occluders);
    Some(())
}

fn push_occluded_border_render_ops(
    ops: &mut Vec<RenderOp>,
    rect: ExtensionRect,
    glyphs: RenderBorderGlyphs,
    style: RenderStyle,
    occluders: &[ExtensionRect],
) {
    push_occluded_border_row(
        ops,
        rect.x,
        rect.y,
        rect.w,
        glyphs.top_left,
        glyphs.horizontal,
        glyphs.top_right,
        style,
        occluders,
    );
    push_occluded_border_row(
        ops,
        rect.x,
        rect.y.saturating_add(rect.h.saturating_sub(1)),
        rect.w,
        glyphs.bottom_left,
        glyphs.horizontal,
        glyphs.bottom_right,
        style,
        occluders,
    );
    if rect.h <= 2 {
        return;
    }
    for row in rect.y.saturating_add(1)..rect.y.saturating_add(rect.h.saturating_sub(1)) {
        push_border_cell_if_visible(ops, rect.x, row, glyphs.vertical, style, occluders);
        push_border_cell_if_visible(
            ops,
            rect.x.saturating_add(rect.w.saturating_sub(1)),
            row,
            glyphs.vertical,
            style,
            occluders,
        );
    }
}

#[allow(clippy::too_many_arguments)] // Border row lowering keeps glyph roles explicit.
fn push_occluded_border_row(
    ops: &mut Vec<RenderOp>,
    x: u16,
    y: u16,
    width: u16,
    left: char,
    horizontal: char,
    right: char,
    style: RenderStyle,
    occluders: &[ExtensionRect],
) {
    let mut run = String::new();
    let mut run_x = x;
    for offset in 0..width {
        let col = x.saturating_add(offset);
        let ch = if offset == 0 {
            left
        } else if offset == width.saturating_sub(1) {
            right
        } else {
            horizontal
        };
        if cell_occluded(col, y, occluders) {
            flush_border_text_run(ops, &mut run, run_x, y, style);
            run_x = col.saturating_add(1);
        } else {
            if run.is_empty() {
                run_x = col;
            }
            run.push(ch);
        }
    }
    flush_border_text_run(ops, &mut run, run_x, y, style);
}

fn push_border_cell_if_visible(
    ops: &mut Vec<RenderOp>,
    x: u16,
    y: u16,
    ch: char,
    style: RenderStyle,
    occluders: &[ExtensionRect],
) {
    if !cell_occluded(x, y, occluders) {
        ops.push(RenderOp::TextRun {
            x,
            y,
            text: ch.to_string(),
            style,
        });
    }
}

fn flush_border_text_run(
    ops: &mut Vec<RenderOp>,
    run: &mut String,
    x: u16,
    y: u16,
    style: RenderStyle,
) {
    if run.is_empty() {
        return;
    }
    ops.push(RenderOp::TextRun {
        x,
        y,
        text: std::mem::take(run),
        style,
    });
}

const fn paint_command_z(command: &PaintCommand) -> i16 {
    match command {
        PaintCommand::Text { z, .. }
        | PaintCommand::FilledRect { z, .. }
        | PaintCommand::GradientRun { z, .. }
        | PaintCommand::CellGrid { z, .. }
        | PaintCommand::BoxBorder { z, .. }
        | PaintCommand::SemanticBorder { z, .. } => *z,
    }
}

fn push_gradient_run_ops(
    ops: &mut Vec<RenderOp>,
    col: u16,
    row: u16,
    text: &str,
    axis: GradientAxis,
    from_style: &SceneStyle,
    to_style: &SceneStyle,
) {
    let segments: Vec<&str> = text_scalar_segments(text).collect();
    let n = segments.len();
    if n == 0 {
        return;
    }
    if n == 1 {
        ops.push(RenderOp::TextRun {
            x: col,
            y: row,
            text: text.to_string(),
            style: render_style_from_scene(from_style),
        });
        return;
    }

    let mut offset = 0_u16;
    #[allow(clippy::cast_precision_loss)] // Segment count is bounded by terminal UI text length.
    let denom = (n - 1) as f32;
    for (index, segment) in segments.into_iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let t = index as f32 / denom;
        let style = render_style_from_scene(&interpolate_style(from_style, to_style, t));
        match axis {
            GradientAxis::Horizontal => {
                ops.push(RenderOp::TextRun {
                    x: col.saturating_add(offset),
                    y: row,
                    text: segment.to_string(),
                    style,
                });
                offset = offset.saturating_add(render_text_width_u16(segment).max(1));
            }
            GradientAxis::Vertical => {
                ops.push(RenderOp::TextRun {
                    x: col,
                    y: row.saturating_add(offset),
                    text: segment.to_string(),
                    style,
                });
                offset = offset.saturating_add(1);
            }
            GradientAxis::Diagonal => {
                ops.push(RenderOp::TextRun {
                    x: col.saturating_add(offset),
                    y: row.saturating_add(offset),
                    text: segment.to_string(),
                    style,
                });
                offset = offset.saturating_add(1);
            }
        }
    }
}

fn text_scalar_segments(text: &str) -> impl Iterator<Item = &str> {
    let mut chars = text.char_indices().peekable();
    std::iter::from_fn(move || {
        let (start, _) = chars.next()?;
        let end = chars.peek().map_or(text.len(), |(index, _)| *index);
        Some(&text[start..end])
    })
}

#[must_use]
pub fn render_style_from_scene(style: &SceneStyle) -> RenderStyle {
    RenderStyle {
        fg: style.fg.as_ref().map(render_color_from_scene),
        bg: style.bg.as_ref().map(render_color_from_scene),
        bold: style.bold,
        underline: style.underline,
        italic: style.italic,
        reverse: style.reverse,
        dim: style.dim,
        blink: style.blink,
        strikethrough: style.strikethrough,
    }
}

#[must_use]
pub fn render_color_from_scene(color: &SceneColor) -> RenderColor {
    match color {
        SceneColor::Default | SceneColor::Reset => RenderColor::Default,
        SceneColor::Indexed { index } => RenderColor::Indexed(*index),
        SceneColor::Rgb { r, g, b } => RenderColor::Rgb {
            r: *r,
            g: *g,
            b: *b,
        },
        SceneColor::Named { name } => RenderColor::Named(render_named_color_from_scene(*name)),
    }
}

#[must_use]
pub const fn render_named_color_from_scene(color: NamedColor) -> RenderNamedColor {
    match color {
        NamedColor::Black => RenderNamedColor::Black,
        NamedColor::Red => RenderNamedColor::Red,
        NamedColor::Green => RenderNamedColor::Green,
        NamedColor::Yellow => RenderNamedColor::Yellow,
        NamedColor::Blue => RenderNamedColor::Blue,
        NamedColor::Magenta => RenderNamedColor::Magenta,
        NamedColor::Cyan => RenderNamedColor::Cyan,
        NamedColor::White => RenderNamedColor::White,
        NamedColor::BrightBlack => RenderNamedColor::BrightBlack,
        NamedColor::BrightRed => RenderNamedColor::BrightRed,
        NamedColor::BrightGreen => RenderNamedColor::BrightGreen,
        NamedColor::BrightYellow => RenderNamedColor::BrightYellow,
        NamedColor::BrightBlue => RenderNamedColor::BrightBlue,
        NamedColor::BrightMagenta => RenderNamedColor::BrightMagenta,
        NamedColor::BrightCyan => RenderNamedColor::BrightCyan,
        NamedColor::BrightWhite => RenderNamedColor::BrightWhite,
    }
}

#[must_use]
pub fn render_cell_grid_rows(cols: u16, cells: &[SceneCell]) -> Option<Vec<Vec<RenderCell>>> {
    let mut rows = Vec::new();
    for row_cells in cells.chunks(usize::from(cols)) {
        let mut row = Vec::with_capacity(row_cells.len());
        for cell in row_cells {
            row.push(RenderCell {
                ch: if cell.glyph.is_empty() {
                    None
                } else {
                    Some(render_single_display_cell_char(&cell.glyph)?)
                },
                style: render_style_from_scene(&cell.style),
            });
        }
        rows.push(row);
    }
    Some(rows)
}

#[must_use]
pub fn render_border_glyphs(glyphs: &SceneBorderGlyphs) -> Option<RenderBorderGlyphs> {
    let glyphs = border_glyphs_corners_or_custom(glyphs)?;
    Some(RenderBorderGlyphs {
        top_left: render_single_display_cell_char(glyphs.top_left)?,
        top_right: render_single_display_cell_char(glyphs.top_right)?,
        bottom_left: render_single_display_cell_char(glyphs.bottom_left)?,
        bottom_right: render_single_display_cell_char(glyphs.bottom_right)?,
        horizontal: render_single_display_cell_char(glyphs.horizontal)?,
        vertical: render_single_display_cell_char(glyphs.vertical)?,
    })
}

fn paint_command_damage(command: &PaintCommand) -> impl Iterator<Item = ExtensionRect> + '_ {
    let rects: Vec<ExtensionRect> = match command {
        PaintCommand::Text { col, row, text, .. }
        | PaintCommand::GradientRun { col, row, text, .. } => vec![ExtensionRect {
            x: *col,
            y: *row,
            w: render_text_width_u16(text),
            h: 1,
        }],
        PaintCommand::FilledRect { rect, .. } => vec![extension_rect_from_scene(rect)],
        PaintCommand::CellGrid {
            origin_col,
            origin_row,
            cols,
            cells,
            ..
        } => {
            let len = u16::try_from(cells.len()).unwrap_or(u16::MAX);
            let rows = len
                .saturating_add(cols.saturating_sub(1))
                .checked_div(*cols)
                .unwrap_or(0);
            vec![ExtensionRect {
                x: *origin_col,
                y: *origin_row,
                w: *cols,
                h: rows,
            }]
        }
        PaintCommand::BoxBorder { rect, .. } | PaintCommand::SemanticBorder { rect, .. } => {
            border_damage_rects(rect)
        }
    };
    rects.into_iter()
}

fn border_damage_rects(rect: &SceneRect) -> Vec<ExtensionRect> {
    if rect.w == 0 || rect.h == 0 {
        return Vec::new();
    }
    if rect.w < 2 || rect.h < 2 {
        return vec![extension_rect_from_scene(rect)];
    }
    vec![
        ExtensionRect {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: 1,
        },
        ExtensionRect {
            x: rect.x,
            y: rect.y.saturating_add(rect.h.saturating_sub(1)),
            w: rect.w,
            h: 1,
        },
        ExtensionRect {
            x: rect.x,
            y: rect.y.saturating_add(1),
            w: 1,
            h: rect.h.saturating_sub(2),
        },
        ExtensionRect {
            x: rect.x.saturating_add(rect.w.saturating_sub(1)),
            y: rect.y.saturating_add(1),
            w: 1,
            h: rect.h.saturating_sub(2),
        },
    ]
}

/// Build a renderer whose cache is fed by `bus`'s retained scene state; the
/// attach runtime's streaming loop relays scenes onto that bus.
fn create_renderer(
    bus: &bmux_plugin::EventBus,
    visual_adapters: Arc<bmux_plugin::VisualAdapterRegistry>,
) -> DecorationRenderExtension {
    let cache = Arc::new(Mutex::new(DecorationRendererCache {
        visual_adapters,
        ..DecorationRendererCache::default()
    }));
    // Register only when absent: registration replaces an existing retained
    // value, which would erase the latest scene on renderer restart.
    if bus
        .subscribe_state::<DecorationScene>(&SCENE_STATE_KIND)
        .is_err()
    {
        let _ = bus.register_state_channel::<DecorationScene>(
            SCENE_STATE_KIND,
            DecorationScene {
                revision: 0,
                surfaces: BTreeMap::new(),
                animation: None,
                input_hooks: Vec::new(),
                visual_adapters: Vec::new(),
            },
        );
    }
    match bus.subscribe_state::<DecorationScene>(&SCENE_STATE_KIND) {
        Ok((initial, rx)) => {
            let mut guard = cache.lock().expect("new decoration cache is not poisoned");
            guard.replace_if_newer(initial.as_ref().clone());
            guard.set_scene_receiver(rx);
        }
        Err(error) => tracing::warn!(%error, "decoration scene subscription failed"),
    }
    DecorationRenderExtension {
        name: "bmux.decoration.renderer".to_string(),
        cache,
    }
}

/// Create an independent renderer subscribed to the supplied retained scene bus
/// that projects visual requests only with the attach runtime's
/// `visual_adapters`. Dropping the extension releases its cache and receiver;
/// no background task or process-wide registration is installed.
#[must_use]
pub fn renderer_for_bus(
    bus: &bmux_plugin::EventBus,
    visual_adapters: Arc<bmux_plugin::VisualAdapterRegistry>,
) -> Arc<dyn AttachRenderExtension> {
    Arc::new(create_renderer(bus, visual_adapters))
}

/// Relay a scene only to the selected renderer bus.
pub fn publish_relayed_scene_on(bus: &bmux_plugin::EventBus, scene: DecorationScene) {
    if bus.publish_state(&SCENE_STATE_KIND, scene.clone()).is_ok() {
        return;
    }
    let _ = bus.register_state_channel::<DecorationScene>(
        SCENE_STATE_KIND,
        DecorationScene {
            revision: 0,
            surfaces: BTreeMap::new(),
            animation: None,
            input_hooks: Vec::new(),
            visual_adapters: Vec::new(),
        },
    );
    let _ = bus.publish_state(&SCENE_STATE_KIND, scene);
}

#[cfg(test)]
mod tests {
    #[test]
    fn input_codec_is_resolved_from_owning_attach_registry() {
        let registry = bmux_plugin::RenderExtensionRegistry::new();
        let endpoint = bmux_plugin::AttachInputEndpoint {
            capability: "bmux.decoration.write".into(),
            interface_id: "decoration-input-hooks/v3".into(),
            operation: "handle-input".into(),
        };
        assert!(registry.input_codec(&endpoint).is_none());
        registry.register(super::renderer_for_bus(
            &bmux_plugin::EventBus::new(),
            Arc::default(),
        ));
        let codec = registry
            .input_codec(&endpoint)
            .expect("renderer supplies checked codec");
        assert_eq!(codec.endpoint(), endpoint);
        assert!(codec.decode_result(b"legacy response").is_err());
        let mut old = endpoint;
        old.interface_id = "decoration-input-hooks/v2".into();
        assert!(registry.input_codec(&old).is_none());
    }

    use super::*;
    use bmux_scene_protocol::scene_protocol::{TerminalCapability, TerminalCapabilityQuery};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_VISUAL_ADAPTER_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct TestVisualAdapter;

    impl bmux_plugin::AttachVisualAdapter for TestVisualAdapter {
        fn id(&self) -> &'static str {
            "test.visual.constant-output"
        }

        fn project(
            &self,
            _surface: &dyn bmux_plugin::AttachVisualSurfaceView,
            _request: &AttachVisualAdapterRequest,
            out: &mut Vec<u8>,
        ) -> Result<bmux_plugin::AttachVisualAdapterOutput, String> {
            TEST_VISUAL_ADAPTER_CALLS.fetch_add(1, Ordering::SeqCst);
            out.extend_from_slice(b"same-payload");
            Ok(bmux_plugin::AttachVisualAdapterOutput {
                encoding: "test".to_string(),
                payload: std::mem::take(out),
            })
        }
    }

    struct TestVisualFrame {
        surface: TestVisualSurface,
    }

    impl bmux_plugin::AttachVisualFrameView for TestVisualFrame {
        fn surface_count(&self) -> usize {
            1
        }

        fn surface(&self, index: usize) -> Option<&dyn bmux_plugin::AttachVisualSurfaceView> {
            (index == 0).then_some(&self.surface as &dyn bmux_plugin::AttachVisualSurfaceView)
        }
    }

    struct TestVisualSurface {
        surface_id: Uuid,
        content_revision: u64,
    }

    impl bmux_plugin::AttachVisualSurfaceView for TestVisualSurface {
        fn surface_id(&self) -> Uuid {
            self.surface_id
        }

        fn pane_id(&self) -> Uuid {
            Uuid::from_u128(42)
        }

        fn rect(&self) -> ExtensionRect {
            ExtensionRect::new(0, 0, 2, 1)
        }

        fn content_rect(&self) -> ExtensionRect {
            ExtensionRect::new(0, 0, 2, 1)
        }

        fn focused(&self) -> bool {
            true
        }

        fn grid_revision(&self) -> u64 {
            self.content_revision.saturating_add(100)
        }

        fn content_revision(&self) -> u64 {
            self.content_revision
        }

        fn width(&self) -> u16 {
            2
        }

        fn height(&self) -> u16 {
            1
        }

        fn cell(&self, _x: u16, _y: u16) -> Option<bmux_plugin::AttachVisualCellRef<'_>> {
            None
        }
    }

    fn test_visual_adapters() -> Arc<bmux_plugin::VisualAdapterRegistry> {
        let adapters = Arc::new(bmux_plugin::VisualAdapterRegistry::new());
        adapters.register(Arc::new(TestVisualAdapter));
        TEST_VISUAL_ADAPTER_CALLS.store(0, Ordering::SeqCst);
        adapters
    }

    fn visual_request() -> VisualAdapterRequest {
        VisualAdapterRequest {
            id: "test.request".to_string(),
            adapter: "test.visual.constant-output".to_string(),
            owner_plugin_id: "test.owner".to_string(),
            event_kind: "test.visual".to_string(),
            scope: "focused-pane".to_string(),
            area: "content".to_string(),
            max_hz: 0,
            dirty_only: true,
            max_bytes: 1024,
            settings: BTreeMap::new(),
        }
    }

    fn extension_with_visual_request() -> (
        DecorationRenderExtension,
        Arc<Mutex<DecorationRendererCache>>,
    ) {
        let scene = DecorationScene {
            revision: 1,
            surfaces: BTreeMap::new(),
            animation: None,
            input_hooks: Vec::new(),
            visual_adapters: vec![visual_request()],
        };
        let (_tx, rx) = tokio::sync::watch::channel(Arc::new(scene));
        let cache = Arc::new(Mutex::new(DecorationRendererCache {
            visual_adapters: test_visual_adapters(),
            ..DecorationRendererCache::default()
        }));
        cache.lock().expect("cache lock").set_scene_receiver(rx);
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache: cache.clone(),
        };
        (extension, cache)
    }

    #[test]
    fn empty_content_scene_is_explicit_retained_noop_for_both_layers() {
        let (extension, _) = extension_with_visual_request();
        for layer in [
            RenderExtensionLayer::BeforePaneContent,
            RenderExtensionLayer::AfterPaneContent,
        ] {
            let scene = extension
                .render_layer_scene_with_context(
                    Uuid::from_u128(42),
                    &ExtensionRect::new(2, 3, 20, 8),
                    layer,
                    &RenderExtensionContext::default(),
                )
                .expect("missing decoration must answer with an empty retained scene");
            assert_eq!(scene.items, [] as [bmux_plugin::RenderSceneItem; 0]);
        }
    }

    #[test]
    fn empty_decoration_publishes_retained_revision() {
        let (extension, cache) = extension_with_visual_request();
        let id = Uuid::from_u128(42);
        cache
            .lock()
            .expect("cache lock")
            .surfaces
            .insert(id, surface(id, Vec::new()));
        for layer in [
            RenderExtensionLayer::BeforePaneContent,
            RenderExtensionLayer::AfterPaneContent,
        ] {
            let scene = extension
                .render_layer_scene_with_context(
                    id,
                    &ExtensionRect::new(2, 3, 20, 8),
                    layer,
                    &RenderExtensionContext::default(),
                )
                .expect("empty decoration must remain a supported retained representation");
            assert_eq!(scene.items, [] as [bmux_plugin::RenderSceneItem; 0]);
            assert!(scene.revision.is_some());
        }
    }

    #[test]
    fn overlay_role_uses_plugin_owned_fallback_border() {
        let (extension, _) = extension_with_visual_request();
        let rect = ExtensionRect::new(2, 3, 20, 8);

        let scene = extension
            .render_layer_scene_with_context(
                Uuid::from_u128(42),
                &rect,
                RenderExtensionLayer::AfterPaneContent,
                &RenderExtensionContext {
                    surface_role: bmux_plugin::RenderSurfaceRole::Overlay,
                    ..RenderExtensionContext::default()
                },
            )
            .expect("overlay scene");

        assert!(matches!(
            &scene.items[0].item,
            bmux_plugin::RenderLayerItem::Op(bmux_plugin::RenderOp::Border { rect: actual, glyphs, .. })
                if *actual == rect && *glyphs == bmux_plugin::BorderGlyphs::rounded()
        ));
    }

    fn kitty_capabilities() -> TerminalRenderCapabilities {
        TerminalRenderCapabilities {
            kitty_graphics: true,
            graphics_alpha: true,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            ..TerminalRenderCapabilities::default()
        }
    }

    fn scene_style() -> SceneStyle {
        SceneStyle {
            fg: None,
            bg: None,
            bold: false,
            underline: false,
            italic: false,
            reverse: false,
            dim: false,
            blink: false,
            strikethrough: false,
        }
    }

    fn surface(surface_id: Uuid, paint_commands: Vec<PaintCommand>) -> SurfaceDecoration {
        SurfaceDecoration {
            surface_id,
            rect: SceneRect {
                x: 0,
                y: 0,
                w: 20,
                h: 10,
            },
            content_rect: SceneRect {
                x: 1,
                y: 1,
                w: 18,
                h: 8,
            },
            paint_commands,
            before_content_paint_commands: Vec::new(),
            interactive_regions: Vec::new(),
        }
    }

    #[test]
    fn content_rect_override_preserves_asymmetric_authoritative_geometry() {
        let surface_id = Uuid::from_u128(105);
        let mut decoration = surface(surface_id, Vec::new());
        decoration.content_rect = SceneRect {
            x: 4,
            y: 2,
            w: 11,
            h: 6,
        };
        let cache = Arc::new(Mutex::new(DecorationRendererCache {
            revision: 1,
            surfaces: BTreeMap::from([(surface_id, decoration)]),
            layer_revisions: BTreeMap::new(),
            scene_rx: None,
            visual_last_at: BTreeMap::new(),
            visual_last_revision: BTreeMap::new(),
            visual_last_payload_hash: BTreeMap::new(),
            visual_adapter_cache: BTreeMap::new(),
            visual_stats: BTreeMap::new(),
            visual_adapters: Arc::default(),
        }));
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache,
        };

        assert_eq!(
            extension.content_rect_override(surface_id),
            Some(ExtensionRect::new(4, 2, 11, 6))
        );
    }

    #[test]
    fn surface_layer_revision_changes_only_for_changed_layer() {
        let surface_id = Uuid::from_u128(101);
        let mut decoration = surface(
            surface_id,
            vec![PaintCommand::Text {
                col: 1,
                row: 1,
                z: 0,
                text: "after".to_string(),
                style: scene_style(),
            }],
        );
        decoration.before_content_paint_commands = vec![PaintCommand::Text {
            col: 1,
            row: 0,
            z: 0,
            text: "before".to_string(),
            style: scene_style(),
        }];
        let before_revision =
            surface_layer_revision(&decoration, RenderExtensionLayer::BeforePaneContent);
        let after_revision =
            surface_layer_revision(&decoration, RenderExtensionLayer::AfterPaneContent);

        decoration.paint_commands = vec![PaintCommand::Text {
            col: 1,
            row: 1,
            z: 0,
            text: "after changed".to_string(),
            style: scene_style(),
        }];

        assert_eq!(
            before_revision,
            surface_layer_revision(&decoration, RenderExtensionLayer::BeforePaneContent)
        );
        assert_ne!(
            after_revision,
            surface_layer_revision(&decoration, RenderExtensionLayer::AfterPaneContent)
        );
    }

    fn extension_with_surface(
        surface_id: Uuid,
        paint_commands: Vec<PaintCommand>,
    ) -> (
        DecorationRenderExtension,
        Arc<Mutex<DecorationRendererCache>>,
    ) {
        let cache = Arc::new(Mutex::new(DecorationRendererCache {
            revision: 7,
            surfaces: BTreeMap::from([(surface_id, surface(surface_id, paint_commands))]),
            layer_revisions: BTreeMap::new(),
            scene_rx: None,
            visual_last_at: BTreeMap::new(),
            visual_last_revision: BTreeMap::new(),
            visual_last_payload_hash: BTreeMap::new(),
            visual_adapter_cache: BTreeMap::new(),
            visual_stats: BTreeMap::new(),
            visual_adapters: Arc::default(),
        }));
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache: cache.clone(),
        };
        (extension, cache)
    }

    #[test]
    fn retained_scene_converts_text_commands() {
        let surface_id = Uuid::from_u128(201);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::Text {
                col: 2,
                row: 3,
                z: 4,
                text: "perf".to_string(),
                style: scene_style(),
            }],
        );
        let scene = extension
            .render_layer_scene_with_context(
                surface_id,
                &ExtensionRect::new(0, 0, 20, 10),
                RenderExtensionLayer::AfterPaneContent,
                &RenderExtensionContext::default(),
            )
            .expect("retained scene should be available");

        assert_eq!(scene.items.len(), 1);
        assert!(matches!(
            &scene.items[0].item,
            bmux_plugin::RenderLayerItem::Op(bmux_plugin::RenderOp::TextRun { x: 2, y: 3, text, .. }) if text == "perf"
        ));
    }

    #[test]
    fn retained_scene_converts_before_content_to_underlay_items() {
        let surface_id = Uuid::from_u128(202);
        let (extension, cache) = extension_with_surface(surface_id, Vec::new());
        cache
            .lock()
            .expect("cache lock")
            .surfaces
            .get_mut(&surface_id)
            .expect("surface")
            .before_content_paint_commands = vec![PaintCommand::Text {
            col: 1,
            row: 0,
            z: -1,
            text: "under".to_string(),
            style: scene_style(),
        }];
        let scene = extension
            .render_layer_scene_with_context(
                surface_id,
                &ExtensionRect::new(0, 0, 20, 10),
                RenderExtensionLayer::BeforePaneContent,
                &RenderExtensionContext::default(),
            )
            .expect("retained scene should be available");

        assert!(matches!(
            &scene.items[0].item,
            bmux_plugin::RenderLayerItem::Op(bmux_plugin::RenderOp::TextRun { x: 1, y: 0, text, .. }) if text == "under"
        ));
    }

    #[test]
    fn retained_scene_converts_semantic_border_to_graphic_items_when_capable() {
        let surface_id = Uuid::from_u128(203);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::SemanticBorder {
                rect: SceneRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                z: 7,
                style: scene_style(),
                fallback_glyphs: SceneBorderGlyphs::Rounded,
                thickness_px: 2,
                radius_px: 0,
                when: None,
            }],
        );
        let scene = extension
            .render_layer_scene_with_context(
                surface_id,
                &ExtensionRect::new(0, 0, 20, 10),
                RenderExtensionLayer::AfterPaneContent,
                &RenderExtensionContext {
                    capabilities: kitty_capabilities(),
                    ..RenderExtensionContext::default()
                },
            )
            .expect("retained scene should be available");

        assert_eq!(scene.items.len(), 4);
        assert!(
            scene
                .items
                .iter()
                .all(|item| matches!(item.item, bmux_plugin::RenderLayerItem::Graphic(_)))
        );
    }

    #[test]
    fn retained_scene_keeps_supported_items_when_one_command_cannot_lower() {
        let surface_id = Uuid::from_u128(205);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![
                PaintCommand::FilledRect {
                    rect: SceneRect {
                        x: 1,
                        y: 1,
                        w: 2,
                        h: 1,
                    },
                    z: 0,
                    glyph: "表".to_string(),
                    style: scene_style(),
                },
                PaintCommand::SemanticBorder {
                    rect: SceneRect {
                        x: 0,
                        y: 0,
                        w: 20,
                        h: 10,
                    },
                    z: 1,
                    style: scene_style(),
                    fallback_glyphs: SceneBorderGlyphs::Rounded,
                    thickness_px: 2,
                    radius_px: 0,
                    when: None,
                },
            ],
        );
        let scene = extension
            .render_layer_scene_with_context(
                surface_id,
                &ExtensionRect::new(0, 0, 20, 10),
                RenderExtensionLayer::AfterPaneContent,
                &RenderExtensionContext {
                    capabilities: kitty_capabilities(),
                    ..RenderExtensionContext::default()
                },
            )
            .expect("retained scene should be available");

        assert_eq!(scene.items.len(), 4);
        assert!(
            scene
                .items
                .iter()
                .all(|item| matches!(item.item, bmux_plugin::RenderLayerItem::Graphic(_)))
        );
    }

    #[test]
    fn retained_scene_uses_text_fallback_for_semantic_border_without_graphics() {
        let surface_id = Uuid::from_u128(204);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::SemanticBorder {
                rect: SceneRect {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 3,
                },
                z: 0,
                style: scene_style(),
                fallback_glyphs: SceneBorderGlyphs::Ascii,
                thickness_px: 2,
                radius_px: 0,
                when: None,
            }],
        );
        let scene = extension
            .render_layer_scene_with_context(
                surface_id,
                &ExtensionRect::new(0, 0, 4, 3),
                RenderExtensionLayer::AfterPaneContent,
                &RenderExtensionContext::default(),
            )
            .expect("retained scene should be available");

        assert!(scene.items.iter().any(|item| matches!(
            item.item,
            bmux_plugin::RenderLayerItem::Op(bmux_plugin::RenderOp::Border { .. })
        )));
    }

    #[test]
    fn renderer_projects_only_with_its_own_runtimes_adapters() {
        // Same scene, two runtimes: only the one whose registry holds the
        // adapter projects; nothing is looked up process-wide.
        let (with_adapter, _) = extension_with_visual_request();
        let (without_adapter, cache) = extension_with_visual_request();
        cache.lock().expect("cache lock").visual_adapters = Arc::default();
        let frame = TestVisualFrame {
            surface: TestVisualSurface {
                surface_id: Uuid::from_u128(9),
                content_revision: 1,
            },
        };
        let mut updates = Vec::new();
        without_adapter.observe_visual_frame(&frame, &mut updates);
        assert_eq!(updates.len(), 0);
        with_adapter.observe_visual_frame(&frame, &mut updates);
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn visual_adapter_dirty_only_uses_content_revision_and_suppresses_unchanged_payload() {
        let (extension, cache) = extension_with_visual_request();
        let surface_id = Uuid::from_u128(7);
        let mut updates = Vec::new();

        extension.observe_visual_frame(
            &TestVisualFrame {
                surface: TestVisualSurface {
                    surface_id,
                    content_revision: 1,
                },
            },
            &mut updates,
        );
        assert_eq!(TEST_VISUAL_ADAPTER_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(updates.len(), 1);

        extension.observe_visual_frame(
            &TestVisualFrame {
                surface: TestVisualSurface {
                    surface_id,
                    content_revision: 1,
                },
            },
            &mut updates,
        );
        assert_eq!(TEST_VISUAL_ADAPTER_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(updates.len(), 1);

        extension.observe_visual_frame(
            &TestVisualFrame {
                surface: TestVisualSurface {
                    surface_id,
                    content_revision: 2,
                },
            },
            &mut updates,
        );
        assert_eq!(TEST_VISUAL_ADAPTER_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(updates.len(), 1);

        let guard = cache.lock().expect("cache lock");
        let stats = guard
            .visual_stats
            .values()
            .next()
            .expect("visual stats recorded");
        assert_eq!(stats.projections, 2);
        assert_eq!(stats.updated, 2);
        assert_eq!(stats.sent_updates, 1);
        assert_eq!(stats.duplicate_suppressed, 1);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn scoped_renderers_retain_independent_scenes_and_release_receivers() {
        let first_bus = bmux_plugin::EventBus::new();
        let second_bus = bmux_plugin::EventBus::new();
        let first = create_renderer(&first_bus, Arc::default());
        let second = create_renderer(&second_bus, Arc::default());
        let scene = |revision| DecorationScene {
            revision,
            surfaces: BTreeMap::new(),
            animation: None,
            input_hooks: Vec::new(),
            visual_adapters: Vec::new(),
        };
        first_bus
            .publish_state(&SCENE_STATE_KIND, scene(7))
            .unwrap();
        second_bus
            .publish_state(&SCENE_STATE_KIND, scene(2))
            .unwrap();
        first.refresh_state();
        second.refresh_state();
        assert_eq!(first.cache.lock().unwrap().revision, 7);
        assert_eq!(second.cache.lock().unwrap().revision, 2);
        let released = Arc::downgrade(&first.cache);
        drop(first);
        assert!(released.upgrade().is_none());
        second_bus
            .publish_state(&SCENE_STATE_KIND, scene(3))
            .unwrap();
        second.refresh_state();
        assert_eq!(second.cache.lock().unwrap().revision, 3);
        let restarted = create_renderer(&first_bus, Arc::default());
        assert_eq!(restarted.cache.lock().unwrap().revision, 7);
    }

    #[test]
    fn refresh_state_drains_retained_scene_before_render_queries() {
        let surface_id = Uuid::from_u128(9001);
        let initial = DecorationScene {
            revision: 1,
            surfaces: BTreeMap::new(),
            animation: None,
            input_hooks: Vec::new(),
            visual_adapters: Vec::new(),
        };
        let updated = DecorationScene {
            revision: 2,
            surfaces: BTreeMap::from([(
                surface_id,
                surface(
                    surface_id,
                    vec![PaintCommand::Text {
                        col: 1,
                        row: 1,
                        z: 0,
                        text: "decorated".to_string(),
                        style: scene_style(),
                    }],
                ),
            )]),
            animation: None,
            input_hooks: Vec::new(),
            visual_adapters: Vec::new(),
        };
        let (tx, rx) = tokio::sync::watch::channel(Arc::new(initial));
        let cache = Arc::new(Mutex::new(DecorationRendererCache::default()));
        {
            let mut guard = cache.lock().expect("cache lock");
            guard.set_scene_receiver(rx);
        }
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache: cache.clone(),
        };

        tx.send(Arc::new(updated))
            .expect("watch receiver remains live");
        extension.refresh_state();

        {
            let guard = cache.lock().expect("cache lock");
            assert_eq!(guard.revision, 2);
            assert!(guard.surfaces.contains_key(&surface_id));
        }
        assert!(
            scene_ops(&extension, surface_id)
                .iter()
                .any(|op| matches!(op, RenderOp::TextRun { text, .. } if text == "decorated")),
            "the drained scene is what the next frame paints"
        );
    }

    #[test]
    fn scene_converts_supported_text_and_reports_its_layer_revision() {
        let surface_id = Uuid::from_u128(1);
        let paint_commands = vec![PaintCommand::Text {
            col: 2,
            row: 3,
            z: 0,
            text: "hello".to_string(),
            style: scene_style(),
        }];
        let expected_revision = surface_layer_revision(
            &surface(surface_id, paint_commands.clone()),
            RenderExtensionLayer::AfterPaneContent,
        );
        let (extension, _cache) = extension_with_surface(surface_id, paint_commands);

        assert_eq!(
            extension.render_layer_revision(surface_id, RenderExtensionLayer::AfterPaneContent),
            Some(expected_revision)
        );
        let ops = scene_ops(&extension, surface_id);

        assert_eq!(
            ops,
            vec![RenderOp::TextRun {
                x: 2,
                y: 3,
                text: "hello".to_string(),
                style: RenderStyle::default(),
            }]
        );
    }

    #[test]
    fn scene_paints_space_fills_as_blank_cells() {
        let surface_id = Uuid::from_u128(6);
        let rect = SceneRect {
            x: 1,
            y: 2,
            w: 3,
            h: 4,
        };
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::FilledRect {
                rect: rect.clone(),
                z: 0,
                glyph: " ".to_string(),
                style: scene_style(),
            }],
        );

        let ops = scene_ops(&extension, surface_id);

        // A space fill is a clear: the host paints it as blank cells in the style.
        assert_eq!(
            ops,
            vec![RenderOp::ClearRect {
                rect: ExtensionRect {
                    x: 1,
                    y: 2,
                    w: 3,
                    h: 4,
                },
                style: RenderStyle::default(),
            }]
        );
    }

    #[test]
    fn scene_converts_empty_cell_grid_cells_to_sparse_cells() {
        let surface_id = Uuid::from_u128(11);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::CellGrid {
                origin_col: 0,
                origin_row: 0,
                z: 0,
                cols: 2,
                cells: vec![
                    SceneCell {
                        glyph: "A".to_string(),
                        style: scene_style(),
                    },
                    SceneCell {
                        glyph: String::new(),
                        style: scene_style(),
                    },
                ],
            }],
        );

        let ops = scene_ops(&extension, surface_id);

        assert!(matches!(
            &ops[..],
            [RenderOp::CellGrid { rows, .. }]
                if rows == &vec![vec![
                    RenderCell { ch: Some('A'), style: RenderStyle::default() },
                    RenderCell { ch: None, style: RenderStyle::default() },
                ]]
        ));
    }

    #[test]
    fn scene_filters_semantic_border_by_capability_context() {
        let surface_id = Uuid::from_u128(17);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::SemanticBorder {
                rect: SceneRect {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 3,
                },
                z: 0,
                style: scene_style(),
                fallback_glyphs: SceneBorderGlyphs::Thick,
                thickness_px: 3,
                radius_px: 2,
                when: Some(TerminalCapabilityQuery {
                    all: vec![TerminalCapability::GraphicsKitty],
                    any: Vec::new(),
                    none: Vec::new(),
                }),
            }],
        );

        let no_graphics = ops_of(scene_items(
            &extension,
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            &RenderExtensionContext::default(),
        ));
        assert_eq!(no_graphics, [] as [bmux_plugin::RenderOp; 0]);

        let kitty_capabilities = TerminalRenderCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            ..TerminalRenderCapabilities::default()
        };
        let kitty = ops_of(scene_items(
            &extension,
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            &RenderExtensionContext {
                capabilities: kitty_capabilities,
                ..RenderExtensionContext::default()
            },
        ));
        assert!(matches!(
            &kitty[..],
            [RenderOp::Border { glyphs, .. }]
                if glyphs.top_left == '┏' && glyphs.horizontal == '━' && glyphs.vertical == '┃'
        ));

        let graphics = scene_items(
            &extension,
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            &RenderExtensionContext {
                capabilities: TerminalRenderCapabilities {
                    graphics_alpha: true,
                    ..kitty_capabilities
                },
                ..RenderExtensionContext::default()
            },
        );
        assert!(
            graphics
                .iter()
                .any(|item| matches!(item, RenderLayerItem::Graphic(_)))
        );
    }

    #[test]
    fn lower_text_skips_cells_covered_by_higher_text() {
        let ops = render_ops_for_paint_commands_with_capabilities(
            &[
                PaintCommand::Text {
                    col: 3,
                    row: 0,
                    z: 5,
                    text: "◆".to_string(),
                    style: scene_style(),
                },
                PaintCommand::Text {
                    col: 2,
                    row: 0,
                    z: 10,
                    text: "HEADER".to_string(),
                    style: scene_style(),
                },
            ],
            SceneRenderCapabilities::default(),
        )
        .expect("text commands should lower to render ops");

        assert!(ops.iter().any(|op| {
            matches!(op, RenderOp::TextRun { x: 2, y: 0, text, .. } if text == "HEADER")
        }));
        assert!(!ops.iter().any(|op| {
            matches!(op, RenderOp::TextRun { x: 3, y: 0, text, .. } if text == "◆")
        }));
    }

    #[test]
    fn text_border_fallback_skips_cells_covered_by_higher_text() {
        let ops = render_ops_for_paint_commands_with_capabilities(
            &[
                PaintCommand::SemanticBorder {
                    rect: SceneRect {
                        x: 0,
                        y: 0,
                        w: 20,
                        h: 5,
                    },
                    z: 0,
                    style: scene_style(),
                    fallback_glyphs: SceneBorderGlyphs::Rounded,
                    thickness_px: 3,
                    radius_px: 0,
                    when: None,
                },
                PaintCommand::Text {
                    col: 2,
                    row: 0,
                    z: 10,
                    text: "HEADER".to_string(),
                    style: scene_style(),
                },
            ],
            SceneRenderCapabilities::default(),
        )
        .expect("semantic border should fall back to text ops");

        assert!(ops.iter().any(|op| {
            matches!(op, RenderOp::TextRun { x: 2, y: 0, text, .. } if text == "HEADER")
        }));
        assert!(!ops.iter().any(|op| match op {
            RenderOp::Border { .. } => true,
            RenderOp::TextRun { x, y: 0, text, .. } => {
                let end = x.saturating_add(u16::try_from(text.chars().count()).unwrap_or(u16::MAX));
                *x < 8 && end > 2 && text != "HEADER"
            }
            _ => false,
        }));
    }

    #[test]
    fn graphics_semantic_border_suppresses_static_text_border() {
        let surface_id = Uuid::from_u128(107);
        let mut decoration = surface(
            surface_id,
            vec![
                PaintCommand::BoxBorder {
                    rect: SceneRect {
                        x: 0,
                        y: 0,
                        w: 20,
                        h: 8,
                    },
                    z: 0,
                    glyphs: SceneBorderGlyphs::SingleLine,
                    style: scene_style(),
                },
                PaintCommand::Text {
                    col: 2,
                    row: 0,
                    z: 10,
                    text: "HEADER".to_string(),
                    style: scene_style(),
                },
            ],
        );
        decoration.before_content_paint_commands = vec![PaintCommand::SemanticBorder {
            rect: SceneRect {
                x: 0,
                y: 0,
                w: 20,
                h: 8,
            },
            z: 10,
            style: scene_style(),
            fallback_glyphs: SceneBorderGlyphs::SingleLine,
            thickness_px: 3,
            radius_px: 0,
            when: None,
        }];
        let cache = Arc::new(Mutex::new(DecorationRendererCache {
            revision: 7,
            surfaces: BTreeMap::from([(surface_id, decoration)]),
            layer_revisions: BTreeMap::new(),
            scene_rx: None,
            visual_last_at: BTreeMap::new(),
            visual_last_revision: BTreeMap::new(),
            visual_last_payload_hash: BTreeMap::new(),
            visual_adapter_cache: BTreeMap::new(),
            visual_stats: BTreeMap::new(),
            visual_adapters: Arc::default(),
        }));
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache,
        };

        let ops = ops_of(scene_items(
            &extension,
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            &kitty_alpha_context(),
        ));

        assert!(
            ops.iter()
                .any(|op| { matches!(op, RenderOp::TextRun { text, .. } if text == "HEADER") })
        );
        assert!(ops.iter().all(|op| !matches!(op, RenderOp::Border { .. })));
    }

    #[test]
    fn before_content_semantic_border_uses_graphics_items() {
        let surface_id = Uuid::from_u128(106);
        let mut decoration = surface(surface_id, Vec::new());
        decoration.before_content_paint_commands = vec![PaintCommand::SemanticBorder {
            rect: SceneRect {
                x: 0,
                y: 0,
                w: 20,
                h: 8,
            },
            z: 10,
            style: scene_style(),
            fallback_glyphs: SceneBorderGlyphs::SingleLine,
            thickness_px: 3,
            radius_px: 0,
            when: None,
        }];
        let cache = Arc::new(Mutex::new(DecorationRendererCache {
            revision: 7,
            surfaces: BTreeMap::from([(surface_id, decoration)]),
            layer_revisions: BTreeMap::new(),
            scene_rx: None,
            visual_last_at: BTreeMap::new(),
            visual_last_revision: BTreeMap::new(),
            visual_last_payload_hash: BTreeMap::new(),
            visual_adapter_cache: BTreeMap::new(),
            visual_stats: BTreeMap::new(),
            visual_adapters: Arc::default(),
        }));
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache,
        };

        let items = scene_items(
            &extension,
            surface_id,
            RenderExtensionLayer::BeforePaneContent,
            &kitty_alpha_context(),
        );

        assert!(
            items
                .iter()
                .any(|item| matches!(item, RenderLayerItem::Graphic(_)))
        );
    }

    #[test]
    fn semantic_border_graphics_use_under_text_placements_for_later_text_decorations() {
        let surface_id = Uuid::from_u128(104);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![
                PaintCommand::SemanticBorder {
                    rect: SceneRect {
                        x: 0,
                        y: 0,
                        w: 20,
                        h: 5,
                    },
                    z: 0,
                    style: scene_style(),
                    fallback_glyphs: SceneBorderGlyphs::Rounded,
                    thickness_px: 3,
                    radius_px: 0,
                    when: None,
                },
                PaintCommand::Text {
                    col: 2,
                    row: 0,
                    z: 10,
                    text: "HEADER".to_string(),
                    style: scene_style(),
                },
            ],
        );

        let items = scene_items(
            &extension,
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            &kitty_alpha_context(),
        );
        let header_rect = ExtensionRect::new(2, 0, 6, 1);

        assert!(items.iter().any(|item| {
            matches!(item, RenderLayerItem::Op(RenderOp::TextRun { text, .. }) if text == "HEADER")
        }));
        assert!(items.iter().all(|item| match item {
            RenderLayerItem::Graphic(graphic) => graphic.z_index == -1,
            RenderLayerItem::Op(_) => true,
        }));
        assert!(items.iter().any(|item| match item {
            RenderLayerItem::Graphic(graphic) => graphic.cell_rect.intersects(header_rect),
            RenderLayerItem::Op(_) => false,
        }));
    }

    #[test]
    fn semantic_border_graphics_use_stable_under_text_placements_for_pong_edge_decorations() {
        let surface_id = Uuid::from_u128(105);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![
                PaintCommand::SemanticBorder {
                    rect: SceneRect {
                        x: 0,
                        y: 0,
                        w: 20,
                        h: 8,
                    },
                    z: 0,
                    style: scene_style(),
                    fallback_glyphs: SceneBorderGlyphs::Rounded,
                    thickness_px: 3,
                    radius_px: 0,
                    when: None,
                },
                PaintCommand::Text {
                    col: 8,
                    row: 0,
                    z: 30,
                    text: "1 : 0".to_string(),
                    style: scene_style(),
                },
                PaintCommand::Text {
                    col: 0,
                    row: 2,
                    z: 20,
                    text: "▌".to_string(),
                    style: scene_style(),
                },
                PaintCommand::Text {
                    col: 19,
                    row: 3,
                    z: 20,
                    text: "▐".to_string(),
                    style: scene_style(),
                },
            ],
        );

        let items = scene_items(
            &extension,
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            &kitty_alpha_context(),
        );
        assert!(items.iter().any(|item| {
            matches!(item, RenderLayerItem::Op(RenderOp::TextRun { text, .. }) if text == "1 : 0")
        }));
        assert!(items.iter().any(|item| {
            matches!(item, RenderLayerItem::Op(RenderOp::TextRun { text, .. }) if text == "▌")
        }));
        assert!(items.iter().any(|item| {
            matches!(item, RenderLayerItem::Op(RenderOp::TextRun { text, .. }) if text == "▐")
        }));
        let graphics = items
            .iter()
            .filter_map(|item| match item {
                RenderLayerItem::Graphic(graphic) => {
                    assert_eq!(graphic.z_index, -1);
                    Some(graphic.cell_rect)
                }
                RenderLayerItem::Op(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            graphics,
            vec![
                ExtensionRect::new(0, 0, 20, 1),
                ExtensionRect::new(0, 7, 20, 1),
                ExtensionRect::new(0, 0, 1, 8),
                ExtensionRect::new(19, 0, 1, 8),
            ]
        );
    }

    fn terminal_graphic_signature(
        scene: &RenderLayerScene,
    ) -> Vec<(RenderSceneItemKey, ExtensionRect)> {
        scene
            .items
            .iter()
            .filter_map(|item| match &item.item {
                bmux_plugin::RenderLayerItem::Graphic(graphic) => {
                    Some((item.key.clone(), graphic.cell_rect))
                }
                bmux_plugin::RenderLayerItem::Op(_) => None,
            })
            .collect()
    }

    fn semantic_border_command(rect: &SceneRect) -> PaintCommand {
        PaintCommand::SemanticBorder {
            rect: rect.clone(),
            z: 0,
            style: scene_style(),
            fallback_glyphs: SceneBorderGlyphs::Rounded,
            thickness_px: 3,
            radius_px: 0,
            when: None,
        }
    }

    fn snake_commands(columns: impl IntoIterator<Item = u16>) -> Vec<PaintCommand> {
        columns
            .into_iter()
            .map(|col| PaintCommand::Text {
                col,
                row: 0,
                z: 20,
                text: "◆".to_string(),
                style: scene_style(),
            })
            .collect()
    }

    #[test]
    fn moving_foreground_only_snake_keeps_stable_kitty_border_graphics() {
        let surface_id = Uuid::from_u128(106);
        let rect = SceneRect {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        };
        let mut first_commands = vec![semantic_border_command(&rect)];
        first_commands.extend(snake_commands(1..19));
        let first = render_scene_for_surface_layer_with_capabilities(
            surface_id,
            &surface(surface_id, first_commands),
            RenderExtensionLayer::AfterPaneContent,
            kitty_capabilities(),
            SceneRenderCapabilities::default(),
            &[],
        );

        let mut moved_commands = vec![semantic_border_command(&rect)];
        moved_commands.extend(snake_commands((1..19).rev()));
        let moved = render_scene_for_surface_layer_with_capabilities(
            surface_id,
            &surface(surface_id, moved_commands),
            RenderExtensionLayer::AfterPaneContent,
            kitty_capabilities(),
            SceneRenderCapabilities::default(),
            &[],
        );

        let expected = vec![
            ExtensionRect::new(0, 0, 20, 1),
            ExtensionRect::new(0, 9, 20, 1),
            ExtensionRect::new(0, 0, 1, 10),
            ExtensionRect::new(19, 0, 1, 10),
        ];
        assert_eq!(
            terminal_graphic_signature(&first)
                .iter()
                .map(|(_, rect)| *rect)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            terminal_graphic_signature(&first),
            terminal_graphic_signature(&moved)
        );
    }

    #[test]
    fn opaque_text_and_global_overlay_still_split_kitty_border_graphics() {
        let surface_id = Uuid::from_u128(107);
        let rect = SceneRect {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        };
        let mut opaque_style = scene_style();
        opaque_style.bg = Some(SceneColor::Rgb { r: 1, g: 2, b: 3 });
        let local_opaque = surface(
            surface_id,
            vec![
                semantic_border_command(&rect),
                PaintCommand::Text {
                    col: 5,
                    row: 0,
                    z: 20,
                    text: "opaque".to_string(),
                    style: opaque_style,
                },
            ],
        );
        let local_scene = render_scene_for_surface_layer_with_capabilities(
            surface_id,
            &local_opaque,
            RenderExtensionLayer::AfterPaneContent,
            kitty_capabilities(),
            SceneRenderCapabilities::default(),
            &[],
        );
        let local_graphics = terminal_graphic_signature(&local_scene);
        assert!(local_graphics.len() > 4);
        assert!(
            local_graphics.iter().all(|(_, graphic_rect)| {
                !graphic_rect.intersects(ExtensionRect::new(5, 0, 6, 1))
            })
        );

        let mut snake = vec![semantic_border_command(&rect)];
        snake.extend(snake_commands(1..19));
        let global_occluder = ExtensionRect::new(7, 0, 6, 4);
        let global_scene = render_scene_for_surface_layer_with_capabilities(
            surface_id,
            &surface(surface_id, snake),
            RenderExtensionLayer::AfterPaneContent,
            kitty_capabilities(),
            SceneRenderCapabilities::default(),
            &[global_occluder],
        );
        let global_graphics = terminal_graphic_signature(&global_scene);
        assert!(global_graphics.len() > 4);
        assert!(
            global_graphics
                .iter()
                .all(|(_, graphic_rect)| !graphic_rect.intersects(global_occluder))
        );
        let snake_columns = global_scene
            .items
            .iter()
            .filter_map(|item| match &item.item {
                bmux_plugin::RenderLayerItem::Op(bmux_plugin::RenderOp::TextRun {
                    x,
                    y,
                    text,
                    ..
                }) if text == "◆" && *y == 0 => Some(*x),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(snake_columns.iter().all(|column| !(7..13).contains(column)));
        assert!(snake_columns.iter().any(|column| *column < 7));
        assert!(snake_columns.iter().any(|column| *column >= 13));
    }

    #[test]
    fn global_overlay_clips_ascii_border_fallback_and_snake_text() {
        let surface_id = Uuid::from_u128(108);
        let rect = SceneRect {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        };
        let mut commands = vec![semantic_border_command(&rect)];
        commands.extend(snake_commands(1..19));
        let occluder = ExtensionRect::new(7, 0, 6, 4);
        let scene = render_scene_for_surface_layer_with_capabilities(
            surface_id,
            &surface(surface_id, commands),
            RenderExtensionLayer::AfterPaneContent,
            TerminalRenderCapabilities::default(),
            SceneRenderCapabilities::default(),
            &[occluder],
        );

        assert!(scene.items.iter().all(|item| match &item.item {
            bmux_plugin::RenderLayerItem::Op(bmux_plugin::RenderOp::TextRun {
                x, y, text, ..
            }) => {
                !ExtensionRect::new(*x, *y, render_text_width_u16(text), 1).intersects(occluder)
            }
            bmux_plugin::RenderLayerItem::Op(bmux_plugin::RenderOp::StyledText { x, y, spans }) => {
                let width = spans
                    .iter()
                    .map(|span| render_text_width_u16(&span.text))
                    .fold(0, u16::saturating_add);
                !ExtensionRect::new(*x, *y, width, 1).intersects(occluder)
            }
            _ => true,
        }));
        assert!(scene.items.iter().any(|item| matches!(
            &item.item,
            bmux_plugin::RenderLayerItem::Op(bmux_plugin::RenderOp::TextRun { text, .. }) if text == "◆"
        )));
    }

    /// What the decoration paints on `layer` of `surface_id` (a 20x10 surface) with `context`:
    /// its scene's items lowered as the attach host lowers them, in paint order (z, then key).
    fn scene_items(
        extension: &DecorationRenderExtension,
        surface_id: Uuid,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Vec<RenderLayerItem> {
        let scene = extension
            .render_layer_scene_with_context(
                surface_id,
                &ExtensionRect::new(0, 0, 20, 10),
                layer,
                context,
            )
            .expect("the decoration answers every surface with a scene");
        let mut items = scene.items.iter().collect::<Vec<_>>();
        items.sort_by(|lhs, rhs| (lhs.z, &lhs.key).cmp(&(rhs.z, &rhs.key)));
        items.into_iter().map(|item| item.item.clone()).collect()
    }

    /// The cell operations among `items`.
    fn ops_of(items: Vec<RenderLayerItem>) -> Vec<RenderOp> {
        items
            .into_iter()
            .filter_map(|item| match item {
                RenderLayerItem::Op(op) => Some(op),
                RenderLayerItem::Graphic(_) => None,
            })
            .collect()
    }

    /// The cell operations of [`scene_items`] on the after-content layer, without terminal
    /// capabilities.
    fn scene_ops(extension: &DecorationRenderExtension, surface_id: Uuid) -> Vec<RenderOp> {
        ops_of(scene_items(
            extension,
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            &RenderExtensionContext::default(),
        ))
    }

    fn kitty_alpha_context() -> RenderExtensionContext {
        RenderExtensionContext {
            capabilities: TerminalRenderCapabilities {
                kitty_graphics: true,
                graphics_alpha: true,
                cell_pixel_width: 8,
                cell_pixel_height: 16,
                ..TerminalRenderCapabilities::default()
            },
            ..RenderExtensionContext::default()
        }
    }

    #[test]
    fn scene_omits_wide_cell_grid_glyphs() {
        let surface_id = Uuid::from_u128(7);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::CellGrid {
                origin_col: 0,
                origin_row: 0,
                z: 0,
                cols: 1,
                cells: vec![SceneCell {
                    glyph: "界".to_string(),
                    style: scene_style(),
                }],
            }],
        );

        assert_eq!(scene_ops(&extension, surface_id), [] as [RenderOp; 0]);
    }

    #[test]
    fn scene_omits_wide_border_glyphs() {
        let surface_id = Uuid::from_u128(8);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::BoxBorder {
                rect: SceneRect {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 3,
                },
                z: 0,
                glyphs: SceneBorderGlyphs::Custom {
                    top_left: "界".to_string(),
                    top_right: "+".to_string(),
                    bottom_left: "+".to_string(),
                    bottom_right: "+".to_string(),
                    horizontal: "-".to_string(),
                    vertical: "|".to_string(),
                },
                style: scene_style(),
            }],
        );

        assert_eq!(scene_ops(&extension, surface_id), [] as [RenderOp; 0]);
    }

    #[test]
    fn scene_preserves_named_color() {
        let surface_id = Uuid::from_u128(5);
        let mut style = scene_style();
        style.fg = Some(SceneColor::Named {
            name: NamedColor::BrightYellow,
        });
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::Text {
                col: 2,
                row: 3,
                z: 0,
                text: "hello".to_string(),
                style,
            }],
        );

        let ops = scene_ops(&extension, surface_id);

        assert!(matches!(
            &ops[..],
            [RenderOp::TextRun { style, .. }]
                if style.fg == Some(RenderColor::Named(RenderNamedColor::BrightYellow))
        ));
    }

    #[test]
    fn scene_text_paints_by_display_width() {
        let surface_id = Uuid::from_u128(4);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::Text {
                col: 2,
                row: 3,
                z: 0,
                text: "界".to_string(),
                style: scene_style(),
            }],
        );

        let ops = scene_ops(&extension, surface_id);
        assert_eq!(
            ops.iter()
                .flat_map(RenderOp::painted_rects)
                .collect::<Vec<_>>(),
            [ExtensionRect {
                x: 2,
                y: 3,
                w: 2,
                h: 1,
            }]
        );
    }

    #[test]
    fn scene_paints_by_z() {
        let surface_id = Uuid::from_u128(2);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![
                PaintCommand::Text {
                    col: 0,
                    row: 0,
                    z: 10,
                    text: "high".to_string(),
                    style: scene_style(),
                },
                PaintCommand::Text {
                    col: 0,
                    row: 1,
                    z: 0,
                    text: "low".to_string(),
                    style: scene_style(),
                },
            ],
        );

        let ops = scene_ops(&extension, surface_id);

        assert!(matches!(
            &ops[..],
            [RenderOp::TextRun { text: low, .. }, RenderOp::TextRun { text: high, .. }]
                if low == "low" && high == "high"
        ));
    }

    #[test]
    fn scene_lowers_horizontal_gradient_runs() {
        let surface_id = Uuid::from_u128(9);
        let mut from_style = scene_style();
        from_style.fg = Some(SceneColor::Rgb { r: 0, g: 0, b: 0 });
        let mut to_style = scene_style();
        to_style.fg = Some(SceneColor::Rgb { r: 255, g: 0, b: 0 });
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::GradientRun {
                col: 4,
                row: 2,
                z: 0,
                text: "abc".to_string(),
                axis: GradientAxis::Horizontal,
                from_style,
                to_style,
            }],
        );

        let ops = scene_ops(&extension, surface_id);

        assert!(matches!(
            &ops[..],
            [
                RenderOp::TextRun { x: 4, y: 2, text: a, style: a_style },
                RenderOp::TextRun { x: 5, y: 2, text: b, style: b_style },
                RenderOp::TextRun { x: 6, y: 2, text: c, style: c_style },
            ] if a == "a"
                && b == "b"
                && c == "c"
                && a_style.fg == Some(RenderColor::Rgb { r: 0, g: 0, b: 0 })
                && b_style.fg == Some(RenderColor::Rgb { r: 128, g: 0, b: 0 })
                && c_style.fg == Some(RenderColor::Rgb { r: 255, g: 0, b: 0 })
        ));
    }

    #[test]
    fn scene_lowers_vertical_gradient_runs() {
        let surface_id = Uuid::from_u128(10);
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::GradientRun {
                col: 4,
                row: 2,
                z: 0,
                text: "ab".to_string(),
                axis: GradientAxis::Vertical,
                from_style: scene_style(),
                to_style: scene_style(),
            }],
        );

        let ops = scene_ops(&extension, surface_id);

        assert!(matches!(
            &ops[..],
            [
                RenderOp::TextRun { x: 4, y: 2, text: a, .. },
                RenderOp::TextRun { x: 4, y: 3, text: b, .. },
            ] if a == "a" && b == "b"
        ));
    }

    #[test]
    fn scene_converts_full_style_flags() {
        let surface_id = Uuid::from_u128(3);
        let mut style = scene_style();
        style.underline = true;
        style.italic = true;
        style.reverse = true;
        style.dim = true;
        style.blink = true;
        style.strikethrough = true;
        let (extension, _cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::Text {
                col: 2,
                row: 3,
                z: 0,
                text: "hello".to_string(),
                style,
            }],
        );

        let ops = scene_ops(&extension, surface_id);

        assert!(matches!(
            &ops[..],
            [RenderOp::TextRun { style, .. }]
                if style.underline
                    && style.italic
                    && style.reverse
                    && style.dim
                    && style.blink
                    && style.strikethrough
        ));
    }
}
