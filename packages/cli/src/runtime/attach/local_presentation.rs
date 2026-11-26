use bmux_plugin::surface::{
    PluginSurface, PluginSurfaceId, PluginSurfaceRegistry, PluginSurfaceSnapshot,
    PluginSurfaceTarget,
};
use bmux_plugin::{ExtensionRect, RenderOp, RenderStyle};
use bmux_presentation_scene::scene::{Anchor, Insets, Length, OverlayArea, OverlayPlacement, Size};
use uuid::Uuid;

const OWNER: &str = "bmux.attach_local";
const SURFACE_ID: &str = "notification";
/// The connection banner has its own owner: publishing one owner's snapshot
/// replaces all of that owner's surfaces, and the banner and status messages
/// change independently.
const BANNER_OWNER: &str = "bmux.attach_local.connection";
const BANNER_SURFACE_ID: &str = "connection-banner";
/// Retained compositor identities are unique across owners.
const BANNER_RETAINED_ID: Uuid = Uuid::from_u128(0x6d75_785f_636f_6e6e_6563_7469_6f6e_0001);

/// One attach-local row overlay: where it sits and what it draws.
struct LocalRow<'a> {
    owner: &'static str,
    surface_id: &'static str,
    retained_id: Uuid,
    anchor: Anchor,
    text: &'a str,
    /// The style of the row's blank cells.
    fill: RenderStyle,
    style: RenderStyle,
}

/// Show `message` on the bottom row of the area left after layout splits
/// (the host resolves where that is), or remove it.
pub fn publish_notification(
    registry: &PluginSurfaceRegistry,
    message: Option<&str>,
    cols: u16,
    rows: u16,
) {
    publish_row(
        registry,
        OWNER,
        message.map(|text| LocalRow {
            owner: OWNER,
            surface_id: SURFACE_ID,
            retained_id: Uuid::nil(),
            anchor: Anchor::Bottom,
            text,
            fill: RenderStyle::new(),
            style: RenderStyle::new().bold(),
        }),
        cols,
        rows,
    );
}

/// Show the connection banner on the top row of the area left after layout
/// splits, or remove it.
pub fn publish_banner(
    registry: &PluginSurfaceRegistry,
    banner: Option<&super::connection_banner::ConnectionBanner>,
    cols: u16,
    rows: u16,
) {
    let text = banner.map(|banner| super::connection_banner::row_text(banner, cols));
    publish_row(
        registry,
        BANNER_OWNER,
        banner.zip(text.as_deref()).map(|(banner, text)| LocalRow {
            owner: BANNER_OWNER,
            surface_id: BANNER_SURFACE_ID,
            retained_id: BANNER_RETAINED_ID,
            anchor: Anchor::Top,
            text,
            fill: banner.severity().style(),
            style: banner.severity().style(),
        }),
        cols,
        rows,
    );
}

/// The row the connection banner takes on a `cols` x `rows` screen with
/// `layouts` reserved: the top row of the area they leave.
pub fn banner_row(
    layouts: &bmux_plugin::layout::PluginLayoutRegistry,
    cols: u16,
    rows: u16,
) -> u16 {
    bmux_plugin::layout::resolve_plugin_layout(
        ExtensionRect::new(0, 0, cols, rows),
        (1, 1),
        &layouts.requests(),
    )
    .map_or(0, |layout| layout.remaining.y)
}

