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
//! 1. [`install`] registers an
//!    [`bmux_plugin::AttachRenderExtension`] and subscribes to the
//!    client-side [`bmux_plugin::global_event_bus`] retained
//!    `bmux.scene/scene-protocol` state.
//! 2. The retained scene state seeds the extension's cache immediately, and
//!    every subsequent scene replacement is drained by
//!    [`AttachRenderExtension::refresh_state`] before the next frame renders
//!    (revision-guarded so stale wire events can't downgrade).
//! 3. On every attach-render pass, the extension publishes retained scene items
//!    for the attach renderer to diff and emit. Legacy damage/render callbacks
//!    remain as fallback paths and keep their own rendered-surface cache.
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
use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bmux_plugin::{
    AttachInputEndpoint, AttachInputHook, AttachInputHookFilter, AttachRenderExtension,
    AttachVisualAdapterRequest, AttachVisualFrameView, AttachVisualProjectionResult,
    AttachVisualProjectionUpdate, BorderGlyphs as RenderBorderGlyphs, ExtensionRect, RenderCell,
    RenderColor, RenderDamage, RenderExtensionContext, RenderExtensionLayer, RenderLayerItem,
    RenderLayerScene, RenderNamedColor, RenderOp, RenderSceneItem, RenderSceneItemKey, RenderStyle,
    RenderUnderCell, TerminalRenderCapabilities, registered_visual_adapter,
    render_single_display_cell_char, render_text_width_u16,
};
use bmux_scene_protocol::glyphs::border_glyphs_corners_or_custom;
use bmux_scene_protocol::scene_protocol::{
    BorderGlyphs as SceneBorderGlyphs, Cell as SceneCell, Color as SceneColor, DecorationScene,
    GradientAxis, NamedColor, PaintCommand, Rect as SceneRect, STATE_KIND as SCENE_STATE_KIND,
    Style as SceneStyle, SurfaceDecoration, VisualAdapterRequest,
};
use bmux_scene_protocol_render::capabilities::{SceneRenderCapabilities, capability_query_matches};
use bmux_scene_protocol_render::paint::{
    apply_paint_command_with_capabilities, apply_paint_commands, interpolate_style,
};
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
    rendered_surfaces: BTreeMap<(Uuid, RenderExtensionLayer), SurfaceDecoration>,
    scene_rx: Option<tokio::sync::watch::Receiver<Arc<DecorationScene>>>,
    visual_last_at: BTreeMap<String, Instant>,
    visual_last_revision: BTreeMap<String, u64>,
    visual_last_payload_hash: BTreeMap<String, u64>,
    visual_adapter_cache: BTreeMap<String, Box<dyn std::any::Any + Send>>,
    visual_stats: BTreeMap<String, VisualProjectionStats>,
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
        true
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

    fn rendered_surface(&self, surface_id: &Uuid) -> Option<&SurfaceDecoration> {
        self.rendered_surface_layer(surface_id, RenderExtensionLayer::AfterPaneContent)
    }

    fn rendered_surface_layer(
        &self,
        surface_id: &Uuid,
        layer: RenderExtensionLayer,
    ) -> Option<&SurfaceDecoration> {
        self.rendered_surfaces.get(&(*surface_id, layer))
    }

    fn mark_layer_rendered(&mut self, surface_id: Uuid, layer: RenderExtensionLayer) {
        if let Some(surface) = self.surfaces.get(&surface_id) {
            self.mark_layer_rendered_snapshot(surface_id, layer, surface.clone());
        } else {
            self.rendered_surfaces
                .retain(|(rendered_id, _), _| rendered_id != &surface_id);
        }
    }

    fn mark_layer_rendered_snapshot(
        &mut self,
        surface_id: Uuid,
        layer: RenderExtensionLayer,
        surface: SurfaceDecoration,
    ) {
        self.rendered_surfaces.insert((surface_id, layer), surface);
    }

    fn mark_rendered(&mut self, surface_id: Uuid) {
        self.mark_layer_rendered(surface_id, RenderExtensionLayer::AfterPaneContent);
    }

    fn mark_rendered_snapshot(&mut self, surface_id: Uuid, surface: SurfaceDecoration) {
        self.mark_layer_rendered_snapshot(
            surface_id,
            RenderExtensionLayer::AfterPaneContent,
            surface,
        );
    }

    fn forget_surface(&mut self, surface_id: &Uuid) {
        self.rendered_surfaces
            .retain(|(rendered_id, _), _| rendered_id != surface_id);
        self.surfaces.remove(surface_id);
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

impl DecorationRenderExtension {
    fn render_layer_ops_with_terminal_capabilities(
        &self,
        surface_id: Uuid,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
        capabilities: TerminalRenderCapabilities,
    ) -> Option<Vec<RenderOp>> {
        let Ok(mut cache) = self.cache.lock() else {
            return Some(Vec::new());
        };
        let Some(surface) = cache.surface(&surface_id) else {
            cache.mark_layer_rendered(surface_id, layer);
            return Some(Vec::new());
        };
        if layer_paint_commands(surface, layer).is_empty() || damage.is_none() {
            cache.mark_layer_rendered(surface_id, layer);
            return Some(Vec::new());
        }
        let rendered_surface = surface.clone();
        let damaged_surface = filter_surface_layer_for_damage(surface, damage, layer);
        if layer_paint_commands(&damaged_surface, layer).is_empty() {
            cache.mark_layer_rendered_snapshot(surface_id, layer, rendered_surface);
            return Some(Vec::new());
        }
        let scene_capabilities = scene_capabilities_from_terminal(capabilities);
        let graphics_semantic_border_active = surface_has_graphics_semantic_border(
            surface_id,
            surface,
            capabilities,
            scene_capabilities,
        );
        let mut render_surface;
        let surface_for_ops =
            if layer == RenderExtensionLayer::AfterPaneContent && graphics_semantic_border_active {
                render_surface = surface.clone();
                render_surface
                    .paint_commands
                    .retain(|command| !matches!(command, PaintCommand::BoxBorder { .. }));
                &render_surface
            } else {
                surface
            };
        // If terminal graphics are unavailable, semantic borders fall back to
        // terminal-cell drawing. Render the full layer, not only the damaged
        // subset, so higher-z text decorations are replayed after lower-z
        // border/paddle clears in the same synchronized frame. Filtering the
        // command list here was the real-terminal flicker path: a dirty border
        // could be emitted without the header/score command that normally
        // occludes it.
        let ops = render_ops_for_surface_layer_with_capabilities(
            surface_for_ops,
            layer,
            scene_capabilities,
        )?;
        cache.mark_layer_rendered_snapshot(surface_id, layer, rendered_surface);
        Some(ops)
    }

    fn render_before_content_cells_with_capabilities(
        &self,
        surface_id: Uuid,
        damage: &RenderDamage,
        capabilities: SceneRenderCapabilities,
    ) -> Option<Vec<(u16, u16, RenderUnderCell)>> {
        let Ok(mut cache) = self.cache.lock() else {
            return Some(Vec::new());
        };
        let Some(surface) = cache.surface(&surface_id) else {
            cache.mark_layer_rendered(surface_id, RenderExtensionLayer::BeforePaneContent);
            return Some(Vec::new());
        };
        if surface.before_content_paint_commands.is_empty() || damage.is_none() {
            cache.mark_layer_rendered(surface_id, RenderExtensionLayer::BeforePaneContent);
            return Some(Vec::new());
        }
        let rendered_surface = surface.clone();
        let surface = filter_surface_layer_for_damage(
            surface,
            damage,
            RenderExtensionLayer::BeforePaneContent,
        );
        let ops = render_ops_for_surface_layer_with_capabilities(
            &surface,
            RenderExtensionLayer::BeforePaneContent,
            capabilities,
        )?;
        cache.mark_layer_rendered_snapshot(
            surface_id,
            RenderExtensionLayer::BeforePaneContent,
            rendered_surface,
        );
        Some(render_ops_to_under_cells(&ops))
    }

    fn render_layer_surface_imperative(
        &self,
        stdout: &mut dyn io::Write,
        surface_id: Uuid,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
        capabilities: TerminalRenderCapabilities,
    ) -> io::Result<bool> {
        let Ok(mut cache) = self.cache.lock() else {
            return Ok(false);
        };
        let Some(surface) = cache.surface(&surface_id) else {
            cache.mark_layer_rendered(surface_id, layer);
            return Ok(false);
        };
        if layer_paint_commands(surface, layer).is_empty() || damage.is_none() {
            cache.mark_layer_rendered(surface_id, layer);
            return Ok(false);
        }
        let rendered_surface = surface.clone();
        let surface = filter_surface_layer_for_damage(surface, damage, layer);
        let scene_capabilities = scene_capabilities_from_terminal(capabilities);
        let mut ordered: Vec<(usize, &PaintCommand)> = layer_paint_commands(&surface, layer)
            .iter()
            .enumerate()
            .collect();
        ordered.sort_by_key(|(index, command)| paint_command_sort_key(*index, command));
        let mut rendered = false;
        let mut emitted_text_style = false;
        for (_, command) in ordered {
            let mut writer: &mut dyn io::Write = &mut *stdout;
            let emitted =
                apply_paint_command_with_capabilities(&mut writer, command, scene_capabilities)
                    .map_err(|err| io::Error::other(err.to_string()))?;
            emitted_text_style |= emitted;
            rendered |= emitted;
        }
        if emitted_text_style {
            stdout.write_all(b"\x1b[0m")?;
        }
        cache.mark_layer_rendered_snapshot(surface_id, layer, rendered_surface);
        Ok(rendered)
    }
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

    fn surface_layer_damage(
        &self,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        layer: RenderExtensionLayer,
    ) -> RenderDamage {
        let Ok(cache) = self.cache.lock() else {
            return RenderDamage::None;
        };
        let current = cache.surface(&surface_id);
        let previous = cache.rendered_surface_layer(&surface_id, layer);
        decoration_surface_layer_damage(previous, current, layer)
    }

    fn surface_damage(&self, surface_id: Uuid, _surface_rect: &ExtensionRect) -> RenderDamage {
        let Ok(cache) = self.cache.lock() else {
            return RenderDamage::None;
        };
        let current = cache.surface(&surface_id);
        let previous = cache.rendered_surface(&surface_id);
        decoration_surface_damage(previous, current)
    }

    fn render_revision(&self, surface_id: Uuid) -> Option<u64> {
        let cache = self.cache.lock().ok()?;
        cache.surface(&surface_id).map(surface_revision)
    }

    fn render_layer_revision(&self, surface_id: Uuid, layer: RenderExtensionLayer) -> Option<u64> {
        let cache = self.cache.lock().ok()?;
        cache
            .surface(&surface_id)
            .map(|surface| surface_layer_revision(surface, layer))
    }

    fn redraws_on_content_damage(&self, layer: RenderExtensionLayer) -> bool {
        // Decoration commands frequently occupy border/title cells that real
        // pane output can still clear during full-row/full-screen terminal
        // repaints. Replaying the after-content layer on pane content damage is
        // required to keep headers, scores, paddles, and fallback borders from
        // disappearing until the next decoration tick.
        layer == RenderExtensionLayer::AfterPaneContent
    }

    fn render_surface(
        &self,
        stdout: &mut dyn io::Write,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        damage: &RenderDamage,
    ) -> io::Result<bool> {
        let Ok(mut cache) = self.cache.lock() else {
            return Ok(false);
        };
        let Some(surface) = cache.surface(&surface_id) else {
            cache.mark_rendered(surface_id);
            return Ok(false);
        };
        if surface.paint_commands.is_empty() || damage.is_none() {
            cache.mark_rendered(surface_id);
            return Ok(false);
        }
        let rendered_surface = surface.clone();
        let surface = filter_surface_for_damage(surface, damage);
        if surface.paint_commands.is_empty() {
            cache.mark_rendered_snapshot(surface_id, rendered_surface);
            return Ok(false);
        }
        // `apply_paint_commands` is generic over `W: io::Write` and
        // requires `Sized` because of the `crossterm::queue!` macro's
        // internals. Rebinding our `&mut dyn io::Write` to a local
        // `&mut impl io::Write` (the reborrow `&mut *stdout` creates
        // a fresh `&mut dyn io::Write`, which is itself Sized) lets
        // the generic bound see a Sized writer.
        let mut writer: &mut dyn io::Write = &mut *stdout;
        let rendered = apply_paint_commands(&mut writer, &surface)
            .map(|()| true)
            .map_err(|err| io::Error::other(err.to_string()))?;
        cache.mark_rendered_snapshot(surface_id, rendered_surface);
        Ok(rendered)
    }

    fn render_layer_ops(
        &self,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
    ) -> Option<Vec<RenderOp>> {
        self.render_layer_ops_with_terminal_capabilities(
            surface_id,
            damage,
            layer,
            TerminalRenderCapabilities::default(),
        )
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

    fn render_layer_items_with_context(
        &self,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Option<Vec<RenderLayerItem>> {
        let Ok(mut cache) = self.cache.lock() else {
            return Some(Vec::new());
        };
        let Some(surface) = cache.surface(&surface_id) else {
            cache.mark_layer_rendered(surface_id, layer);
            return Some(Vec::new());
        };
        let rendered_surface = surface.clone();
        let previous_surface = cache.rendered_surface_layer(&surface_id, layer).cloned();
        if layer_paint_commands(surface, layer).is_empty() && previous_surface.is_none()
            || damage.is_none()
        {
            cache.mark_layer_rendered(surface_id, layer);
            return Some(Vec::new());
        }

        let scene_capabilities = scene_capabilities_from_terminal(context.capabilities);
        let global_occluders = &context.opaque_occluders;
        let previous_used_graphics = previous_surface.as_ref().is_some_and(|previous| {
            layer_paint_commands(previous, layer)
                .iter()
                .enumerate()
                .any(|(semantic_index, command)| {
                    raster_border::semantic_border_graphic_items_with_occlusion(
                        surface_id,
                        u64::try_from(semantic_index).unwrap_or(u64::MAX),
                        command,
                        context.capabilities,
                        scene_capabilities,
                        global_occluders,
                    )
                    .is_some()
                })
        });

        let layer_commands = layer_paint_commands(surface, layer);
        let mut ordered: Vec<(usize, &PaintCommand)> = layer_commands.iter().enumerate().collect();
        ordered.sort_by_key(|(index, command)| paint_command_sort_key(*index, command));

        let mut used_graphics = previous_used_graphics;
        let mut items = Vec::new();
        for (command_index, command) in ordered {
            if let Some(graphics) = raster_border::semantic_border_graphic_items_with_occlusion(
                surface_id,
                u64::try_from(command_index).unwrap_or(u64::MAX),
                command,
                context.capabilities,
                scene_capabilities,
                global_occluders,
            ) {
                used_graphics = true;
                items.extend(graphics);
                continue;
            }
            if !paint_command_intersects_render_damage(command, damage) {
                continue;
            }
            let ops = render_ops_for_paint_commands_with_capabilities(
                std::slice::from_ref(command),
                scene_capabilities,
            )?;
            items.extend(ops.into_iter().map(RenderLayerItem::Op));
        }
        if !used_graphics {
            return None;
        }
        cache.mark_layer_rendered_snapshot(surface_id, layer, rendered_surface);
        Some(items)
    }

    fn render_layer_ops_with_context(
        &self,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Option<Vec<RenderOp>> {
        self.render_layer_ops_with_terminal_capabilities(
            surface_id,
            damage,
            layer,
            context.capabilities,
        )
    }

    fn render_before_content_cells(
        &self,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        damage: &RenderDamage,
    ) -> Option<Vec<(u16, u16, RenderUnderCell)>> {
        self.render_before_content_cells_with_capabilities(
            surface_id,
            damage,
            SceneRenderCapabilities::default(),
        )
    }

    fn render_before_content_cells_with_context(
        &self,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        context: &RenderExtensionContext,
    ) -> Option<Vec<(u16, u16, RenderUnderCell)>> {
        self.render_before_content_cells_with_capabilities(
            surface_id,
            damage,
            scene_capabilities_from_terminal(context.capabilities),
        )
    }

    fn render_layer_surface_with_context(
        &self,
        stdout: &mut dyn io::Write,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> io::Result<bool> {
        self.render_layer_surface_imperative(
            stdout,
            surface_id,
            damage,
            layer,
            context.capabilities,
        )
    }

    fn render_ops(
        &self,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        damage: &RenderDamage,
    ) -> Option<Vec<RenderOp>> {
        let Ok(mut cache) = self.cache.lock() else {
            return Some(Vec::new());
        };
        let Some(surface) = cache.surface(&surface_id) else {
            cache.mark_rendered(surface_id);
            return Some(Vec::new());
        };
        if surface.paint_commands.is_empty() || damage.is_none() {
            cache.mark_rendered(surface_id);
            return Some(Vec::new());
        }
        let rendered_surface = surface.clone();
        let surface = filter_surface_for_damage(surface, damage);
        if surface.paint_commands.is_empty() {
            cache.mark_rendered_snapshot(surface_id, rendered_surface);
            return Some(Vec::new());
        }
        let ops = render_ops_for_surface(&surface)?;
        cache.mark_rendered_snapshot(surface_id, rendered_surface);
        Some(ops)
    }

    fn content_rect_override(&self, surface_id: Uuid) -> Option<ExtensionRect> {
        let cache = self.cache.lock().ok()?;
        let surface = cache.surface(&surface_id)?;
        Some(extension_rect_from_scene(&surface.content_rect))
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

fn surface_has_graphics_semantic_border(
    surface_id: Uuid,
    surface: &SurfaceDecoration,
    capabilities: TerminalRenderCapabilities,
    scene_capabilities: SceneRenderCapabilities,
) -> bool {
    surface
        .before_content_paint_commands
        .iter()
        .chain(surface.paint_commands.iter())
        .enumerate()
        .any(|(index, command)| {
            raster_border::semantic_border_graphic_items(
                surface_id,
                u64::try_from(index).unwrap_or(u64::MAX),
                command,
                capabilities,
                scene_capabilities,
            )
            .is_some()
        })
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

fn layer_paint_commands_mut(
    surface: &mut SurfaceDecoration,
    layer: RenderExtensionLayer,
) -> &mut Vec<PaintCommand> {
    match layer {
        RenderExtensionLayer::BeforePaneContent => &mut surface.before_content_paint_commands,
        RenderExtensionLayer::AfterPaneContent => &mut surface.paint_commands,
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
    let Some(adapter) = registered_visual_adapter(&request.adapter) else {
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

fn decoration_surface_layer_damage(
    previous: Option<&SurfaceDecoration>,
    current: Option<&SurfaceDecoration>,
    layer: RenderExtensionLayer,
) -> RenderDamage {
    match (previous, current) {
        (None, None) | (Some(_), Some(_)) if previous == current => RenderDamage::None,
        (previous, current) => paint_command_list_damage(
            previous.map(|surface| layer_paint_commands(surface, layer)),
            current.map(|surface| layer_paint_commands(surface, layer)),
        ),
    }
}

fn decoration_surface_damage(
    previous: Option<&SurfaceDecoration>,
    current: Option<&SurfaceDecoration>,
) -> RenderDamage {
    match (previous, current) {
        (None, None) | (Some(_), Some(_)) if previous == current => RenderDamage::None,
        (previous, current) => paint_command_list_damage(
            previous.map(|surface| surface.paint_commands.as_slice()),
            current.map(|surface| surface.paint_commands.as_slice()),
        ),
    }
}

fn paint_command_list_damage(
    previous: Option<&[PaintCommand]>,
    current: Option<&[PaintCommand]>,
) -> RenderDamage {
    let previous = previous.unwrap_or_default();
    let current = current.unwrap_or_default();
    let max_len = previous.len().max(current.len());
    RenderDamage::from_rects((0..max_len).flat_map(|index| {
        match (previous.get(index), current.get(index)) {
            (Some(previous), Some(current)) if previous == current => Vec::new(),
            (previous, current) => previous
                .into_iter()
                .chain(current)
                .flat_map(paint_command_damage)
                .collect(),
        }
    }))
}

fn surface_revision(surface: &SurfaceDecoration) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_revision_part(&mut hasher, &surface.rect);
    hash_revision_part(&mut hasher, &surface.content_rect);
    hash_revision_part(&mut hasher, &surface.before_content_paint_commands);
    hash_revision_part(&mut hasher, &surface.paint_commands);
    hasher.finish()
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

fn paint_command_intersects_render_damage(command: &PaintCommand, damage: &RenderDamage) -> bool {
    match damage {
        RenderDamage::None => false,
        RenderDamage::FullSurface => true,
        RenderDamage::Regions(regions) => paint_command_damage(command)
            .any(|rect| regions.iter().any(|region| region.intersects(rect))),
    }
}

fn filter_surface_layer_for_damage(
    surface: &SurfaceDecoration,
    damage: &RenderDamage,
    layer: RenderExtensionLayer,
) -> SurfaceDecoration {
    let mut filtered = surface.clone();
    if matches!(damage, RenderDamage::FullSurface) {
        return filtered;
    }
    let RenderDamage::Regions(regions) = damage else {
        layer_paint_commands_mut(&mut filtered, layer).clear();
        return filtered;
    };
    *layer_paint_commands_mut(&mut filtered, layer) = layer_paint_commands(surface, layer)
        .iter()
        .filter(|command| {
            paint_command_damage(command)
                .any(|rect| regions.iter().any(|region| region.intersects(rect)))
        })
        .cloned()
        .collect();
    filtered
}

fn filter_surface_for_damage(
    surface: &SurfaceDecoration,
    damage: &RenderDamage,
) -> SurfaceDecoration {
    if matches!(damage, RenderDamage::FullSurface) {
        return surface.clone();
    }
    let RenderDamage::Regions(regions) = damage else {
        let mut filtered = surface.clone();
        filtered.paint_commands.clear();
        return filtered;
    };
    let mut filtered = surface.clone();
    filtered.paint_commands = surface
        .paint_commands
        .iter()
        .filter(|command| {
            paint_command_damage(command)
                .any(|rect| regions.iter().any(|region| region.intersects(rect)))
        })
        .cloned()
        .collect();
    filtered
}

#[must_use]
pub fn render_ops_for_surface_layer(
    surface: &SurfaceDecoration,
    layer: RenderExtensionLayer,
) -> Option<Vec<RenderOp>> {
    render_ops_for_surface_layer_with_capabilities(
        surface,
        layer,
        SceneRenderCapabilities::default(),
    )
}

#[must_use]
pub fn render_ops_for_surface_layer_with_capabilities(
    surface: &SurfaceDecoration,
    layer: RenderExtensionLayer,
    capabilities: SceneRenderCapabilities,
) -> Option<Vec<RenderOp>> {
    render_ops_for_paint_commands_with_capabilities(
        layer_paint_commands(surface, layer),
        capabilities,
    )
}

#[must_use]
pub fn render_ops_for_surface(surface: &SurfaceDecoration) -> Option<Vec<RenderOp>> {
    render_ops_for_paint_commands(&surface.paint_commands)
}

#[must_use]
pub fn render_ops_for_paint_commands(paint_commands: &[PaintCommand]) -> Option<Vec<RenderOp>> {
    render_ops_for_paint_commands_with_capabilities(
        paint_commands,
        SceneRenderCapabilities::default(),
    )
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
    let terminal_cell_occluders =
        command_regions_after(context.terminal_cell_regions, command_index, command);
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
        items.push(render_scene_item_from_op(
            format!("{key_prefix}:op-{op_index:02}"),
            z,
            op,
        ));
    }
}

fn render_scene_item_from_op(key: String, z: i16, op: RenderOp) -> RenderSceneItem {
    match op {
        RenderOp::TextRun { x, y, text, style } => RenderSceneItem::text(key, z, x, y, text, style),
        RenderOp::StyledText { x, y, spans } => RenderSceneItem::styled_text(key, z, x, y, spans),
        RenderOp::ClearRect { rect, style } => {
            RenderSceneItem::fill_rect(RenderSceneItemKey::new(key), z, rect, ' ', style)
        }
        RenderOp::EraseRowSegment { x, y, width, style } => RenderSceneItem::fill_rect(
            RenderSceneItemKey::new(key),
            z,
            ExtensionRect::new(x, y, width, 1),
            ' ',
            style,
        ),
        RenderOp::FillRect { rect, ch, style } => {
            RenderSceneItem::fill_rect(RenderSceneItemKey::new(key), z, rect, ch, style)
        }
        RenderOp::Border {
            rect,
            glyphs,
            style,
        } => RenderSceneItem::border(RenderSceneItemKey::new(key), z, rect, glyphs, style),
        RenderOp::CellGrid { x, y, rows } => RenderSceneItem::cell_grid(key, z, x, y, rows),
    }
}

fn render_ops_to_under_cells(ops: &[RenderOp]) -> Vec<(u16, u16, RenderUnderCell)> {
    let mut cells = Vec::new();
    for op in ops {
        match op {
            RenderOp::TextRun { x, y, text, style } => {
                let mut col = *x;
                for ch in text.chars() {
                    cells.push((col, *y, RenderUnderCell { ch, style: *style }));
                    col = col.saturating_add(1);
                }
            }
            RenderOp::FillRect { rect, ch, style } => {
                for row in rect.y..rect.bottom() {
                    for col in rect.x..rect.right() {
                        cells.push((
                            col,
                            row,
                            RenderUnderCell {
                                ch: *ch,
                                style: *style,
                            },
                        ));
                    }
                }
            }
            RenderOp::CellGrid { x, y, rows } => {
                for (row_offset, row) in rows.iter().enumerate() {
                    let Ok(row_offset) = u16::try_from(row_offset) else {
                        break;
                    };
                    for (col_offset, cell) in row.iter().enumerate() {
                        let Ok(col_offset) = u16::try_from(col_offset) else {
                            break;
                        };
                        let Some(ch) = cell.ch else { continue };
                        cells.push((
                            x.saturating_add(col_offset),
                            y.saturating_add(row_offset),
                            RenderUnderCell {
                                ch,
                                style: cell.style,
                            },
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    cells
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

/// Process-wide handle to the installed extension's cache. `install` stores it
/// on first call; the retained scene-state relay (living in the CLI's streaming
/// loop) updates the local state channel that feeds this cache.
static INSTALLED_CACHE: OnceLock<Arc<Mutex<DecorationRendererCache>>> = OnceLock::new();

/// Install the decoration render extension.
///
/// Idempotent: the first call registers an `AttachRenderExtension`
/// with [`bmux_plugin::register_render_extension`] and remembers the
/// installed cache handle; subsequent calls return immediately.
///
/// Call this once during CLI bootstrap when the decoration plugin is
/// bundled. No `install` call means no decoration painting —
/// deployments that don't bundle the decoration plugin can simply
/// skip this crate.
pub fn install() {
    // SAFETY: `OnceLock` coordinates single-shot initialisation; repeat
    // calls are no-ops after the first.
    let _ = INSTALLED_CACHE.get_or_init(|| {
        let cache: Arc<Mutex<DecorationRendererCache>> =
            Arc::new(Mutex::new(DecorationRendererCache::default()));
        let ext = Arc::new(DecorationRenderExtension {
            name: "bmux.decoration.renderer".to_string(),
            cache: cache.clone(),
        }) as Arc<dyn AttachRenderExtension>;
        bmux_plugin::register_render_extension(ext);
        // Register a local retained state channel for scene updates. The CLI's
        // streaming loop re-publishes IPC-delivered `PluginBusEvent`s onto this
        // channel so the extension can drain the retained state at the frame
        // boundary without a background subscriber race.
        let _ = bmux_plugin::global_event_bus().register_state_channel::<DecorationScene>(
            SCENE_STATE_KIND,
            DecorationScene {
                revision: 0,
                surfaces: BTreeMap::new(),
                animation: None,
                input_hooks: Vec::new(),
                visual_adapters: Vec::new(),
            },
        );
        match bmux_plugin::global_event_bus().subscribe_state::<DecorationScene>(&SCENE_STATE_KIND)
        {
            Ok((initial, rx)) => {
                if let Ok(mut guard) = cache.lock() {
                    guard.replace_if_newer(initial.as_ref().clone());
                    guard.set_scene_receiver(rx);
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "decoration render extension: scene-protocol state channel not registered"
                );
            }
        }
        tracing::debug!("decoration render extension installed");
        cache
    });
}

/// Manual push path. Callers that receive scene payloads from a
/// transport other than the local event bus (e.g. the CLI's
/// streaming loop decoding IPC `PluginBusEvent`s) call this to
/// update the cache directly.
pub fn push_scene(scene: DecorationScene) -> bool {
    let Some(cache) = INSTALLED_CACHE.get() else {
        return false;
    };
    let Ok(mut guard) = cache.lock() else {
        return false;
    };
    guard.replace_if_newer(scene)
}

#[cfg(test)]
mod tests {
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

    fn install_test_visual_adapter() {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| bmux_plugin::register_visual_adapter(Arc::new(TestVisualAdapter)));
        TEST_VISUAL_ADAPTER_CALLS.store(0, Ordering::SeqCst);
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
        let cache = Arc::new(Mutex::new(DecorationRendererCache::default()));
        cache.lock().expect("cache lock").set_scene_receiver(rx);
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache: cache.clone(),
        };
        (extension, cache)
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
            &scene.items[0].kind,
            bmux_plugin::RenderSceneItemKind::Border { rect: actual, glyphs, .. }
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
    fn decoration_damage_uses_paint_regions_when_content_rect_changes() {
        let surface_id = Uuid::from_u128(100);
        let mut previous = surface(
            surface_id,
            vec![PaintCommand::BoxBorder {
                rect: SceneRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                z: 0,
                glyphs: SceneBorderGlyphs::Ascii,
                style: scene_style(),
            }],
        );
        let mut current = previous.clone();
        previous.content_rect.w = 18;
        current.content_rect.w = 16;
        current.paint_commands = vec![PaintCommand::BoxBorder {
            rect: SceneRect {
                x: 0,
                y: 0,
                w: 16,
                h: 10,
            },
            z: 0,
            glyphs: SceneBorderGlyphs::Ascii,
            style: scene_style(),
        }];

        let damage = decoration_surface_layer_damage(
            Some(&previous),
            Some(&current),
            RenderExtensionLayer::AfterPaneContent,
        );

        assert!(matches!(damage, RenderDamage::Regions(_)));
    }

    #[test]
    fn rendering_before_layer_does_not_suppress_after_layer_damage() {
        let surface_id = Uuid::from_u128(102);
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
        let cache = Arc::new(Mutex::new(DecorationRendererCache {
            revision: 1,
            surfaces: BTreeMap::from([(surface_id, decoration)]),
            rendered_surfaces: BTreeMap::new(),
            scene_rx: None,
            visual_last_at: BTreeMap::new(),
            visual_last_revision: BTreeMap::new(),
            visual_last_payload_hash: BTreeMap::new(),
            visual_adapter_cache: BTreeMap::new(),
            visual_stats: BTreeMap::new(),
        }));
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache,
        };

        assert!(
            extension
                .render_before_content_cells(
                    surface_id,
                    &ExtensionRect::new(0, 0, 10, 5),
                    &RenderDamage::FullSurface,
                )
                .is_some()
        );
        let after_damage = extension.surface_layer_damage(
            surface_id,
            &ExtensionRect::new(0, 0, 10, 5),
            RenderExtensionLayer::AfterPaneContent,
        );

        assert!(!after_damage.is_none());
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
            rendered_surfaces: BTreeMap::new(),
            scene_rx: None,
            visual_last_at: BTreeMap::new(),
            visual_last_revision: BTreeMap::new(),
            visual_last_payload_hash: BTreeMap::new(),
            visual_adapter_cache: BTreeMap::new(),
            visual_stats: BTreeMap::new(),
        }));
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache: cache.clone(),
        };
        (extension, cache)
    }

    #[test]
    fn retained_scene_converts_text_commands_without_marking_legacy_rendered_cache() {
        let surface_id = Uuid::from_u128(201);
        let (extension, cache) = extension_with_surface(
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
            &scene.items[0].kind,
            bmux_plugin::RenderSceneItemKind::Text { x: 2, y: 3, text, .. } if text == "perf"
        ));
        assert!(
            cache
                .lock()
                .expect("cache lock")
                .rendered_surface_layer(&surface_id, RenderExtensionLayer::AfterPaneContent)
                .is_none(),
            "retained scene diffing is renderer-owned; decoration only keeps legacy rendered snapshots for fallback APIs"
        );
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
            &scene.items[0].kind,
            bmux_plugin::RenderSceneItemKind::Text { x: 1, y: 0, text, .. } if text == "under"
        ));
        assert!(
            cache
                .lock()
                .expect("cache lock")
                .rendered_surface_layer(&surface_id, RenderExtensionLayer::BeforePaneContent)
                .is_none(),
            "retained before-content scenes do not update the legacy fallback rendered-surface cache"
        );
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
        assert!(scene.items.iter().all(|item| matches!(
            item.kind,
            bmux_plugin::RenderSceneItemKind::TerminalGraphic { .. }
        )));
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
        assert!(scene.items.iter().all(|item| matches!(
            item.kind,
            bmux_plugin::RenderSceneItemKind::TerminalGraphic { .. }
        )));
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

        assert!(
            scene
                .items
                .iter()
                .any(|item| matches!(item.kind, bmux_plugin::RenderSceneItemKind::Border { .. }))
        );
    }

    #[test]
    fn partial_render_snapshot_preserves_full_surface_after_successful_emit() {
        let surface_id = Uuid::from_u128(103);
        let (extension, cache) = extension_with_surface(
            surface_id,
            vec![
                PaintCommand::Text {
                    col: 1,
                    row: 1,
                    z: 0,
                    text: "dirty".to_string(),
                    style: scene_style(),
                },
                PaintCommand::Text {
                    col: 1,
                    row: 3,
                    z: 0,
                    text: "later".to_string(),
                    style: scene_style(),
                },
            ],
        );

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect::new(0, 0, 20, 10),
                &RenderDamage::Regions(vec![ExtensionRect::new(1, 1, 5, 1)]),
            )
            .expect("partial render should use declarative ops");
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], RenderOp::TextRun { text, .. } if text == "dirty"));

        let rendered = cache
            .lock()
            .expect("cache should lock")
            .rendered_surface(&surface_id)
            .expect("partial snapshot recorded")
            .clone();
        assert_eq!(rendered.paint_commands.len(), 2);
        assert!(
            matches!(&rendered.paint_commands[0], PaintCommand::Text { text, .. } if text == "dirty")
        );
        assert!(
            matches!(&rendered.paint_commands[1], PaintCommand::Text { text, .. } if text == "later")
        );

        let followup_damage =
            extension.surface_damage(surface_id, &ExtensionRect::new(0, 0, 20, 10));
        assert_eq!(followup_damage, RenderDamage::None);
    }

    #[test]
    fn visual_adapter_dirty_only_uses_content_revision_and_suppresses_unchanged_payload() {
        install_test_visual_adapter();
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
            !extension
                .surface_damage(surface_id, &ExtensionRect::new(0, 0, 10, 5))
                .is_none()
        );
    }

    #[test]
    fn render_ops_converts_supported_text_and_marks_rendered() {
        let surface_id = Uuid::from_u128(1);
        let paint_commands = vec![PaintCommand::Text {
            col: 2,
            row: 3,
            z: 0,
            text: "hello".to_string(),
            style: scene_style(),
        }];
        let expected_revision = surface_revision(&surface(surface_id, paint_commands.clone()));
        let (extension, cache) = extension_with_surface(surface_id, paint_commands);

        assert_eq!(
            extension.render_revision(surface_id),
            Some(expected_revision)
        );
        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("supported decoration should use declarative ops");

        assert_eq!(
            ops,
            vec![RenderOp::TextRun {
                x: 2,
                y: 3,
                text: "hello".to_string(),
                style: RenderStyle::default(),
            }]
        );
        assert!(
            cache
                .lock()
                .expect("cache should lock")
                .rendered_surface(&surface_id)
                .is_some()
        );
    }

    #[test]
    fn render_ops_converts_space_fill_to_clear_rect() {
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

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("space fills should be declarative clears");

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
    fn render_ops_converts_empty_cell_grid_cells_to_sparse_cells() {
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

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("empty grid cells should be sparse declarative cells");

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
    fn render_ops_filters_semantic_border_by_capability_context() {
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

        let rect = ExtensionRect {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        };
        let no_graphics = extension
            .render_layer_ops_with_context(
                surface_id,
                &rect,
                &RenderDamage::FullSurface,
                RenderExtensionLayer::AfterPaneContent,
                &RenderExtensionContext::default(),
            )
            .expect("semantic borders should remain declarative");
        assert!(no_graphics.is_empty());

        let kitty_capabilities = TerminalRenderCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            ..TerminalRenderCapabilities::default()
        };
        let kitty = extension
            .render_layer_ops_with_context(
                surface_id,
                &rect,
                &RenderDamage::FullSurface,
                RenderExtensionLayer::AfterPaneContent,
                &RenderExtensionContext {
                    capabilities: kitty_capabilities,
                    ..RenderExtensionContext::default()
                },
            )
            .expect("operation path remains a deterministic text fallback");
        assert!(matches!(
            &kitty[..],
            [RenderOp::Border { glyphs, .. }]
                if glyphs.top_left == '┏' && glyphs.horizontal == '━' && glyphs.vertical == '┃'
        ));

        let graphics = extension
            .render_layer_items_with_context(
                surface_id,
                &rect,
                &RenderDamage::FullSurface,
                RenderExtensionLayer::AfterPaneContent,
                &RenderExtensionContext {
                    capabilities: TerminalRenderCapabilities {
                        graphics_alpha: true,
                        ..kitty_capabilities
                    },
                    ..RenderExtensionContext::default()
                },
            )
            .expect("kitty alpha terminals should use graphics items");
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
            rendered_surfaces: BTreeMap::new(),
            scene_rx: None,
            visual_last_at: BTreeMap::new(),
            visual_last_revision: BTreeMap::new(),
            visual_last_payload_hash: BTreeMap::new(),
            visual_adapter_cache: BTreeMap::new(),
            visual_stats: BTreeMap::new(),
        }));
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache,
        };

        let ops = extension
            .render_layer_ops_with_context(
                surface_id,
                &ExtensionRect::new(0, 0, 20, 8),
                &RenderDamage::FullSurface,
                RenderExtensionLayer::AfterPaneContent,
                &kitty_alpha_context(),
            )
            .expect("after layer should render header ops");

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
            rendered_surfaces: BTreeMap::new(),
            scene_rx: None,
            visual_last_at: BTreeMap::new(),
            visual_last_revision: BTreeMap::new(),
            visual_last_payload_hash: BTreeMap::new(),
            visual_adapter_cache: BTreeMap::new(),
            visual_stats: BTreeMap::new(),
        }));
        let extension = DecorationRenderExtension {
            name: "test.decoration.renderer".to_string(),
            cache,
        };

        let items = extension
            .render_layer_items_with_context(
                surface_id,
                &ExtensionRect::new(0, 0, 20, 8),
                &RenderDamage::FullSurface,
                RenderExtensionLayer::BeforePaneContent,
                &kitty_alpha_context(),
            )
            .expect("before-content semantic border should use graphics items");

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

        let items = extension
            .render_layer_items_with_context(
                surface_id,
                &ExtensionRect::new(0, 0, 20, 5),
                &RenderDamage::FullSurface,
                RenderExtensionLayer::AfterPaneContent,
                &kitty_alpha_context(),
            )
            .expect("semantic border should use graphics items");
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

        let items = extension
            .render_layer_items_with_context(
                surface_id,
                &ExtensionRect::new(0, 0, 20, 8),
                &RenderDamage::FullSurface,
                RenderExtensionLayer::AfterPaneContent,
                &kitty_alpha_context(),
            )
            .expect("semantic border should use graphics items");
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
            .filter_map(|item| match &item.kind {
                bmux_plugin::RenderSceneItemKind::TerminalGraphic { graphic } => {
                    Some((item.key.clone(), graphic.cell_rect))
                }
                _ => None,
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
        let global_scene = render_scene_for_surface_layer_with_capabilities(
            surface_id,
            &surface(surface_id, snake),
            RenderExtensionLayer::AfterPaneContent,
            kitty_capabilities(),
            SceneRenderCapabilities::default(),
            &[ExtensionRect::new(7, 0, 6, 4)],
        );
        let global_graphics = terminal_graphic_signature(&global_scene);
        assert!(global_graphics.len() > 4);
        assert!(
            global_graphics.iter().all(|(_, graphic_rect)| {
                !graphic_rect.intersects(ExtensionRect::new(7, 0, 6, 4))
            })
        );
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
    fn render_ops_falls_back_for_wide_cell_grid_glyphs() {
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

        assert!(
            extension
                .render_ops(
                    surface_id,
                    &ExtensionRect {
                        x: 0,
                        y: 0,
                        w: 20,
                        h: 10,
                    },
                    &RenderDamage::FullSurface,
                )
                .is_none()
        );
    }

    #[test]
    fn render_ops_falls_back_for_wide_border_glyphs() {
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

        assert!(
            extension
                .render_ops(
                    surface_id,
                    &ExtensionRect {
                        x: 0,
                        y: 0,
                        w: 20,
                        h: 10,
                    },
                    &RenderDamage::FullSurface,
                )
                .is_none()
        );
    }

    #[test]
    fn render_ops_preserves_named_color() {
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

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("named colors should be declarative");

        assert!(matches!(
            &ops[..],
            [RenderOp::TextRun { style, .. }]
                if style.fg == Some(RenderColor::Named(RenderNamedColor::BrightYellow))
        ));
    }

    #[test]
    fn surface_damage_uses_display_width_for_text_commands() {
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

        assert_eq!(
            extension.surface_damage(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
            ),
            RenderDamage::Regions(vec![ExtensionRect {
                x: 2,
                y: 3,
                w: 2,
                h: 1,
            }])
        );
    }

    #[test]
    fn render_ops_sorts_by_z_before_returning_ops() {
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

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("supported decoration should use declarative ops");

        assert!(matches!(
            &ops[..],
            [RenderOp::TextRun { text: low, .. }, RenderOp::TextRun { text: high, .. }]
                if low == "low" && high == "high"
        ));
    }

    #[test]
    fn render_ops_lowers_horizontal_gradient_runs() {
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

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("gradient runs should lower to text ops");

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
    fn render_ops_lowers_vertical_gradient_runs() {
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

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("vertical gradients should lower to text ops");

        assert!(matches!(
            &ops[..],
            [
                RenderOp::TextRun { x: 4, y: 2, text: a, .. },
                RenderOp::TextRun { x: 4, y: 3, text: b, .. },
            ] if a == "a" && b == "b"
        ));
    }

    #[test]
    fn render_ops_converts_full_style_flags_and_marks_rendered() {
        let surface_id = Uuid::from_u128(3);
        let mut style = scene_style();
        style.underline = true;
        style.italic = true;
        style.reverse = true;
        style.dim = true;
        style.blink = true;
        style.strikethrough = true;
        let (extension, cache) = extension_with_surface(
            surface_id,
            vec![PaintCommand::Text {
                col: 2,
                row: 3,
                z: 0,
                text: "hello".to_string(),
                style,
            }],
        );

        let ops = extension
            .render_ops(
                surface_id,
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &RenderDamage::FullSurface,
            )
            .expect("full style flags should be declarative");

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
        assert!(
            cache
                .lock()
                .expect("cache should lock")
                .rendered_surface(&surface_id)
                .is_some()
        );
    }
}
