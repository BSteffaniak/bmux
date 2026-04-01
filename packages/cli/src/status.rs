use bmux_config::{StatusBarConfig, StatusHintPolicy};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;

pub struct AttachTab {
    pub(crate) label: String,
    pub(crate) active: bool,
    pub(crate) context_id: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct AttachStatusTabHitbox {
    pub(crate) start_col: u16,
    pub(crate) end_col: u16,
    pub(crate) context_id: Uuid,
}

#[derive(Clone, Debug)]
pub struct AttachStatusLine {
    pub(crate) rendered: String,
    pub(crate) tab_hitboxes: Vec<AttachStatusTabHitbox>,
}

pub fn build_attach_status_line(
    width: u16,
    config: &StatusBarConfig,
    session_label: &str,
    session_count: usize,
    current_context_label: &str,
    tabs: &[AttachTab],
    tab_position_label: Option<&str>,
    mode_label: &str,
    role_label: &str,
    follow_label: Option<&str>,
    hint: &str,
) -> AttachStatusLine {
    let mut left = String::new();
    let mut tab_hitboxes = Vec::new();

    append_tabs(&mut left, &mut tab_hitboxes, config, tabs);

    if config.show_session_name {
        append_segment(
            &mut left,
            &config.segment_separator,
            &format!("session:{session_label} ({session_count})"),
        );
    }
    if config.show_context_name {
        append_segment(
            &mut left,
            &config.segment_separator,
            &format!("ctx:{current_context_label}"),
        );
    }

    let mut right_segments = Vec::new();
    if config.show_mode {
        right_segments.push(format!("{mode_label}"));
    }
    if config.show_role {
        right_segments.push(role_label.to_string());
    }
    if let Some(tab_position_label) = tab_position_label {
        right_segments.push(tab_position_label.to_string());
    }
    if config.show_follow
        && let Some(follow) = follow_label
    {
        right_segments.push(follow.to_string());
    }
    if config.show_hint && hint_allowed(config.hint_policy, mode_label) {
        right_segments.push(hint.to_string());
    }
    let right = right_segments.join(&config.segment_separator);

    let rendered = compose_status_line(width, &left, &right);
    clamp_hitboxes_to_width(&mut tab_hitboxes, width);

    AttachStatusLine {
        rendered,
        tab_hitboxes,
    }
}

fn hint_allowed(policy: StatusHintPolicy, mode_label: &str) -> bool {
    match policy {
        StatusHintPolicy::Always => true,
        StatusHintPolicy::ScrollOnly => mode_label == "SCROLL",
        StatusHintPolicy::Never => false,
    }
}

fn append_tabs(
    out: &mut String,
    hitboxes: &mut Vec<AttachStatusTabHitbox>,
    config: &StatusBarConfig,
    tabs: &[AttachTab],
) {
    if tabs.is_empty() {
        out.push_str("[no-tabs]");
        return;
    }

    let max_tabs = config.max_tabs.max(1);
    let visible = tabs.iter().take(max_tabs).collect::<Vec<_>>();
    let hidden = tabs.len().saturating_sub(visible.len());
    let mut col = 0usize;

    for (index, tab) in visible.iter().enumerate() {
        if index > 0 {
            out.push_str(&config.tab_separator);
            col = col.saturating_add(display_width(&config.tab_separator));
        }
        let label = truncate_cells(&tab.label, config.tab_label_max_width.max(1));
        let indexed = if config.show_tab_index {
            format!("{}:{}", index + 1, label)
        } else {
            label
        };
        let token = if tab.active {
            format!(
                "{}{}{}",
                config.active_tab_prefix, indexed, config.active_tab_suffix
            )
        } else {
            format!(
                "{}{}{}",
                config.inactive_tab_prefix, indexed, config.inactive_tab_suffix
            )
        };
        out.push_str(&token);
        let token_width = display_width(&token);
        if let Some(context_id) = tab.context_id {
            hitboxes.push(AttachStatusTabHitbox {
                start_col: col as u16,
                end_col: col.saturating_add(token_width.saturating_sub(1)) as u16,
                context_id,
            });
        }
        col = col.saturating_add(token_width);
    }

    if hidden > 0 {
        out.push_str(&config.tab_separator);
        out.push_str(&format!("{}{}", config.tab_overflow_marker, hidden));
    }
}

fn append_segment(out: &mut String, separator: &str, value: &str) {
    if out.is_empty() {
        out.push_str(value);
    } else {
        out.push_str(separator);
        out.push_str(value);
    }
}

fn compose_status_line(width: u16, left: &str, right: &str) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }

    if right.is_empty() {
        return pad_or_truncate(left, width);
    }

    let right_width = display_width(right);
    if right_width >= width {
        return truncate_cells(right, width);
    }

    let available_left = width.saturating_sub(right_width + 1);
    let left_trimmed = truncate_cells(left, available_left);
    let left_width = display_width(&left_trimmed);
    let spacer = " ".repeat(width.saturating_sub(left_width + right_width));
    format!("{left_trimmed}{spacer}{right}")
}

fn pad_or_truncate(value: &str, width: usize) -> String {
    let mut rendered = truncate_cells(value, width);
    let current = display_width(&rendered);
    if current < width {
        rendered.push_str(&" ".repeat(width - current));
    }
    rendered
}

fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

fn truncate_cells(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut width = 0usize;
    let mut out = String::new();
    for ch in value.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width.saturating_add(char_width) > max_width {
            break;
        }
        out.push(ch);
        width = width.saturating_add(char_width);
    }
    out
}

fn clamp_hitboxes_to_width(hitboxes: &mut Vec<AttachStatusTabHitbox>, width: u16) {
    if width == 0 {
        hitboxes.clear();
        return;
    }
    let max = width - 1;
    hitboxes.retain_mut(|entry| {
        if entry.start_col > max {
            return false;
        }
        entry.end_col = entry.end_col.min(max);
        entry.start_col <= entry.end_col
    });
}
