use bmux_config::{
    StatusAlignActive, StatusBarConfig, StatusBarPreset, StatusDensity, StatusHintPolicy,
    StatusOverflowStyle, StatusSeparatorSet,
};
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
    if !config.enabled {
        return AttachStatusLine {
            rendered: String::new(),
            tab_hitboxes: Vec::new(),
        };
    }
    let style = StatusRenderStyle::from_config(config);
    let mut left = String::new();
    let mut tab_hitboxes = Vec::new();
    left.push_str(&" ".repeat(config.layout.left_padding));

    append_tabs(&mut left, &mut tab_hitboxes, config, tabs, &style);

    if config.show_session_name {
        append_segment(
            &mut left,
            &style.module_separator,
            &format!("session:{session_label} ({session_count})"),
        );
    }
    if config.show_context_name {
        append_segment(
            &mut left,
            &style.module_separator,
            &format!("ctx:{current_context_label}"),
        );
    }

    let mut right_segments = Vec::new();
    if config.show_mode {
        right_segments.push(style.badge(mode_label));
    }
    if config.show_role {
        right_segments.push(style.badge(role_label));
    }
    if let Some(tab_position_label) = tab_position_label {
        right_segments.push(style.badge(tab_position_label));
    }
    if config.show_follow
        && let Some(follow) = follow_label
    {
        right_segments.push(style.badge(follow));
    }
    if config.show_hint && hint_allowed(config.hint_policy, mode_label) {
        right_segments.push(style.badge(hint));
    }
    let mut right = right_segments.join(&style.module_separator);
    if config.layout.right_padding > 0 {
        right.push_str(&" ".repeat(config.layout.right_padding));
    }

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
    style: &StatusRenderStyle,
) {
    if tabs.is_empty() {
        out.push_str(&style.empty_tabs_label);
        return;
    }

    let max_tabs = config.max_tabs.max(1);
    let (visible, hidden_left, hidden_right) = visible_tabs_for_layout(tabs, max_tabs, config);
    let mut col = 0usize;

    if hidden_left > 0 {
        let marker = style.overflow_marker(hidden_left);
        out.push_str(&marker);
        out.push_str(&style.tab_separator);
        col = col
            .saturating_add(display_width(&marker))
            .saturating_add(display_width(&style.tab_separator));
    }

    for (index, tab) in visible.iter().enumerate() {
        if index > 0 {
            out.push_str(&style.tab_separator);
            col = col.saturating_add(display_width(&style.tab_separator));
        }
        let label = truncate_cells(&tab.label, config.tab_label_max_width.max(1));
        let global_index = tabs
            .iter()
            .position(|entry| entry.context_id == tab.context_id)
            .unwrap_or(index);
        let indexed = if config.show_tab_index {
            format!("{}:{}", global_index + 1, label)
        } else {
            label
        };
        let token = if tab.active {
            style.active_tab(&indexed)
        } else {
            style.inactive_tab(&indexed)
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

    if hidden_right > 0 {
        out.push_str(&style.tab_separator);
        out.push_str(&style.overflow_marker(hidden_right));
    }
}

fn visible_tabs_for_layout<'a>(
    tabs: &'a [AttachTab],
    max_tabs: usize,
    config: &StatusBarConfig,
) -> (Vec<&'a AttachTab>, usize, usize) {
    if tabs.len() <= max_tabs {
        return (tabs.iter().collect(), 0, 0);
    }
    let active_index = tabs.iter().position(|tab| tab.active).unwrap_or(0);
    let start = match config.layout.align_active {
        StatusAlignActive::KeepVisible => active_index.saturating_sub(max_tabs.saturating_sub(1)),
        StatusAlignActive::FocusBias => active_index.saturating_sub(max_tabs / 2),
    }
    .min(tabs.len().saturating_sub(max_tabs));
    let end = (start + max_tabs).min(tabs.len());
    (
        tabs[start..end].iter().collect(),
        start,
        tabs.len().saturating_sub(end),
    )
}

struct StatusRenderStyle {
    tab_separator: String,
    module_separator: String,
    active_prefix: &'static str,
    active_suffix: &'static str,
    inactive_prefix: &'static str,
    inactive_suffix: &'static str,
    empty_tabs_label: String,
    overflow_left: &'static str,
    overflow_right: &'static str,
    overflow_count_prefix: &'static str,
    badge_left: &'static str,
    badge_right: &'static str,
    overflow_style: StatusOverflowStyle,
}

impl StatusRenderStyle {
    fn from_config(config: &StatusBarConfig) -> Self {
        let use_ascii = config.style.force_ascii;
        let separators =
            if use_ascii || matches!(config.style.separator_set, StatusSeparatorSet::Ascii) {
                ("|", "|", "<", ">")
            } else if matches!(config.style.separator_set, StatusSeparatorSet::Plain) {
                ("|", "|", "<", ">")
            } else if config.style.prefer_unicode {
                ("", "", "◀", "▶")
            } else {
                ("|", "|", "<", ">")
            };
        let gap = " ".repeat(match config.layout.density {
            StatusDensity::Compact => 0,
            StatusDensity::Cozy => config.layout.tab_gap.max(1),
        });
        let module_gap = " ".repeat(match config.layout.density {
            StatusDensity::Compact => 0,
            StatusDensity::Cozy => config.layout.module_gap.max(1),
        });
        let (
            active_prefix,
            active_suffix,
            inactive_prefix,
            inactive_suffix,
            badge_left,
            badge_right,
        ) = match config.preset {
            StatusBarPreset::TabRail => ("[", "]", " ", " ", "{", "}"),
            StatusBarPreset::Minimal => ("", "", "", "", "", ""),
            StatusBarPreset::Classic => ("(", ")", " ", " ", "[", "]"),
        };
        Self {
            tab_separator: if gap.is_empty() {
                separators.0.to_string()
            } else {
                format!("{gap}{}{gap}", separators.0)
            },
            module_separator: if module_gap.is_empty() {
                separators.1.to_string()
            } else {
                format!("{module_gap}{}{module_gap}", separators.1)
            },
            active_prefix,
            active_suffix,
            inactive_prefix,
            inactive_suffix,
            empty_tabs_label: "[no tabs]".to_string(),
            overflow_left: separators.2,
            overflow_right: separators.3,
            overflow_count_prefix: "+",
            badge_left,
            badge_right,
            overflow_style: config.layout.overflow_style,
        }
    }

    fn active_tab(&self, label: &str) -> String {
        format!("{}{}{}", self.active_prefix, label, self.active_suffix)
    }

    fn inactive_tab(&self, label: &str) -> String {
        format!("{}{}{}", self.inactive_prefix, label, self.inactive_suffix)
    }

    fn overflow_marker(&self, hidden: usize) -> String {
        match self.overflow_style {
            StatusOverflowStyle::Count => format!("{}{hidden}", self.overflow_count_prefix),
            StatusOverflowStyle::Arrows => {
                format!("{}{}{}", self.overflow_left, hidden, self.overflow_right)
            }
        }
    }

    fn badge(&self, value: &str) -> String {
        format!("{}{}{}", self.badge_left, value, self.badge_right)
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
