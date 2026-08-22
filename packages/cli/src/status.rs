use bmux_appearance::RuntimeAppearance;
use bmux_config::{StatusBarConfig, StatusHintPolicy};
use bmux_plugin::{RenderStyle, RenderTextSpan};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;

/// Compatibility hitbox shape retained temporarily for legacy reducer removal.
/// Normal status rendering never populates these; window interaction is owned
/// by presentation-plugin retained regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachStatusTabHitbox {
    pub context_id: Uuid,
    pub start_col: u16,
    pub end_col: u16,
}

/// Compatibility status modules independent from plugin-owned windows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachStatusLine {
    pub rendered: String,
    pub spans: Vec<RenderTextSpan>,
    pub tab_hitboxes: Vec<AttachStatusTabHitbox>,
    pub drag_marker_col: Option<u16>,
    pub edit_cursor_col: Option<u16>,
}

#[allow(clippy::too_many_arguments)]
pub fn build_attach_status_line(
    width: u16,
    config: &StatusBarConfig,
    _runtime_appearance: &RuntimeAppearance,
    session_label: &str,
    session_count: usize,
    current_context_label: &str,
    mode_label: &str,
    role_label: &str,
    follow_label: Option<&str>,
    mode_modifier: Option<&str>,
    hint: &str,
) -> AttachStatusLine {
    if !config.enabled || width == 0 {
        return AttachStatusLine::default();
    }
    let mut segments = Vec::new();
    if config.show_session_name {
        segments.push(if session_count > 1 {
            format!("{session_label} ({session_count})")
        } else {
            session_label.to_string()
        });
    }
    if config.show_context_name {
        segments.push(current_context_label.to_string());
    }
    if config.show_mode {
        segments.push(mode_label.to_string());
        segments.extend(mode_modifier.map(str::to_string));
    }
    if config.show_role {
        segments.push(role_label.to_string());
    }
    if config.show_follow {
        segments.extend(follow_label.map(str::to_string));
    }
    if config.show_hint && hint_allowed(config.hint_policy, mode_label) && !hint.is_empty() {
        segments.push(hint.to_string());
    }
    let rendered = truncate_cells(&segments.join(" | "), usize::from(width));
    let spans = if rendered.is_empty() {
        Vec::new()
    } else {
        vec![RenderTextSpan::new(rendered.clone(), RenderStyle::new())]
    };
    AttachStatusLine {
        rendered,
        spans,
        ..AttachStatusLine::default()
    }
}

fn hint_allowed(policy: StatusHintPolicy, mode_label: &str) -> bool {
    match policy {
        StatusHintPolicy::Always => true,
        StatusHintPolicy::ScrollOnly => mode_label == "SCROLL",
        StatusHintPolicy::Never => false,
    }
}

fn truncate_cells(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    let mut output = String::new();
    let mut width = 0_usize;
    for ch in value.chars() {
        let next = width.saturating_add(UnicodeWidthChar::width(ch).unwrap_or(0));
        if next > max_width {
            break;
        }
        output.push(ch);
        width = next;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_compatibility_has_no_window_projection() {
        let line = build_attach_status_line(
            80,
            &StatusBarConfig::default(),
            &RuntimeAppearance::default(),
            "session",
            1,
            "context",
            "NORMAL",
            "write",
            None,
            None,
            "hint",
        );
        assert!(line.tab_hitboxes.is_empty());
    }
}
