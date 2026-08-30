use bmux_attach_view_protocol::AttachLocalPresentationSnapshot;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    ActiveAlignment, Density, HintPolicy, OverflowStyle, Preset, SeparatorSet, Settings,
    windows_list,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Base,
    ActiveTab,
    InactiveTab,
    HoveredActiveTab,
    HoveredInactiveTab,
    Mode,
    Module,
    Overflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedSegment {
    pub(super) text: String,
    pub(super) kind: SegmentKind,
    pub(super) window_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedBar {
    pub(super) segments: Vec<ProjectedSegment>,
}

impl ProjectedBar {
    #[cfg(test)]
    fn plain_text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }
}

struct TabToken {
    text: String,
    width: usize,
    active: bool,
    hovered: bool,
    window_id: Uuid,
}

struct TabWindow {
    start: usize,
    end: usize,
}

struct RenderStyle {
    tab_separator: String,
    module_separator: String,
    active_prefix: &'static str,
    active_suffix: &'static str,
    inactive_prefix: &'static str,
    inactive_suffix: &'static str,
    badge_left: &'static str,
    badge_right: &'static str,
    overflow_left: &'static str,
    overflow_right: &'static str,
}

impl RenderStyle {
    fn from_settings(settings: &Settings) -> Self {
        let separators = if settings.force_ascii
            || matches!(
                settings.separator_set,
                SeparatorSet::Ascii | SeparatorSet::Plain
            ) {
            ("|", "|", "<", ">")
        } else if settings.prefer_unicode {
            ("", "", "◀", "▶")
        } else {
            ("|", "|", "<", ">")
        };
        let gap = " ".repeat(match settings.density {
            Density::Compact => 0,
            Density::Cozy => settings.tab_gap.max(1),
        });
        let module_gap = " ".repeat(match settings.density {
            Density::Compact => 0,
            Density::Cozy => settings.module_gap.max(1),
        });
        let (
            active_prefix,
            active_suffix,
            inactive_prefix,
            inactive_suffix,
            badge_left,
            badge_right,
        ) = match settings.preset {
            Preset::TabRail => (" ", " ", " ", " ", " ", " "),
            Preset::Minimal => ("", "", "", "", "", ""),
            Preset::Classic => ("(", ")", " ", " ", "[", "]"),
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
            badge_left,
            badge_right,
            overflow_left: separators.2,
            overflow_right: separators.3,
        }
    }

    fn tab(&self, label: &str, active: bool) -> String {
        if active {
            format!("{}{label}{}", self.active_prefix, self.active_suffix)
        } else {
            format!("{}{label}{}", self.inactive_prefix, self.inactive_suffix)
        }
    }

    fn badge(&self, text: &str) -> String {
        format!("{}{text}{}", self.badge_left, self.badge_right)
    }

    fn overflow(&self, hidden: usize, style: OverflowStyle) -> String {
        match style {
            OverflowStyle::Count => format!("+{hidden}"),
            OverflowStyle::Arrows => {
                format!("{}{hidden}{}", self.overflow_left, self.overflow_right)
            }
        }
    }
}

