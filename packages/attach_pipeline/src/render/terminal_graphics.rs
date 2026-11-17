use crate::types::TerminalGraphicsCache;
use anyhow::Result;
use bmux_attach_layout_protocol::{AttachScene, AttachSurfaceKind};
use bmux_plugin::{TerminalGraphicOverlay, TerminalRenderCapabilities};
use std::collections::BTreeSet;
use uuid::Uuid;

use super::{
    AttachSceneRenderStats, TerminalGraphicsResourceStats, cleanup_stale_terminal_graphics,
    cleanup_stale_terminal_graphics_for_surface, queue_terminal_graphic_overlay,
    terminal_graphic_can_render,
};

fn retain_cached_terminal_graphics_for_visible_surfaces(
    scene: &AttachScene,
    graphics_cache: &TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
    active_terminal_graphics: &mut BTreeSet<u64>,
) {
    if !terminal_graphic_can_render(capabilities) {
        return;
    }
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

#[derive(Default)]
pub(super) struct TerminalGraphicsFrameResources {
    active_graphics: BTreeSet<u64>,
    pub(super) stats: TerminalGraphicsResourceStats,
}

impl TerminalGraphicsFrameResources {
    pub(super) fn begin(
        scene: &AttachScene,
        graphics_cache: &mut TerminalGraphicsCache,
        capabilities: TerminalRenderCapabilities,
    ) -> Result<Self> {
        let mut resources = Self::default();
        retain_cached_terminal_graphics_for_visible_surfaces(
            scene,
            graphics_cache,
            capabilities,
            &mut resources.active_graphics,
        );
        resources.cleanup_stale(graphics_cache, capabilities)?;
        Ok(resources)
    }

    pub(super) fn activate_graphics(&mut self, current_graphics: BTreeSet<u64>) {
        self.active_graphics.extend(current_graphics);
    }

    pub(super) fn cleanup_stale_for_surface(
        &mut self,
        pane_id: Uuid,
        surface_id: Uuid,
        current_graphics: &BTreeSet<u64>,
        graphics_cache: &mut TerminalGraphicsCache,
        capabilities: TerminalRenderCapabilities,
    ) -> Result<bool> {
        cleanup_stale_terminal_graphics_for_surface(
            pane_id,
            surface_id,
            current_graphics,
            graphics_cache,
            capabilities,
            Some(&mut self.stats),
        )
    }

    #[allow(clippy::too_many_arguments)] // Centralizes terminal graphic reconciliation inputs at call sites.
    pub(super) fn queue_graphic_overlay(
        &mut self,
        pane_id: Uuid,
        surface_id: Uuid,
        instance_key: u64,
        graphic: &TerminalGraphicOverlay,
        graphics_cache: &mut TerminalGraphicsCache,
        capabilities: TerminalRenderCapabilities,
    ) -> Result<bool> {
        queue_terminal_graphic_overlay(
            pane_id,
            surface_id,
            instance_key,
            graphic,
            graphics_cache,
            capabilities,
            Some(&mut self.stats),
        )
    }

    pub(super) fn cleanup_stale(
        &mut self,
        graphics_cache: &mut TerminalGraphicsCache,
        capabilities: TerminalRenderCapabilities,
    ) -> Result<bool> {
        cleanup_stale_terminal_graphics(
            &self.active_graphics,
            graphics_cache,
            capabilities,
            Some(&mut self.stats),
        )
    }

    pub(super) fn finish(
        &mut self,
        graphics_cache: &mut TerminalGraphicsCache,
        capabilities: TerminalRenderCapabilities,
        render_stats: Option<&mut AttachSceneRenderStats>,
    ) -> Result<()> {
        self.cleanup_stale(graphics_cache, capabilities)?;
        if let Some(stats) = render_stats {
            self.stats.apply_to(stats);
        }
        Ok(())
    }
}

pub(super) fn begin_terminal_graphics_frame(
    scene: &AttachScene,
    graphics_cache: &mut TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
) -> Result<TerminalGraphicsFrameResources> {
    TerminalGraphicsFrameResources::begin(scene, graphics_cache, capabilities)
}

pub(super) fn finish_terminal_graphics_frame(
    terminal_graphics: &mut TerminalGraphicsFrameResources,
    graphics_cache: &mut TerminalGraphicsCache,
    capabilities: TerminalRenderCapabilities,
    render_stats: Option<&mut AttachSceneRenderStats>,
) -> Result<()> {
    terminal_graphics.finish(graphics_cache, capabilities, render_stats)
}
