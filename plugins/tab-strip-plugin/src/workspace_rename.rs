use super::{
    AttachInputEvent, AttachInputResult, CompanionHandle, CompanionState, Instant, Uuid,
    command_invocation, republish_companion, workspaces_commands, workspaces_state,
};

pub fn invocation(id: Uuid, name: String) -> Option<super::AttachInputServiceInvocation> {
    command_invocation(
        // Keep typed result interpretation in the plugin, not generic attach dispatch.
        bmux_plugin::AttachInputEndpoint {
            capability: "bmux.tab_strip.input".to_string(),
            interface_id: "presentation-input".to_string(),
            operation: "rename-workspace".to_string(),
        },
        &workspaces_commands::client::RenameWorkspaceRequest {
            selector: workspaces_state::WorkspaceSelector {
                id: Some(id),
                name: None,
            },
            name,
        },
    )
}

fn begin(companion: &mut CompanionState, id: Uuid, col: u16, row: u16) -> bool {
    companion.last_left_click = None;
    let now = Instant::now();
    let interval = std::time::Duration::from_millis(companion.local_presentation.double_click_ms);
    let double = !interval.is_zero()
        && companion
            .last_workspace_click
            .is_some_and(|(last, x, y, at)| {
                last == id && x == col && y == row && now.saturating_duration_since(at) <= interval
            });
    companion.last_workspace_click = if double {
        None
    } else {
        Some((id, col, row, now))
    };
    if double {
        companion.editing_window_id = None;
        companion.menu_window_id = None;
        companion.pointer_source = None;
        companion.pointer_moved = false;
        companion.drag_target = None;
        companion.editing_workspace_id = Some(id);
        companion.edit_buffer = bmux_text_edit::TextEditBuffer::from_text(
            companion.workspace_label.clone().unwrap_or_default(),
        )
        .into();
        companion.edit_buffer.select_all();
    }
    double
}

#[allow(clippy::significant_drop_tightening)] // Serialize the gesture and its retained publication.
pub fn handle_pointer(
    owner: &CompanionHandle,
    event: &AttachInputEvent,
) -> Option<AttachInputResult> {
    let id = event
        .hook_id
        .strip_prefix("bmux.tab_strip:strip:workspace:")
        .and_then(|id| Uuid::parse_str(id).ok())?;
    let mut guard = owner.lock().ok()?;
    let companion = guard.as_mut()?;
    if companion.workspace_id != Some(id) {
        return None;
    }
    let editing = event.phase == "down"
        && event.button.as_deref() == Some("left")
        && begin(
            companion,
            id,
            event.col.unwrap_or_default(),
            event.row.unwrap_or_default(),
        );
    Some(AttachInputResult {
        consumed: true,
        release_capture: editing,
        capture_keyboard: if editing {
            vec!["*".to_string()]
        } else {
            Vec::new()
        },
        dirty: editing && republish_companion(companion),
        ..AttachInputResult::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_click_selects_full_name_and_pins_identity() {
        let mut companion = CompanionState::new(super::super::Settings::default());
        companion.local_presentation.double_click_ms = 500;
        companion.workspace_label = Some("a long workspace 界 name".to_string());
        let id = Uuid::from_u128(12);
        assert!(!begin(&mut companion, id, 2, 0));
        assert!(begin(&mut companion, id, 2, 0));
        assert_eq!(companion.editing_workspace_id, Some(id));
        assert_eq!(companion.edit_buffer.text(), "a long workspace 界 name");
        assert!(companion.edit_buffer.selection().is_some());
        let invocation = invocation(id, "renamed".to_string()).unwrap();
        let request: workspaces_commands::client::RenameWorkspaceRequest =
            bmux_plugin_sdk::decode_service_message(&invocation.payload).unwrap();
        assert_eq!(request.selector.id, Some(id));
        assert_eq!(request.name, "renamed");
    }

    #[test]
    fn disabled_timing_and_different_targets_do_not_edit() {
        let mut companion = CompanionState::new(super::super::Settings::default());
        companion.local_presentation.double_click_ms = 0;
        assert!(!begin(&mut companion, Uuid::nil(), 2, 0));
        assert!(!begin(&mut companion, Uuid::nil(), 2, 0));
        companion.local_presentation.double_click_ms = 500;
        assert!(!begin(&mut companion, Uuid::from_u128(1), 2, 0));
        assert!(!begin(&mut companion, Uuid::from_u128(1), 3, 0));
        assert!(companion.editing_workspace_id.is_none());
    }
}