pub fn project_bar(
    settings: &Settings,
    windows: &[windows_list::WindowListEntry],
    local: &AttachLocalPresentationSnapshot,
    hovered_window_id: Option<Uuid>,
) -> ProjectedBar {
    let style = RenderStyle::from_settings(settings);
    let right = right_segments(settings, local, &style);
    let right_width = segments_width(&right);
    let tail = left_tail(settings, local, &style);
    let tail_width = segments_width(&tail);
    let width = if local.viewport_cols == 0 {
        usize::from(u16::MAX)
    } else {
        usize::from(local.viewport_cols)
    };
    let tab_budget = width
        .saturating_sub(right_width)
        .saturating_sub(usize::from(!right.is_empty()))
        .saturating_sub(settings.left_padding)
        .saturating_sub(settings.right_padding)
        .saturating_sub(tail_width);
    let tokens = windows
        .iter()
        .enumerate()
        .map(|(index, window)| {
            let label = render_tab_template(settings, window, index, local);
            let text = style.tab(&label, window.active);
            TabToken {
                width: UnicodeWidthStr::width(text.as_str()),
                text,
                active: window.active,
                hovered: settings.hover_highlight && hovered_window_id == Some(window.id),
                window_id: window.id,
            }
        })
        .collect::<Vec<_>>();
    let window = visible_tabs_for_layout(&tokens, settings, &style, tab_budget);
    let mut left = vec![ProjectedSegment {
        text: " ".repeat(settings.left_padding),
        kind: SegmentKind::Base,
        window_id: None,
    }];
    if tokens.is_empty() {
        left.push(ProjectedSegment {
            text: "[no tabs]".to_string(),
            kind: SegmentKind::Base,
            window_id: None,
        });
    } else {
        append_tabs(&mut left, &tokens, &window, settings, &style);
    }
    left.extend(tail);

    let mut segments = left;
    let left_width = segments_width(&segments);
    let spacer = width
        .saturating_sub(settings.right_padding)
        .saturating_sub(right_width)
        .saturating_sub(left_width);
    if spacer > 0 {
        segments.push(ProjectedSegment {
            text: " ".repeat(spacer),
            kind: SegmentKind::Base,
            window_id: None,
        });
    }
    segments.extend(right);
    if settings.right_padding > 0 {
        segments.push(ProjectedSegment {
            text: " ".repeat(settings.right_padding),
            kind: SegmentKind::Base,
            window_id: None,
        });
    }
    truncate_segments(&mut segments, width);
    let current_width = segments_width(&segments);
    if current_width < width {
        segments.push(ProjectedSegment {
            text: " ".repeat(width - current_width),
            kind: SegmentKind::Base,
            window_id: None,
        });
    }
    ProjectedBar { segments }
}

fn append_tabs(
    output: &mut Vec<ProjectedSegment>,
    tokens: &[TabToken],
    window: &TabWindow,
    settings: &Settings,
    style: &RenderStyle,
) {
    let hidden_left = window.start;
    let hidden_right = tokens.len().saturating_sub(window.end);
    if hidden_left > 0 {
        output.push(ProjectedSegment {
            text: style.overflow(hidden_left, settings.overflow_style),
            kind: SegmentKind::Overflow,
            window_id: None,
        });
        push_separator(output, &style.tab_separator);
    }
    for (offset, token) in tokens[window.start..window.end].iter().enumerate() {
        if offset > 0 {
            push_separator(output, &style.tab_separator);
        }
        output.push(ProjectedSegment {
            text: token.text.clone(),
            kind: match (token.active, token.hovered) {
                (true, true) => SegmentKind::HoveredActiveTab,
                (true, false) => SegmentKind::ActiveTab,
                (false, true) => SegmentKind::HoveredInactiveTab,
                (false, false) => SegmentKind::InactiveTab,
            },
            window_id: Some(token.window_id),
        });
    }
    if hidden_right > 0 {
        push_separator(output, &style.tab_separator);
        output.push(ProjectedSegment {
            text: style.overflow(hidden_right, settings.overflow_style),
            kind: SegmentKind::Overflow,
            window_id: None,
        });
    }
}

fn left_tail(
    settings: &Settings,
    local: &AttachLocalPresentationSnapshot,
    style: &RenderStyle,
) -> Vec<ProjectedSegment> {
    let mut values = Vec::new();
    if settings.show_session_name
        && let Some(label) = local.session_label.as_deref()
    {
        values.push(format!("session:{label} ({})", local.session_count));
    }
    if settings.show_context_name
        && let Some(label) = local.context_label.as_deref()
    {
        values.push(format!("ctx:{label}"));
    }
    values_to_segments(values, SegmentKind::Module, &style.module_separator)
}

