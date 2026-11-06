//! Attach-local rendering of plugin-raised toast notifications.
//!
//! The server-side store (see `external.rs`) retains a bounded queue and the
//! companion receives it as a forwarded state snapshot. Expiry is evaluated
//! here against wall-clock time so replayed snapshots never resurrect expired
//! toasts; the companion task wakes at [`next_expiry_ms`] to republish.

use std::cell::Cell;

use bmux_plugin::ExtensionRect;
use bmux_plugin::component_viewport::ComponentViewport;
use bmux_plugin::layout::PluginLayoutId;
use bmux_plugin::surface::{PluginSurface, PluginSurfaceId, PluginSurfaceTarget};
use bmux_tab_bar_plugin_api::{notifications_list::Notification, status_items_list::Severity};
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::style::{Color, Style};
use bmux_tui_components::toast_stack::{
    ToastItem, ToastSeverity, ToastStackComponent, ToastStackPolicy, ToastStackState,
    ToastStackStyles,
};
use uuid::Uuid;

use super::{CompanionState, LAYOUT_ID, OWNER, Placement, external::notification_expired};

const SURFACE_UUID: Uuid = Uuid::from_u128(0x626d_7578_5f74_6162_5f73_7472_6970_0004);

/// Unexpired toasts in display order (newest first), bounded by `limit`.
pub fn visible(notifications: &[Notification], now_ms: u64, limit: usize) -> Vec<&Notification> {
    notifications
        .iter()
        .rev()
        .filter(|notification| !notification_expired(notification, now_ms))
        .take(limit)
        .collect()
}

/// Earliest wall-clock instant (Unix ms) at which a visible toast expires.
pub fn next_expiry_ms(notifications: &[Notification], now_ms: u64) -> Option<u64> {
    notifications
        .iter()
        .filter(|notification| !notification_expired(notification, now_ms))
        .map(|notification| {
            notification
                .created_at_ms
                .saturating_add(notification.ttl_ms)
        })
        .min()
}

const fn toast_severity(severity: Severity) -> ToastSeverity {
    match severity {
        Severity::Normal => ToastSeverity::Default,
        Severity::Info => ToastSeverity::Info,
        Severity::Success => ToastSeverity::Success,
        Severity::Warning => ToastSeverity::Warning,
        Severity::Critical => ToastSeverity::Error,
    }
}

/// Build the retained toast overlay surface, if any toast is visible.
pub fn surface(companion: &CompanionState, revision: u64, now_ms: u64) -> Option<PluginSurface> {
    let settings = &companion.settings.notifications;
    if !settings.enabled {
        return None;
    }
    let local = &companion.local_presentation;
    let visible = visible(&companion.notifications, now_ms, settings.max_visible);
    if visible.is_empty() || local.viewport_cols < 8 || local.viewport_rows < 3 {
        return None;
    }
    let ids = visible
        .iter()
        .map(|notification| notification.id.to_string())
        .collect::<Vec<_>>();
    let items = visible
        .iter()
        .zip(&ids)
        .map(|(notification, id)| {
            let item = ToastItem::new(id, &notification.title)
                .severity(toast_severity(notification.severity));
            notification
                .body
                .as_deref()
                .map_or(item, |body| item.body(body))
        })
        .collect::<Vec<_>>();
    let color = |value: &str, fallback| {
        let (r, g, b) = super::parse_hex_color(value).unwrap_or(fallback);
        Color::Rgb(r, g, b)
    };
    let foreground = color(&local.status_foreground, (220, 220, 220));
    let background = color(&local.status_background, (30, 30, 30));
    let styles = ToastStackStyles {
        default: Style::new().fg(foreground),
        body: Style::new().fg(foreground),
        surface: Style::new().fg(foreground).bg(background),
        ..ToastStackStyles::default()
    };
    let width = settings.width.min(local.viewport_cols.saturating_sub(2));
    let policy = ToastStackPolicy {
        width,
        max_visible: settings.max_visible,
        ..ToastStackPolicy::cards()
    };
    let bar_rows = companion
        .committed_projection
        .as_ref()
        .map_or(companion.settings.height, |projection| projection.height);
    let available_rows = local
        .viewport_rows
        .saturating_sub(bar_rows)
        .saturating_sub(1);
    if available_rows == 0 || width == 0 {
        return None;
    }
    let state = Cell::new(ToastStackState::default());
    let component = ToastStackComponent::new("toasts", &items, &state)
        .policy(policy)
        .styles(styles);
    let layout = component.layout(
        Constraints::loose(Size::new(width, available_rows)),
        &mut LayoutCx::new(),
    );
    let w = u16::try_from(layout.size.width).unwrap_or(width).min(width);
    let h = u16::try_from(layout.size.height)
        .unwrap_or(available_rows)
        .min(available_rows);
    let x = local.viewport_cols.saturating_sub(w).saturating_sub(1);
    let y = match companion.settings.placement {
        Placement::Top => bar_rows,
        Placement::Bottom => 0,
    };
    let viewport = ComponentViewport::new(layout, Rect::new(x, y, w, h), Point::new(0, 0))?;
    let painted = viewport.paint(&component);
    let mut surface = PluginSurface::layout(
        PluginSurfaceId::new(OWNER, "toasts", SURFACE_UUID),
        revision,
        PluginLayoutId::new(OWNER, LAYOUT_ID),
        bmux_plugin::component_render::buffer_render_ops(&painted.buffer),
    )
    .order(100, 2)
    .opaque(true);
    surface.target = PluginSurfaceTarget::Explicit(ExtensionRect::new(x, y, w, h));
    Some(surface)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(id: u128, created_at_ms: u64, ttl_ms: u64) -> Notification {
        Notification {
            id: Uuid::from_u128(id),
            owner: "example.native".into(),
            title: format!("toast {id}"),
            body: None,
            severity: Severity::Info,
            created_at_ms,
            ttl_ms,
        }
    }

    #[test]
    fn visible_toasts_are_newest_first_unexpired_and_bounded() {
        let queue = [
            notification(1, 0, 1_000),
            notification(2, 0, 5_000),
            notification(3, 100, 5_000),
            notification(4, 200, 5_000),
        ];
        let ids = visible(&queue, 1_000, 2)
            .into_iter()
            .map(|notification| notification.id.as_u128())
            .collect::<Vec<_>>();
        assert_eq!(ids, [4, 3]);
        assert_eq!(visible(&queue, 1_000, 8).len(), 3);
        assert_eq!(visible(&queue, 10_000, 8).len(), 0);
    }

    #[test]
    fn next_expiry_tracks_the_earliest_live_toast() {
        let queue = [notification(1, 0, 1_000), notification(2, 500, 1_000)];
        assert_eq!(next_expiry_ms(&queue, 0), Some(1_000));
        assert_eq!(next_expiry_ms(&queue, 1_000), Some(1_500));
        assert_eq!(next_expiry_ms(&queue, 1_500), None);
    }

    #[test]
    fn severities_map_onto_toast_stack_levels() {
        assert_eq!(toast_severity(Severity::Normal), ToastSeverity::Default);
        assert_eq!(toast_severity(Severity::Critical), ToastSeverity::Error);
        assert_eq!(toast_severity(Severity::Success), ToastSeverity::Success);
    }
}
