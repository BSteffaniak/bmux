use bmux_config::{
    StatusAlignActive, StatusBarConfig, StatusBarPreset, StatusDensity, StatusHintPolicy,
    StatusOverflowStyle, StatusSeparatorSet, ThemeConfig,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;

pub struct AttachTab {
    pub label: String,
    pub active: bool,
    pub context_id: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct AttachStatusTabHitbox {
    pub start_col: u16,
    pub end_col: u16,
    pub context_id: Uuid,
}

#[derive(Clone, Debug)]
pub struct AttachStatusLine {
    pub rendered: String,
    pub tab_hitboxes: Vec<AttachStatusTabHitbox>,
}

#[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
pub fn build_attach_status_line(
    width: u16,
    config: &StatusBarConfig,
    global_theme: &ThemeConfig,
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
    let resolved_theme = ResolvedStatusTheme::resolve(config, global_theme);
    let mut left = String::new();
    let mut tab_hitboxes = Vec::new();
    let mut overflow_ranges = Vec::new();
    left.push_str(&" ".repeat(config.layout.left_padding));

    append_tabs(
        &mut left,
        &mut tab_hitboxes,
        &mut overflow_ranges,
        config,
        tabs,
        &style,
    );

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

    let composed = compose_status_line(width, &left, &right);
    clamp_hitboxes_to_width(&mut tab_hitboxes, width);

    let rendered = stylize_status_line(
        &composed.rendered,
        width,
        config,
        &resolved_theme,
        tabs,
        &tab_hitboxes,
        &overflow_ranges,
        composed.right_start_col,
    );

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
    overflow_ranges: &mut Vec<(usize, usize)>,
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
    let mut col = display_width(out);

    if hidden_left > 0 {
        let marker = style.overflow_marker(hidden_left);
        let start = col;
        out.push_str(&marker);
        col = col.saturating_add(display_width(&marker));
        let end = col.saturating_sub(1);
        overflow_ranges.push((start, end));
        out.push_str(&style.tab_separator);
        col = col.saturating_add(display_width(&style.tab_separator));
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
                start_col: u16::try_from(col).unwrap_or(u16::MAX),
                end_col: u16::try_from(col.saturating_add(token_width.saturating_sub(1)))
                    .unwrap_or(u16::MAX),
                context_id,
            });
        }
        col = col.saturating_add(token_width);
    }

    if hidden_right > 0 {
        out.push_str(&style.tab_separator);
        col = col.saturating_add(display_width(&style.tab_separator));
        let marker = style.overflow_marker(hidden_right);
        let start = col;
        out.push_str(&marker);
        col = col.saturating_add(display_width(&marker));
        let end = col.saturating_sub(1);
        overflow_ranges.push((start, end));
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
        let separators = if use_ascii
            || matches!(
                config.style.separator_set,
                StatusSeparatorSet::Ascii | StatusSeparatorSet::Plain
            ) {
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
            StatusBarPreset::TabRail => (" ", " ", " ", " ", " ", " "),
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

struct ComposedStatusLine {
    rendered: String,
    right_start_col: Option<usize>,
}

fn compose_status_line(width: u16, left: &str, right: &str) -> ComposedStatusLine {
    let width = usize::from(width);
    if width == 0 {
        return ComposedStatusLine {
            rendered: String::new(),
            right_start_col: None,
        };
    }

    if right.is_empty() {
        return ComposedStatusLine {
            rendered: pad_or_truncate(left, width),
            right_start_col: None,
        };
    }

    let right_width = display_width(right);
    if right_width >= width {
        return ComposedStatusLine {
            rendered: truncate_cells(right, width),
            right_start_col: Some(0),
        };
    }

    let available_left = width.saturating_sub(right_width + 1);
    let left_trimmed = truncate_cells(left, available_left);
    let left_width = display_width(&left_trimmed);
    let spacer = " ".repeat(width.saturating_sub(left_width + right_width));
    ComposedStatusLine {
        rendered: format!("{left_trimmed}{spacer}{right}"),
        right_start_col: Some(width.saturating_sub(right_width)),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SegmentKind {
    Base,
    ActiveTab,
    InactiveTab,
    Module,
    Overflow,
}

#[derive(Clone, Copy)]
struct RgbColor {
    r: u8,
    g: u8,
    b: u8,
}

#[derive(Clone, Copy)]
struct SegmentStyle {
    fg: RgbColor,
    bg: RgbColor,
    bold: bool,
    dim: bool,
    underline: bool,
}

struct ResolvedStatusTheme {
    base: SegmentStyle,
    active_tab: SegmentStyle,
    inactive_tab: SegmentStyle,
    module: SegmentStyle,
    overflow: SegmentStyle,
}

impl ResolvedStatusTheme {
    #[allow(clippy::similar_names, clippy::too_many_lines)] // bg/fg pairs are intentionally parallel names
    fn resolve(config: &StatusBarConfig, global_theme: &ThemeConfig) -> Self {
        let fallback_bar_bg =
            parse_hex_color(&global_theme.status.background).unwrap_or(RgbColor {
                r: 30,
                g: 30,
                b: 30,
            });
        let fallback_bar_fg =
            parse_hex_color(&global_theme.status.foreground).unwrap_or(RgbColor {
                r: 220,
                g: 220,
                b: 220,
            });
        let fallback_active_bg =
            parse_hex_color(&global_theme.status.active_window).unwrap_or(RgbColor {
                r: 110,
                g: 170,
                b: 240,
            });
        let fallback_active_fg = parse_hex_color(&global_theme.background).unwrap_or(RgbColor {
            r: 20,
            g: 20,
            b: 20,
        });

        let bar_bg = config
            .theme
            .bar_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(fallback_bar_bg);
        let bar_fg = config
            .theme
            .bar_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(fallback_bar_fg);
        let active_bg = config
            .theme
            .tab_active_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(fallback_active_bg);
        let active_fg = config
            .theme
            .tab_active_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(fallback_active_fg);
        let inactive_bg = config
            .theme
            .tab_inactive_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or_else(|| adjust_rgb(bar_bg, 18));
        let inactive_fg = config
            .theme
            .tab_inactive_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(bar_fg);
        let module_bg = config
            .theme
            .module_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or_else(|| adjust_rgb(bar_bg, 10));
        let module_fg = config
            .theme
            .module_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(bar_fg);
        let overflow_bg = config
            .theme
            .overflow_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or_else(|| adjust_rgb(bar_bg, 26));
        let overflow_fg = config
            .theme
            .overflow_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(bar_fg);

        Self {
            base: SegmentStyle {
                fg: bar_fg,
                bg: bar_bg,
                bold: false,
                dim: false,
                underline: false,
            },
            active_tab: SegmentStyle {
                fg: active_fg,
                bg: active_bg,
                bold: config.style.bold_active,
                dim: false,
                underline: config.style.underline_active,
            },
            inactive_tab: SegmentStyle {
                fg: inactive_fg,
                bg: inactive_bg,
                bold: false,
                dim: config.style.dim_inactive,
                underline: false,
            },
            module: SegmentStyle {
                fg: module_fg,
                bg: module_bg,
                bold: false,
                dim: false,
                underline: false,
            },
            overflow: SegmentStyle {
                fg: overflow_fg,
                bg: overflow_bg,
                bold: false,
                dim: false,
                underline: false,
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stylize_status_line(
    rendered_plain: &str,
    width: u16,
    config: &StatusBarConfig,
    theme: &ResolvedStatusTheme,
    tabs: &[AttachTab],
    hitboxes: &[AttachStatusTabHitbox],
    overflow_ranges: &[(usize, usize)],
    right_start_col: Option<usize>,
) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }

    let mut segments = vec![SegmentKind::Base; width];
    if let Some(start) = right_start_col {
        for segment in &mut segments[start.min(width)..width] {
            *segment = SegmentKind::Module;
        }
    }

    for (start, end) in overflow_ranges {
        if *start >= width {
            continue;
        }
        for segment in &mut segments[*start..=(*end).min(width.saturating_sub(1))] {
            *segment = SegmentKind::Overflow;
        }
    }

    for hitbox in hitboxes {
        let kind = tabs
            .iter()
            .find(|tab| tab.context_id == Some(hitbox.context_id))
            .map_or(SegmentKind::InactiveTab, |tab| {
                if tab.active {
                    SegmentKind::ActiveTab
                } else {
                    SegmentKind::InactiveTab
                }
            });
        let start = usize::from(hitbox.start_col).min(width.saturating_sub(1));
        let end = usize::from(hitbox.end_col).min(width.saturating_sub(1));
        for segment in &mut segments[start..=end] {
            *segment = kind;
        }
    }

    let mut rendered = String::new();
    let mut current_style = None;
    let mut col = 0usize;
    for ch in rendered_plain.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if char_width == 0 {
            rendered.push(ch);
            continue;
        }
        if col >= width {
            break;
        }
        let style = style_for_segment(segments[col], config, theme);
        if current_style != Some(segments[col]) {
            rendered.push_str(&style_sgr(style));
            current_style = Some(segments[col]);
        }
        rendered.push(ch);
        col = col.saturating_add(char_width);
    }
    rendered.push_str("\x1b[0m");
    rendered
}

const fn style_for_segment(
    segment: SegmentKind,
    _config: &StatusBarConfig,
    theme: &ResolvedStatusTheme,
) -> SegmentStyle {
    match segment {
        SegmentKind::Base => theme.base,
        SegmentKind::ActiveTab => theme.active_tab,
        SegmentKind::InactiveTab => theme.inactive_tab,
        SegmentKind::Module => theme.module,
        SegmentKind::Overflow => theme.overflow,
    }
}

fn style_sgr(style: SegmentStyle) -> String {
    let mut parts = vec!["0".to_string()];
    if style.bold {
        parts.push("1".to_string());
    }
    if style.dim {
        parts.push("2".to_string());
    }
    if style.underline {
        parts.push("4".to_string());
    }
    parts.push(format!("38;2;{};{};{}", style.fg.r, style.fg.g, style.fg.b));
    parts.push(format!("48;2;{};{};{}", style.bg.r, style.bg.g, style.bg.b));
    format!("\x1b[{}m", parts.join(";"))
}

fn parse_hex_color(value: &str) -> Option<RgbColor> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(RgbColor { r, g, b })
}

fn adjust_rgb(value: RgbColor, delta: i16) -> RgbColor {
    let adjust = |channel: u8| -> u8 { (i16::from(channel) + delta).clamp(0, 255) as u8 };
    RgbColor {
        r: adjust(value.r),
        g: adjust(value.g),
        b: adjust(value.b),
    }
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
