use bmux_plugin::layout::{PluginLayoutRegistry, resolve_plugin_layout};
use bmux_plugin::surface::{
    PluginSurface, PluginSurfaceId, PluginSurfaceRegistry, PluginSurfaceSnapshot,
    PluginSurfaceTarget,
};
use bmux_plugin::{ExtensionRect, RenderOp, RenderStyle};
use uuid::Uuid;

const OWNER: &str = "bmux.attach_local";
const SURFACE_ID: &str = "notification";

pub fn publish_notification(
    layouts: &PluginLayoutRegistry,
    registry: &PluginSurfaceRegistry,
    message: Option<&str>,
    cols: u16,
    rows: u16,
) {
    let previous = registry.owner_snapshot(OWNER);
    let revision = previous
        .as_ref()
        .map_or(1, |snapshot| snapshot.revision.saturating_add(1).max(1));
    let surfaces = message
        .filter(|message| !message.is_empty() && cols > 0 && rows > 0)
        .map(|message| {
            let viewport = ExtensionRect::new(0, 0, cols, rows);
            let remaining = resolve_plugin_layout(viewport, (1, 1), &layouts.requests())
                .map_or(viewport, |layout| layout.remaining);
            let row = remaining.y.saturating_add(remaining.h).saturating_sub(1);
            let rect = ExtensionRect::new(remaining.x, row, remaining.w, 1);
            PluginSurface {
                id: PluginSurfaceId::new(OWNER, SURFACE_ID, Uuid::nil()),
                revision,
                target: PluginSurfaceTarget::Explicit(rect),
                clip_rect: Some(ExtensionRect::new(0, 0, remaining.w, 1)),
                interactive_regions: Vec::new(),
                accepts_input: false,
                layer: 40,
                z: i32::MAX,
                opaque: true,
                modal: false,
                visible: true,
                ops: vec![
                    RenderOp::clear_rect(rect, RenderStyle::new()),
                    RenderOp::text_run(0, 0, message, RenderStyle::new().bold()),
                ],
            }
        })
        .into_iter()
        .collect::<Vec<_>>();

    if previous
        .as_ref()
        .is_some_and(|snapshot| snapshots_match(&snapshot.surfaces, &surfaces))
    {
        return;
    }
    if surfaces.is_empty() && previous.is_none() {
        return;
    }
    let _ = registry.publish(OWNER, PluginSurfaceSnapshot { revision, surfaces });
}

pub fn uninstall(registry: &PluginSurfaceRegistry) {
    registry.remove_owner(OWNER);
}

fn snapshots_match(previous: &[PluginSurface], current: &[PluginSurface]) -> bool {
    previous.len() == current.len()
        && previous.iter().zip(current).all(|(left, right)| {
            left.id == right.id
                && left.target == right.target
                && left.clip_rect == right.clip_rect
                && left.interactive_regions == right.interactive_regions
                && left.accepts_input == right.accepts_input
                && left.layer == right.layer
                && left.z == right.z
                && left.opaque == right.opaque
                && left.modal == right.modal
                && left.visible == right.visible
                && left.ops == right.ops
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use bmux_plugin::layout::{
        LayoutEdge, LayoutExtent, PluginLayoutId, PluginLayoutRequest, PluginLayoutSnapshot,
    };

    #[test]
    fn notification_uses_generic_explicit_surface_and_removes_cleanly() {
        let layouts = PluginLayoutRegistry::new(4);
        let surfaces = PluginSurfaceRegistry::new(4);
        publish_notification(&layouts, &surfaces, Some("saved"), 80, 24);
        let snapshot = surfaces
            .owner_snapshot(OWNER)
            .expect("notification snapshot");
        assert_eq!(snapshot.surfaces.len(), 1);
        assert_eq!(
            snapshot.surfaces[0].target,
            PluginSurfaceTarget::Explicit(ExtensionRect::new(0, 23, 80, 1))
        );

        publish_notification(&layouts, &surfaces, None, 80, 24);
        let removed = surfaces
            .owner_snapshot(OWNER)
            .expect("empty replacement snapshot");
        assert!(removed.surfaces.is_empty());
        uninstall(&surfaces);
    }

    #[test]
    fn notification_avoids_bottom_layout_reservations() {
        let layouts = PluginLayoutRegistry::new(4);
        let surfaces = PluginSurfaceRegistry::new(4);
        layouts
            .publish(
                "test.bottom",
                PluginLayoutSnapshot {
                    revision: 1,
                    requests: vec![PluginLayoutRequest::split(
                        PluginLayoutId::new("test.bottom", "bar"),
                        0,
                        LayoutEdge::Bottom,
                        LayoutExtent::Cells(1),
                    )],
                },
            )
            .expect("bottom layout should publish");

        publish_notification(&layouts, &surfaces, Some("saved"), 80, 24);
        let snapshot = surfaces
            .owner_snapshot(OWNER)
            .expect("notification snapshot");
        assert_eq!(
            snapshot.surfaces[0].target,
            PluginSurfaceTarget::Explicit(ExtensionRect::new(0, 22, 80, 1))
        );

        uninstall(&surfaces);
    }
}
