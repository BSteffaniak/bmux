use crate::types::TerminalGraphicsCache;
use bmux_attach_layout_protocol::{AttachScene, AttachSurfaceKind};
use bmux_plugin::{TerminalGraphicOverlay, TerminalRenderCapabilities};
use std::collections::BTreeSet;
use uuid::Uuid;

use super::{
    AttachSceneRenderStats, TerminalGraphicsResourceStats, cleanup_stale_terminal_graphics,
    cleanup_stale_terminal_graphics_for_surface, queue_terminal_graphic_overlay,
    terminal_graphics_shown,
};
use crate::canvas::Canvas;

/// Keep the cached overlays of visible pane surfaces: a frame that does not repaint a surface
/// keeps its overlays.
fn retain_cached_terminal_graphics_for_visible_surfaces(
    scene: &AttachScene,
    graphics_cache: &TerminalGraphicsCache,
    active_terminal_graphics: &mut BTreeSet<u64>,
) {
    let visible_surfaces = scene
        .surfaces
        .iter()
        .filter_map(|surface| {
            if !surface.visible
                || !matches!(
                    surface.kind,
                    AttachSurfaceKind::Pane | AttachSurfaceKind::FloatingPane
                )
            {
                return None;
            }
            surface.pane_id.map(|pane_id| (pane_id, surface.id))
        })
        .collect::<BTreeSet<_>>();
    active_terminal_graphics.extend(graphics_cache.iter().filter_map(|(key, entry)| {
        visible_surfaces
            .contains(&(entry.pane_id, entry.surface_id))
            .then_some(*key)
    }));
}

/// One frame's extension graphic overlays: those still active, and what contributing and
/// removing them cost.
#[derive(Default)]
pub(super) struct TerminalGraphicsFrameResources {
    active_graphics: BTreeSet<u64>,
    pub(super) stats: TerminalGraphicsResourceStats,
}

impl TerminalGraphicsFrameResources {
    /// Start a frame: overlays of surfaces no longer visible, and every overlay when the frame
    /// shows none, are removed.
    pub(super) fn begin(
        canvas: &mut Canvas<'_, '_, '_>,
        scene: &AttachScene,
        graphics_cache: &mut TerminalGraphicsCache,
        capabilities: TerminalRenderCapabilities,
    ) -> Self {
        let mut resources = Self::default();
        if terminal_graphics_shown(canvas.for_host(), capabilities) {
            retain_cached_terminal_graphics_for_visible_surfaces(
                scene,
                graphics_cache,
                &mut resources.active_graphics,
            );
        }
        resources.cleanup_stale(canvas, graphics_cache);
        resources
    }

    pub(super) fn activate_graphics(&mut self, current_graphics: BTreeSet<u64>) {
        self.active_graphics.extend(current_graphics);
    }

    pub(super) fn cleanup_stale_for_surface(
        &mut self,
        canvas: &mut Canvas<'_, '_, '_>,
        pane_id: Uuid,
        surface_id: Uuid,
        current_graphics: &BTreeSet<u64>,
        graphics_cache: &mut TerminalGraphicsCache,
    ) -> bool {
        cleanup_stale_terminal_graphics_for_surface(
            canvas,
            pane_id,
            surface_id,
            current_graphics,
            graphics_cache,
            Some(&mut self.stats),
        )
    }

    pub(super) fn queue_graphic_overlay(
        &mut self,
        canvas: &mut Canvas<'_, '_, '_>,
        identity: (Uuid, Uuid, u64),
        graphic: &TerminalGraphicOverlay,
        graphics_cache: &mut TerminalGraphicsCache,
    ) -> bool {
        queue_terminal_graphic_overlay(
            canvas,
            identity,
            graphic,
            graphics_cache,
            Some(&mut self.stats),
        )
    }

    pub(super) fn cleanup_stale(
        &mut self,
        canvas: &mut Canvas<'_, '_, '_>,
        graphics_cache: &mut TerminalGraphicsCache,
    ) -> bool {
        cleanup_stale_terminal_graphics(
            canvas,
            &self.active_graphics,
            graphics_cache,
            Some(&mut self.stats),
        )
    }

    pub(super) fn finish(
        &mut self,
        canvas: &mut Canvas<'_, '_, '_>,
        graphics_cache: &mut TerminalGraphicsCache,
        render_stats: Option<&mut AttachSceneRenderStats>,
    ) {
        self.cleanup_stale(canvas, graphics_cache);
        if let Some(stats) = render_stats {
            self.stats.apply_to(stats);
        }
    }
}