fn publish_row(
    registry: &PluginSurfaceRegistry,
    owner: &'static str,
    row: Option<LocalRow<'_>>,
    cols: u16,
    rows: u16,
) {
    let previous = registry.owner_snapshot(owner);
    let revision = previous
        .as_ref()
        .map_or(1, |snapshot| snapshot.revision.saturating_add(1).max(1));
    let surfaces = row
        .filter(|row| !row.text.is_empty() && cols > 0 && rows > 0)
        .map(|row| PluginSurface {
            items: Vec::new(),
            id: PluginSurfaceId::new(row.owner, row.surface_id, row.retained_id),
            revision,
            target: PluginSurfaceTarget::Overlay(OverlayPlacement::Anchored {
                area: OverlayArea::Remaining,
                anchor: row.anchor,
                size: Size {
                    width: Length::Fill,
                    height: Length::Cells(1),
                },
                margin: Insets::default(),
            }),
            clip_rect: None,
            metadata: None,
            interactive_regions: Vec::new(),
            accepts_input: false,
            layer: 40,
            z: i32::MAX,
            opaque: true,
            modal: false,
            visible: true,
            ops: vec![
                RenderOp::clear_rect(ExtensionRect::new(0, 0, cols, 1), row.fill),
                RenderOp::text_run(0, 0, row.text, row.style),
            ],
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
    let _ = registry.publish(owner, PluginSurfaceSnapshot { revision, surfaces });
}

pub fn uninstall(registry: &PluginSurfaceRegistry) {
    registry.remove_owner(OWNER);
    registry.remove_owner(BANNER_OWNER);
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
        LayoutEdge, LayoutExtent, PluginLayoutId, PluginLayoutRegistry, PluginLayoutRequest,
        PluginLayoutSnapshot, resolve_plugin_layout,
    };
    use std::collections::BTreeMap;

    /// Where the host places the notification with `layouts` reserved.
    fn resolved_rect(
        layouts: &PluginLayoutRegistry,
        surfaces: &PluginSurfaceRegistry,
    ) -> ExtensionRect {
        let viewport = ExtensionRect::new(0, 0, 80, 24);
        let layout = resolve_plugin_layout(viewport, (1, 1), &layouts.requests()).unwrap();
        let rects = bmux_attach_pipeline::resolve_plugin_surface_rects(
            &surfaces.surfaces(),
            &BTreeMap::new(),
            layout.remaining,
            bmux_attach_pipeline::DamageRect::new(0, 0, 80, 24),
        );
        *rects.get(&0).expect("notification resolves")
    }

    #[test]
    fn notification_uses_generic_overlay_surface_and_removes_cleanly() {
        let layouts = PluginLayoutRegistry::new(4);
        let surfaces = PluginSurfaceRegistry::new(4);
        publish_notification(&surfaces, Some("saved"), 80, 24);
        let snapshot = surfaces
            .owner_snapshot(OWNER)
            .expect("notification snapshot");
        assert_eq!(snapshot.surfaces.len(), 1);
        assert_eq!(
            resolved_rect(&layouts, &surfaces),
            ExtensionRect::new(0, 23, 80, 1)
        );

        publish_notification(&surfaces, None, 80, 24);
        let removed = surfaces
            .owner_snapshot(OWNER)
            .expect("empty replacement snapshot");
        assert_eq!(
            removed.surfaces,
            [] as [bmux_plugin::surface::PluginSurface; 0]
        );
        uninstall(&surfaces);
    }

    #[test]
    fn the_connection_banner_takes_the_top_row_beside_a_notification() {
        use super::super::connection_banner::ConnectionBanner;
        let layouts = PluginLayoutRegistry::new(4);
        let surfaces = PluginSurfaceRegistry::new(4);
        publish_notification(&surfaces, Some("saved"), 80, 24);
        publish_banner(&surfaces, Some(&ConnectionBanner::Degraded), 80, 24);
        let viewport = ExtensionRect::new(0, 0, 80, 24);
        let layout = resolve_plugin_layout(viewport, (1, 1), &layouts.requests()).unwrap();
        let all = surfaces.surfaces();
        assert_eq!(all.len(), 2, "neither replaces the other");
        let rects = bmux_attach_pipeline::resolve_plugin_surface_rects(
            &all,
            &BTreeMap::new(),
            layout.remaining,
            bmux_attach_pipeline::DamageRect::new(0, 0, 80, 24),
        );
        let banner = all
            .iter()
            .position(|surface| surface.id.owner_plugin_id == BANNER_OWNER)
            .expect("banner surface");
        assert_eq!(rects.get(&banner), Some(&ExtensionRect::new(0, 0, 80, 1)));
        assert_eq!(banner_row(&layouts, 80, 24), 0);

        publish_banner(&surfaces, None, 80, 24);
        assert_eq!(surfaces.surfaces().len(), 1, "the notification stays");
        uninstall(&surfaces);
        assert_eq!(
            surfaces.surfaces(),
            [] as [bmux_plugin::surface::PluginSurface; 0]
        );
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

        publish_notification(&surfaces, Some("saved"), 80, 24);
        assert_eq!(
            resolved_rect(&layouts, &surfaces),
            ExtensionRect::new(0, 22, 80, 1)
        );

        uninstall(&surfaces);
    }
}