fn right_segments(
    settings: &Settings,
    local: &AttachLocalPresentationSnapshot,
    style: &RenderStyle,
) -> Vec<ProjectedSegment> {
    let mut values = Vec::new();
    if settings.show_mode {
        values.push((style.badge(&local.mode_label), SegmentKind::Mode));
        if let Some(modifier) = local.mode_modifier.as_deref() {
            values.push((style.badge(modifier), SegmentKind::Module));
        }
    }
    if settings.show_role {
        values.push((style.badge(&local.role_label), SegmentKind::Module));
    }
    if settings.show_follow
        && let Some(follow) = local.follow_label.as_deref()
    {
        values.push((style.badge(follow), SegmentKind::Module));
    }
    let hint_visible = match settings.hint_policy {
        HintPolicy::Always => true,
        HintPolicy::ScrollOnly => local.mode_label == "SCROLL",
        HintPolicy::Never => false,
    };
    if settings.show_hint && hint_visible && !local.hint.is_empty() {
        values.push((style.badge(&local.hint), SegmentKind::Module));
    }
    let mut result = Vec::new();
    for (index, (text, kind)) in values.into_iter().enumerate() {
        if index > 0 {
            push_separator(&mut result, &style.module_separator);
        }
        result.push(ProjectedSegment {
            text,
            kind,
            window_id: None,
        });
    }
    result
}

fn values_to_segments(
    values: Vec<String>,
    kind: SegmentKind,
    separator: &str,
) -> Vec<ProjectedSegment> {
    let mut result = Vec::new();
    for (index, text) in values.into_iter().enumerate() {
        if index > 0 || !result.is_empty() {
            push_separator(&mut result, separator);
        }
        result.push(ProjectedSegment {
            text,
            kind,
            window_id: None,
        });
    }
    result
}

fn push_separator(output: &mut Vec<ProjectedSegment>, separator: &str) {
    output.push(ProjectedSegment {
        text: separator.to_string(),
        kind: SegmentKind::Base,
        window_id: None,
    });
}

fn visible_tabs_for_layout(
    tokens: &[TabToken],
    settings: &Settings,
    style: &RenderStyle,
    budget: usize,
) -> TabWindow {
    if tokens.is_empty() {
        return TabWindow { start: 0, end: 0 };
    }
    let anchor = tokens
        .iter()
        .position(|token| token.active)
        .unwrap_or(0)
        .min(tokens.len() - 1);
    let cap = settings.maximum_visible_tabs.unwrap_or(usize::MAX).max(1);
    let mut start = anchor;
    let mut end = anchor + 1;
    let prefer_left_first = matches!(settings.align_active, ActiveAlignment::FocusBias);
    let mut extend_left = prefer_left_first;
    loop {
        if end.saturating_sub(start) >= cap {
            break;
        }
        let left = start.checked_sub(1);
        let right = (end < tokens.len()).then_some(end);
        let candidates = if extend_left {
            [left, right]
        } else {
            [right, left]
        };
        let mut expanded = false;
        for candidate in candidates.into_iter().flatten() {
            let proposed = if candidate < start {
                TabWindow {
                    start: candidate,
                    end,
                }
            } else {
                TabWindow {
                    start,
                    end: candidate + 1,
                }
            };
            if tab_window_width(tokens, &proposed, style, settings.overflow_style) <= budget {
                start = proposed.start;
                end = proposed.end;
                expanded = true;
                break;
            }
        }
        if !expanded {
            break;
        }
        extend_left = !extend_left;
    }
    TabWindow { start, end }
}

fn tab_window_width(
    tokens: &[TabToken],
    window: &TabWindow,
    style: &RenderStyle,
    overflow_style: OverflowStyle,
) -> usize {
    let visible = window.end.saturating_sub(window.start);
    let mut width = tokens[window.start..window.end]
        .iter()
        .map(|token| token.width)
        .sum::<usize>()
        .saturating_add(
            UnicodeWidthStr::width(style.tab_separator.as_str())
                .saturating_mul(visible.saturating_sub(1)),
        );
    if window.start > 0 {
        width = width
            .saturating_add(UnicodeWidthStr::width(
                style.overflow(window.start, overflow_style).as_str(),
            ))
            .saturating_add(UnicodeWidthStr::width(style.tab_separator.as_str()));
    }
    let hidden_right = tokens.len().saturating_sub(window.end);
    if hidden_right > 0 {
        width = width
            .saturating_add(UnicodeWidthStr::width(
                style.overflow(hidden_right, overflow_style).as_str(),
            ))
            .saturating_add(UnicodeWidthStr::width(style.tab_separator.as_str()));
    }
    width
}

fn render_tab_template(
    settings: &Settings,
    window: &windows_list::WindowListEntry,
    index: usize,
    local: &AttachLocalPresentationSnapshot,
) -> String {
    let name = truncate_cells(&window.name, usize::from(settings.maximum_label_width));
    let session = local.session_label.as_deref().unwrap_or("");
    let mut output = String::with_capacity(settings.label_template.len());
    let mut chars = settings.label_template.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                output.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                output.push('}');
            }
            '{' => {
                let mut placeholder = String::new();
                let mut terminated = false;
                for next in chars.by_ref() {
                    if next == '}' {
                        terminated = true;
                        break;
                    }
                    placeholder.push(next);
                }
                let value = match placeholder.as_str() {
                    "name" => Some(name.clone()),
                    "index" => Some(index.saturating_add(1).to_string()),
                    "index0" => Some(index.to_string()),
                    "session" => Some(session.to_string()),
                    "marker" => Some(if window.active { "*" } else { "" }.to_string()),
                    "id" => Some(window.id.to_string()),
                    "active" => Some(if window.active { "active" } else { "idle" }.to_string()),
                    _ => None,
                };
                if terminated && let Some(value) = value {
                    output.push_str(&value);
                } else {
                    output.push('{');
                    output.push_str(&placeholder);
                    if terminated {
                        output.push('}');
                    }
                }
            }
            other => output.push(other),
        }
    }
    output
}

fn truncate_cells(value: &str, maximum: usize) -> String {
    let mut output = String::new();
    let mut width = 0_usize;
    for character in value.chars() {
        let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if width.saturating_add(character_width) > maximum {
            break;
        }
        output.push(character);
        width = width.saturating_add(character_width);
    }
    output
}

fn truncate_segments(segments: &mut Vec<ProjectedSegment>, maximum: usize) {
    let mut remaining = maximum;
    let mut retained = Vec::new();
    for mut segment in segments.drain(..) {
        if remaining == 0 {
            break;
        }
        let width = UnicodeWidthStr::width(segment.text.as_str());
        if width > remaining {
            segment.text = truncate_cells(&segment.text, remaining);
        }
        remaining = remaining.saturating_sub(UnicodeWidthStr::width(segment.text.as_str()));
        if !segment.text.is_empty() {
            retained.push(segment);
        }
    }
    *segments = retained;
}

fn segments_width(segments: &[ProjectedSegment]) -> usize {
    segments
        .iter()
        .map(|segment| UnicodeWidthStr::width(segment.text.as_str()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(index: u128, name: &str, active: bool) -> windows_list::WindowListEntry {
        windows_list::WindowListEntry {
            id: Uuid::from_u128(index),
            name: name.to_string(),
            active,
            workspace: "default".to_string(),
            workspace_id: Uuid::nil(),
        }
    }

    fn local(width: u16) -> AttachLocalPresentationSnapshot {
        AttachLocalPresentationSnapshot {
            viewport_cols: width,
            ..AttachLocalPresentationSnapshot::initial()
        }
    }

    #[test]
    fn default_projection_is_full_width_and_includes_mode_and_role() {
        let projected = project_bar(
            &Settings::default(),
            &[window(1, "main", true)],
            &local(40),
            None,
        );
        let text = projected.plain_text();
        assert_eq!(UnicodeWidthStr::width(text.as_str()), 40);
        assert!(text.contains(" main "));
        assert!(text.contains(" NORMAL "));
        assert!(text.contains(" write "));
    }

    #[test]
    fn narrow_projection_keeps_active_tab_and_uses_overflow() {
        let windows = (0..8)
            .map(|index| window(index + 1, &format!("window-{index}"), index == 7))
            .collect::<Vec<_>>();
        let projected = project_bar(&Settings::default(), &windows, &local(40), None);
        let text = projected.plain_text();
        assert_eq!(UnicodeWidthStr::width(text.as_str()), 40);
        assert!(text.contains("window-7"));
        assert!(text.contains('◀'));
    }

    #[test]
    fn legacy_template_escaping_and_unknown_tokens_are_preserved() {
        let settings = Settings {
            label_template: "{{{index}}}:{name}:{unknown}".to_string(),
            ..Settings::default()
        };
        let projected = project_bar(&settings, &[window(1, "main", true)], &local(80), None);
        let text = projected.plain_text();
        assert!(text.contains("{1}:main:{unknown}"));
    }
}
