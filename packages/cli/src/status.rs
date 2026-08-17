use bmux_appearance::RuntimeAppearance;
use bmux_config::{
    StatusAlignActive, StatusBarConfig, StatusBarPreset, StatusDensity, StatusHintPolicy,
    StatusOverflowStyle, StatusSeparatorSet,
};
use bmux_plugin::{RenderColor, RenderStyle, RenderTextSpan};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;

pub struct AttachTab {
    pub label: String,
    pub active: bool,
    pub context_id: Option<Uuid>,
}

/// Tab-strip inputs for the status line: the tabs themselves plus transient
/// pointer state that affects how they are drawn.
#[derive(Default)]
pub struct AttachTabStripInput<'a> {
    pub tabs: &'a [AttachTab],
    /// Context id of the tab currently under the mouse cursor, if any.
    pub hovered_context_id: Option<Uuid>,
    /// Inline label editor state, when a tab is being renamed.
    pub editing: Option<AttachTabEdit<'a>>,
}

/// In-progress inline tab rename, projected for rendering.
#[derive(Clone, Copy)]
pub struct AttachTabEdit<'a> {
    /// Context being edited.
    pub context_id: Uuid,
    /// Raw editor text, rendered in place of the templated label.
    pub text: &'a str,
    /// Cursor position as a byte index into `text`.
    pub cursor: usize,
    /// Selected byte range, if any.
    pub selection: Option<(usize, usize)>,
}

impl<'a> AttachTabStripInput<'a> {
    #[must_use]
    pub const fn new(tabs: &'a [AttachTab]) -> Self {
        Self {
            tabs,
            hovered_context_id: None,
            editing: None,
        }
    }

    #[must_use]
    pub const fn hovered(mut self, hovered_context_id: Option<Uuid>) -> Self {
        self.hovered_context_id = hovered_context_id;
        self
    }

    #[must_use]
    pub const fn editing(mut self, editing: Option<AttachTabEdit<'a>>) -> Self {
        self.editing = editing;
        self
    }
}

#[derive(Clone, Debug)]
pub struct AttachStatusTabHitbox {
    pub start_col: u16,
    pub end_col: u16,
    pub context_id: Uuid,
}

#[derive(Clone, Debug)]
pub struct AttachStatusLine {
    /// ANSI-styled status line retained for text consumers and compatibility.
    pub rendered: String,
    /// Declarative spans for render paths that want display-cell-safe styling.
    pub spans: Vec<RenderTextSpan>,
    pub tab_hitboxes: Vec<AttachStatusTabHitbox>,
    pub drag_marker_col: Option<u16>,
    /// Column of the inline rename cursor, when a tab is being edited.
    pub edit_cursor_col: Option<u16>,
}

#[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
pub fn build_attach_status_line(
    width: u16,
    config: &StatusBarConfig,
    runtime_appearance: &RuntimeAppearance,
    session_label: &str,
    session_count: usize,
    current_context_label: &str,
    tab_strip: &AttachTabStripInput<'_>,
    tab_position_label: Option<&str>,
    mode_label: &str,
    role_label: &str,
    follow_label: Option<&str>,
    mode_modifier: Option<&str>,
    hint: &str,
) -> AttachStatusLine {
    let tabs = tab_strip.tabs;
    let hovered_context_id = if config.hover_highlight {
        tab_strip.hovered_context_id
    } else {
        None
    };
    if !config.enabled {
        return AttachStatusLine {
            rendered: String::new(),
            spans: Vec::new(),
            tab_hitboxes: Vec::new(),
            drag_marker_col: None,
            edit_cursor_col: None,
        };
    }

    let style = StatusRenderStyle::from_config(config);
    let resolved_appearance = ResolvedStatusAppearance::resolve(config, runtime_appearance);
    let mut tab_hitboxes = Vec::new();
    let mut overflow_ranges = Vec::new();

    // Left-side segments that trail the tab strip are measured before the
    // tabs are packed so the tab strip knows how much width it may use.
    let mut tail = String::new();
    if config.show_session_name {
        append_segment(
            &mut tail,
            &style.module_separator,
            &format!("session:{session_label} ({session_count})"),
        );
    }
    if config.show_context_name {
        append_segment(
            &mut tail,
            &style.module_separator,
            &format!("ctx:{current_context_label}"),
        );
    }

    let (right, mode_range) = build_right_modules(
        config,
        &style,
        tab_position_label,
        mode_label,
        role_label,
        follow_label,
        mode_modifier,
        hint,
    );

    // Budget available to the tab strip: total width minus the right-aligned
    // modules, the minimum one-column spacer `compose_status_line` reserves
    // between the two sides, the left padding, and the trailing left segments.
    let tab_budget = usize::from(width)
        .saturating_sub(display_width(&right))
        .saturating_sub(usize::from(!right.is_empty()))
        .saturating_sub(config.layout.left_padding)
        .saturating_sub(display_width(&tail));

    let mut left = " ".repeat(config.layout.left_padding);
    let mut edit_ranges = None;
    append_tabs(
        &mut left,
        &mut tab_hitboxes,
        &mut overflow_ranges,
        config,
        tabs,
        &style,
        tab_budget,
        session_label,
        tab_strip.editing,
        &mut edit_ranges,
    );
    left.push_str(&tail);

    let composed = compose_status_line(width, &left, &right);
    // Clamp to the columns the left side actually kept, not the full terminal
    // width: `compose_status_line` truncates tab text that collides with the
    // right-hand modules, and a hitbox surviving past that point would be
    // invisible yet still clickable.
    clamp_hitboxes_to_visible_width(&mut tab_hitboxes, composed.left_visible_width);

    // Drop editor decorations that were truncated away by composition.
    let edit_ranges = edit_ranges.filter(|edit| edit.cursor_col < composed.left_visible_width);
    let edit_cursor_col = edit_ranges
        .as_ref()
        .and_then(|edit| u16::try_from(edit.cursor_col).ok());
    let edit_selection = edit_ranges.as_ref().and_then(|edit| {
        edit.selection.map(|(start, end)| {
            (
                start,
                end.min(composed.left_visible_width.saturating_sub(1)),
            )
        })
    });

    attach_status_line_from_composed(
        &composed,
        width,
        config,
        &resolved_appearance,
        tabs,
        hovered_context_id,
        tab_hitboxes,
        &overflow_ranges,
        mode_range,
        edit_cursor_col,
        edit_selection,
    )
}

/// Assemble the right-aligned status modules, returning the rendered text and
/// the mode badge's column range within it.
#[allow(
    clippy::too_many_arguments,
    reason = "status modules are independently configurable inputs"
)]
fn build_right_modules(
    config: &StatusBarConfig,
    style: &StatusRenderStyle,
    tab_position_label: Option<&str>,
    mode_label: &str,
    role_label: &str,
    follow_label: Option<&str>,
    mode_modifier: Option<&str>,
    hint: &str,
) -> (String, Option<(usize, usize)>) {
    let mut right_segments = Vec::new();
    let mut mode_range = None;
    if config.show_mode {
        append_right_segment(
            &mut right_segments,
            style,
            style.badge(mode_label),
            Some(&mut mode_range),
        );
        if let Some(modifier) = mode_modifier {
            append_right_segment(&mut right_segments, style, style.badge(modifier), None);
        }
    }
    if config.show_role {
        append_right_segment(&mut right_segments, style, style.badge(role_label), None);
    }
    if let Some(tab_position_label) = tab_position_label {
        append_right_segment(
            &mut right_segments,
            style,
            style.badge(tab_position_label),
            None,
        );
    }
    if config.show_follow
        && let Some(follow) = follow_label
    {
        append_right_segment(&mut right_segments, style, style.badge(follow), None);
    }
    if config.show_hint && hint_allowed(config.hint_policy, mode_label) {
        append_right_segment(&mut right_segments, style, style.badge(hint), None);
    }
    let mut right = right_segments.concat();
    if config.layout.right_padding > 0 {
        right.push_str(&" ".repeat(config.layout.right_padding));
    }
    (right, mode_range)
}

#[allow(clippy::too_many_arguments)]
fn attach_status_line_from_composed(
    composed: &ComposedStatusLine,
    width: u16,
    config: &StatusBarConfig,
    resolved_appearance: &ResolvedStatusAppearance,
    tabs: &[AttachTab],
    hovered_context_id: Option<Uuid>,
    tab_hitboxes: Vec<AttachStatusTabHitbox>,
    overflow_ranges: &[(usize, usize)],
    mode_range: Option<(usize, usize)>,
    edit_cursor_col: Option<u16>,
    edit_selection: Option<(usize, usize)>,
) -> AttachStatusLine {
    let resolved_mode_range = mode_range.map(|(start, end)| {
        let right_start = composed.right_start_col.unwrap_or(0);
        (
            right_start.saturating_add(start),
            right_start.saturating_add(end),
        )
    });
    let segment_input = StatusSegmentInput {
        tabs,
        hovered_context_id,
        hitboxes: &tab_hitboxes,
        overflow_ranges,
        mode_range: resolved_mode_range,
        right_start_col: composed.right_start_col,
        edit_selection,
    };
    let rendered = stylize_status_line(
        &composed.rendered,
        width,
        config,
        resolved_appearance,
        &segment_input,
    );
    let spans = status_line_spans(
        &composed.rendered,
        width,
        config,
        resolved_appearance,
        &segment_input,
    );

    AttachStatusLine {
        rendered,
        spans,
        tab_hitboxes,
        drag_marker_col: None,
        edit_cursor_col,
    }
}

/// Inputs that determine per-column styling of the status line.
struct StatusSegmentInput<'a> {
    tabs: &'a [AttachTab],
    hovered_context_id: Option<Uuid>,
    hitboxes: &'a [AttachStatusTabHitbox],
    overflow_ranges: &'a [(usize, usize)],
    mode_range: Option<(usize, usize)>,
    right_start_col: Option<usize>,
    /// Inclusive column range selected in the inline tab editor.
    edit_selection: Option<(usize, usize)>,
}

fn hint_allowed(policy: StatusHintPolicy, mode_label: &str) -> bool {
    match policy {
        StatusHintPolicy::Always => true,
        StatusHintPolicy::ScrollOnly => mode_label == "SCROLL",
        StatusHintPolicy::Never => false,
    }
}

#[allow(clippy::too_many_arguments)] // Tab packing needs config, style, budget, and template inputs.
fn append_tabs(
    out: &mut String,
    hitboxes: &mut Vec<AttachStatusTabHitbox>,
    overflow_ranges: &mut Vec<(usize, usize)>,
    config: &StatusBarConfig,
    tabs: &[AttachTab],
    style: &StatusRenderStyle,
    budget: usize,
    session_label: &str,
    editing: Option<AttachTabEdit<'_>>,
    edit_ranges: &mut Option<AppendedTabEdit>,
) {
    if tabs.is_empty() {
        out.push_str(&style.empty_tabs_label);
        return;
    }

    let tokens = tab_tokens(config, tabs, style, session_label, editing);
    let window = visible_tabs_for_layout(&tokens, config, style, budget);
    let hidden_left = window.start;
    let hidden_right = tokens.len().saturating_sub(window.end);
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

    for (offset, token) in tokens[window.start..window.end].iter().enumerate() {
        if offset > 0 {
            out.push_str(&style.tab_separator);
            col = col.saturating_add(display_width(&style.tab_separator));
        }
        out.push_str(&token.text);
        if let Some(context_id) = token.context_id {
            hitboxes.push(AttachStatusTabHitbox {
                start_col: u16::try_from(col).unwrap_or(u16::MAX),
                end_col: u16::try_from(col.saturating_add(token.width.saturating_sub(1)))
                    .unwrap_or(u16::MAX),
                context_id,
            });
        }
        // Translate editor offsets within the token into absolute columns.
        if let Some(cursor_offset) = token.edit_cursor_offset {
            *edit_ranges = Some(AppendedTabEdit {
                cursor_col: col.saturating_add(cursor_offset),
                selection: token.edit_selection.and_then(|(start, end)| {
                    (end > start).then_some((
                        col.saturating_add(start),
                        col.saturating_add(end).saturating_sub(1),
                    ))
                }),
            });
        }
        col = col.saturating_add(token.width);
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

/// Absolute columns of the inline editor within the composed left side.
struct AppendedTabEdit {
    cursor_col: usize,
    /// Inclusive selected column range.
    selection: Option<(usize, usize)>,
}

/// One tab pre-rendered to its final status-bar token, so packing decisions
/// can measure exact display widths without re-formatting.
struct TabToken {
    text: String,
    width: usize,
    active: bool,
    context_id: Option<Uuid>,
    /// Cursor offset in display cells from the token start, when this tab is
    /// the one being edited inline.
    edit_cursor_offset: Option<usize>,
    /// Selected display-cell range within the token, when editing.
    edit_selection: Option<(usize, usize)>,
}

fn tab_tokens(
    config: &StatusBarConfig,
    tabs: &[AttachTab],
    style: &StatusRenderStyle,
    session_label: &str,
    editing: Option<AttachTabEdit<'_>>,
) -> Vec<TabToken> {
    let template = config.resolved_tab_template();
    tabs.iter()
        .enumerate()
        .map(|(index, tab)| {
            let edit = editing.filter(|edit| Some(edit.context_id) == tab.context_id);
            if let Some(edit) = edit {
                // While editing, the raw buffer replaces the templated label so
                // the user sees exactly the text they are typing. Template
                // chrome is restored on commit or cancel.
                return editing_tab_token(tab, style, edit);
            }
            // Truncate the variable-length name before substitution so
            // `tab_label_max_width` bounds the name without ever clipping
            // template chrome mid-token.
            let label = truncate_cells(&tab.label, config.tab_label_max_width.max(1));
            let rendered = render_tab_template(
                template,
                &TabTemplateFields {
                    name: &label,
                    index,
                    session: session_label,
                    active: tab.active,
                },
            );
            let text = if tab.active {
                style.active_tab(&rendered)
            } else {
                style.inactive_tab(&rendered)
            };
            TabToken {
                width: display_width(&text),
                text,
                active: tab.active,
                context_id: tab.context_id,
                edit_cursor_offset: None,
                edit_selection: None,
            }
        })
        .collect()
}

/// Build the token for the tab currently being renamed.
///
/// The editor is never truncated by `tab_label_max_width`: the packing pass
/// keeps it whole so typing a long name pushes other tabs into overflow rather
/// than hiding the text being entered.
fn editing_tab_token(
    tab: &AttachTab,
    style: &StatusRenderStyle,
    edit: AttachTabEdit<'_>,
) -> TabToken {
    let text = if tab.active {
        style.active_tab(edit.text)
    } else {
        style.inactive_tab(edit.text)
    };
    // Affix width offsets cursor/selection positions inside the token.
    let prefix_width = display_width(if tab.active {
        style.active_prefix
    } else {
        style.inactive_prefix
    });
    let cursor_offset = prefix_width.saturating_add(byte_prefix_width(edit.text, edit.cursor));
    let selection = edit.selection.map(|(start, end)| {
        (
            prefix_width.saturating_add(byte_prefix_width(edit.text, start)),
            prefix_width.saturating_add(byte_prefix_width(edit.text, end)),
        )
    });
    TabToken {
        width: display_width(&text),
        text,
        active: tab.active,
        context_id: tab.context_id,
        edit_cursor_offset: Some(cursor_offset),
        edit_selection: selection,
    }
}

/// Display width of `text` up to `byte_index`, clamped to a char boundary.
fn byte_prefix_width(text: &str, byte_index: usize) -> usize {
    let mut end = byte_index.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    display_width(&text[..end])
}

/// Values substituted into [`StatusBarConfig::resolved_tab_template`].
struct TabTemplateFields<'a> {
    name: &'a str,
    /// Zero-based position in the full tab list.
    index: usize,
    session: &'a str,
    active: bool,
}

impl TabTemplateFields<'_> {
    /// Resolve a placeholder name, or `None` when it is not recognized.
    fn lookup(&self, placeholder: &str) -> Option<String> {
        match placeholder {
            "name" => Some(self.name.to_string()),
            "index" => Some((self.index + 1).to_string()),
            "index0" => Some(self.index.to_string()),
            "session" => Some(self.session.to_string()),
            "marker" => Some(if self.active { "*" } else { "" }.to_string()),
            _ => None,
        }
    }
}

/// Substitute `{placeholder}` tokens in `template`.
///
/// `{{` and `}}` produce literal braces. Unknown or unterminated placeholders
/// are emitted verbatim so a malformed template degrades to visible text rather
/// than breaking the status bar.
fn render_tab_template(template: &str, fields: &TabTemplateFields<'_>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
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
                if let Some(value) = fields.lookup(&placeholder).filter(|_| terminated) {
                    out.push_str(&value);
                } else {
                    // Unknown or unterminated: keep the original text.
                    out.push('{');
                    out.push_str(&placeholder);
                    if terminated {
                        out.push('}');
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Half-open range of tab indexes that will be rendered.
struct TabWindow {
    start: usize,
    end: usize,
}

/// Choose the widest run of tabs that fits `budget` display columns while
/// always keeping the active tab visible.
///
/// Width is the primary constraint; `status_bar.max_tabs`, when set, applies
/// an additional hard cap on the number of visible tabs.
fn visible_tabs_for_layout(
    tokens: &[TabToken],
    config: &StatusBarConfig,
    style: &StatusRenderStyle,
    budget: usize,
) -> TabWindow {
    // The tab being edited anchors the window so its text is never scrolled
    // out from under the cursor; otherwise the active tab anchors it.
    let anchor = tokens
        .iter()
        .position(|token| token.edit_cursor_offset.is_some())
        .or_else(|| tokens.iter().position(|token| token.active))
        .unwrap_or(0)
        .min(tokens.len().saturating_sub(1));
    let cap = config.max_tabs.unwrap_or(usize::MAX).max(1);

    // The anchor is always rendered, even when it alone exceeds the budget;
    // `compose_status_line` truncates rather than dropping the active tab.
    let mut window = TabWindow {
        start: anchor,
        end: anchor + 1,
    };
    let separator_width = display_width(&style.tab_separator);

    // Alternate extension direction so `FocusBias` keeps the active tab
    // centered, while `KeepVisible` fills to the left first and therefore
    // parks the active tab at the right edge of the strip.
    let mut prefer_left = match config.layout.align_active {
        StatusAlignActive::KeepVisible => true,
        StatusAlignActive::FocusBias => false,
    };

    while window.end - window.start < cap {
        let can_extend_left = window.start > 0;
        let can_extend_right = window.end < tokens.len();
        if !can_extend_left && !can_extend_right {
            break;
        }

        let extend_left = if prefer_left {
            can_extend_left
        } else {
            !can_extend_right
        };
        let candidate = if extend_left {
            TabWindow {
                start: window.start - 1,
                end: window.end,
            }
        } else {
            TabWindow {
                start: window.start,
                end: window.end + 1,
            }
        };

        if tab_strip_width(tokens, &candidate, style, separator_width) > budget {
            // This direction is exhausted for good; try the other one and stop
            // once neither can grow.
            let other = if extend_left {
                TabWindow {
                    start: window.start,
                    end: window.end + 1,
                }
            } else {
                TabWindow {
                    start: window.start.saturating_sub(1),
                    end: window.end,
                }
            };
            let other_fits = if extend_left {
                window.end < tokens.len()
            } else {
                window.start > 0
            } && tab_strip_width(tokens, &other, style, separator_width) <= budget;
            if !other_fits {
                break;
            }
            window = other;
        } else {
            window = candidate;
        }

        if matches!(config.layout.align_active, StatusAlignActive::FocusBias) {
            prefer_left = !prefer_left;
        }
    }

    window
}

/// Total display width of the rendered tab strip for `window`, including the
/// inter-tab separators and whichever overflow markers would still be needed.
fn tab_strip_width(
    tokens: &[TabToken],
    window: &TabWindow,
    style: &StatusRenderStyle,
    separator_width: usize,
) -> usize {
    let visible = window.end.saturating_sub(window.start);
    let mut width = tokens[window.start..window.end]
        .iter()
        .map(|token| token.width)
        .sum::<usize>()
        .saturating_add(separator_width.saturating_mul(visible.saturating_sub(1)));

    let hidden_left = window.start;
    if hidden_left > 0 {
        width = width
            .saturating_add(display_width(&style.overflow_marker(hidden_left)))
            .saturating_add(separator_width);
    }
    let hidden_right = tokens.len().saturating_sub(window.end);
    if hidden_right > 0 {
        width = width
            .saturating_add(display_width(&style.overflow_marker(hidden_right)))
            .saturating_add(separator_width);
    }
    width
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
    if !out.is_empty() {
        out.push_str(separator);
    }
    out.push_str(value);
}

fn append_right_segment(
    segments: &mut Vec<String>,
    style: &StatusRenderStyle,
    value: String,
    range_out: Option<&mut Option<(usize, usize)>>,
) {
    let start = segments
        .iter()
        .map(String::as_str)
        .map(display_width)
        .sum::<usize>();
    if !segments.is_empty() {
        segments.push(style.module_separator.clone());
    }
    let value_start = start.saturating_add(if segments.len() > 1 {
        display_width(&style.module_separator)
    } else {
        0
    });
    let width = display_width(&value);
    if let Some(range) = range_out
        && width > 0
    {
        *range = Some((value_start, value_start.saturating_add(width - 1)));
    }
    segments.push(value);
}

struct ComposedStatusLine {
    rendered: String,
    right_start_col: Option<usize>,
    /// Columns the left side (tab strip and trailing left segments) actually
    /// occupies in `rendered`. Left content beyond this was truncated away, so
    /// hitboxes and styling must not extend past it.
    left_visible_width: usize,
}

fn compose_status_line(width: u16, left: &str, right: &str) -> ComposedStatusLine {
    let width = usize::from(width);
    if width == 0 {
        return ComposedStatusLine {
            rendered: String::new(),
            right_start_col: None,
            left_visible_width: 0,
        };
    }

    if right.is_empty() {
        return ComposedStatusLine {
            rendered: pad_or_truncate(left, width),
            right_start_col: None,
            left_visible_width: width,
        };
    }

    let right_width = display_width(right);
    if right_width >= width {
        // The right modules consume the whole line; no left content survives.
        return ComposedStatusLine {
            rendered: truncate_cells(right, width),
            right_start_col: Some(0),
            left_visible_width: 0,
        };
    }

    let available_left = width.saturating_sub(right_width + 1);
    let left_trimmed = truncate_cells(left, available_left);
    let left_width = display_width(&left_trimmed);
    let spacer = " ".repeat(width.saturating_sub(left_width + right_width));
    ComposedStatusLine {
        rendered: format!("{left_trimmed}{spacer}{right}"),
        right_start_col: Some(width.saturating_sub(right_width)),
        left_visible_width: available_left,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SegmentKind {
    Base,
    ActiveTab,
    InactiveTab,
    HoveredActiveTab,
    HoveredInactiveTab,
    /// Selected text inside the inline tab rename editor.
    EditSelection,
    Mode,
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

struct ResolvedStatusAppearance {
    base: SegmentStyle,
    active_tab: SegmentStyle,
    inactive_tab: SegmentStyle,
    hovered_active_tab: SegmentStyle,
    hovered_inactive_tab: SegmentStyle,
    edit_selection: SegmentStyle,
    mode: SegmentStyle,
    module: SegmentStyle,
    overflow: SegmentStyle,
}

impl ResolvedStatusAppearance {
    #[allow(clippy::similar_names, clippy::too_many_lines)] // bg/fg pairs are intentionally parallel names
    fn resolve(config: &StatusBarConfig, runtime_appearance: &RuntimeAppearance) -> Self {
        let fallback_bar_bg =
            parse_hex_color(&runtime_appearance.status.background).unwrap_or(RgbColor {
                r: 30,
                g: 30,
                b: 30,
            });
        let fallback_bar_fg =
            parse_hex_color(&runtime_appearance.status.foreground).unwrap_or(RgbColor {
                r: 220,
                g: 220,
                b: 220,
            });
        let fallback_active_bg = parse_hex_color(&runtime_appearance.status.active_window)
            .unwrap_or(RgbColor {
                r: 110,
                g: 170,
                b: 240,
            });
        let fallback_active_fg =
            parse_hex_color(&runtime_appearance.background).unwrap_or(RgbColor {
                r: 20,
                g: 20,
                b: 20,
            });
        let fallback_mode_bg = parse_hex_color(&runtime_appearance.status.mode_indicator)
            .unwrap_or(fallback_active_bg);

        let bar_bg = config
            .colors
            .bar_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(fallback_bar_bg);
        let bar_fg = config
            .colors
            .bar_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(fallback_bar_fg);
        let active_bg = config
            .colors
            .tab_active_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(fallback_active_bg);
        let active_fg = config
            .colors
            .tab_active_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(fallback_active_fg);
        let inactive_bg = config
            .colors
            .tab_inactive_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or_else(|| adjust_rgb(bar_bg, 18));
        let inactive_fg = config
            .colors
            .tab_inactive_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(bar_fg);
        let module_bg = config
            .colors
            .module_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or_else(|| adjust_rgb(bar_bg, 10));
        let module_fg = config
            .colors
            .module_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(bar_fg);
        let overflow_bg = config
            .colors
            .overflow_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or_else(|| adjust_rgb(bar_bg, 26));
        let overflow_fg = config
            .colors
            .overflow_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(bar_fg);
        // Hover is a subtle background lift over the tab's normal color, so it
        // reads cohesively on both active and inactive tabs regardless of theme.
        let hovered_inactive_bg = config
            .colors
            .tab_hover_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or_else(|| adjust_rgb(inactive_bg, 18));
        let hovered_inactive_fg = config
            .colors
            .tab_hover_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(inactive_fg);
        let hovered_active_bg = config
            .colors
            .tab_active_hover_bg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or_else(|| adjust_rgb(active_bg, 12));
        let hovered_active_fg = config
            .colors
            .tab_active_hover_fg
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(active_fg);

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
            hovered_active_tab: SegmentStyle {
                fg: hovered_active_fg,
                bg: hovered_active_bg,
                bold: config.style.bold_active,
                dim: false,
                underline: config.style.underline_active,
            },
            hovered_inactive_tab: SegmentStyle {
                fg: hovered_inactive_fg,
                bg: hovered_inactive_bg,
                bold: false,
                // Undim on hover so the highlight is unmistakable.
                dim: false,
                underline: false,
            },
            mode: SegmentStyle {
                fg: fallback_active_fg,
                bg: fallback_mode_bg,
                bold: true,
                dim: false,
                underline: false,
            },
            // Selected editor text: swap fg/bg for an unmistakable, theme-
            // independent reverse-video look.
            edit_selection: SegmentStyle {
                fg: active_bg,
                bg: active_fg,
                bold: false,
                dim: false,
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

fn stylize_status_line(
    rendered_plain: &str,
    width: u16,
    config: &StatusBarConfig,
    appearance: &ResolvedStatusAppearance,
    input: &StatusSegmentInput<'_>,
) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    let segments = status_segments(width, input);

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
        let style = style_for_segment(segments[col], config, appearance);
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

fn status_line_spans(
    rendered_plain: &str,
    width: u16,
    config: &StatusBarConfig,
    appearance: &ResolvedStatusAppearance,
    input: &StatusSegmentInput<'_>,
) -> Vec<RenderTextSpan> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }
    let segments = status_segments(width, input);
    let mut spans = Vec::new();
    let mut current_kind = None;
    let mut current_text = String::new();
    let mut col = 0usize;
    for ch in rendered_plain.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if char_width == 0 {
            current_text.push(ch);
            continue;
        }
        if col >= width {
            break;
        }
        let kind = segments[col];
        if let Some(previous_kind) = current_kind
            && previous_kind != kind
            && !current_text.is_empty()
        {
            let style = style_for_segment(previous_kind, config, appearance);
            spans.push(RenderTextSpan::new(
                std::mem::take(&mut current_text),
                render_style_from_status_segment(style),
            ));
        }
        current_kind = Some(kind);
        current_text.push(ch);
        col = col.saturating_add(char_width);
    }
    if let Some(kind) = current_kind
        && !current_text.is_empty()
    {
        let style = style_for_segment(kind, config, appearance);
        spans.push(RenderTextSpan::new(
            current_text,
            render_style_from_status_segment(style),
        ));
    }
    spans
}

fn status_segments(width: usize, input: &StatusSegmentInput<'_>) -> Vec<SegmentKind> {
    let StatusSegmentInput {
        tabs,
        hovered_context_id,
        hitboxes,
        overflow_ranges,
        mode_range,
        right_start_col,
        edit_selection,
    } = *input;
    let mut segments = vec![SegmentKind::Base; width];
    // Left-side ranges may never bleed into the right-hand module zone. Cap
    // them at `right_start_col` so a stale or oversized range cannot repaint
    // the mode/role/tab-position badges.
    let left_limit = right_start_col.unwrap_or(width).min(width);
    if let Some(start) = right_start_col {
        for segment in &mut segments[start.min(width)..width] {
            *segment = SegmentKind::Module;
        }
    }

    for (start, end) in overflow_ranges {
        if *start >= left_limit {
            continue;
        }
        for segment in &mut segments[*start..=(*end).min(left_limit.saturating_sub(1))] {
            *segment = SegmentKind::Overflow;
        }
    }

    if let Some((start, end)) = mode_range
        && start < width
    {
        for segment in &mut segments[start..=end.min(width.saturating_sub(1))] {
            *segment = SegmentKind::Mode;
        }
    }

    for hitbox in hitboxes {
        let hovered = hovered_context_id == Some(hitbox.context_id);
        let kind = tabs
            .iter()
            .find(|tab| tab.context_id == Some(hitbox.context_id))
            .map_or(SegmentKind::InactiveTab, |tab| {
                match (tab.active, hovered) {
                    (true, true) => SegmentKind::HoveredActiveTab,
                    (true, false) => SegmentKind::ActiveTab,
                    (false, true) => SegmentKind::HoveredInactiveTab,
                    (false, false) => SegmentKind::InactiveTab,
                }
            });
        let start = usize::from(hitbox.start_col);
        if start >= left_limit {
            // Out-of-range hitbox: skip it rather than folding it onto the
            // last column, which would tint the module zone.
            continue;
        }
        let end = usize::from(hitbox.end_col).min(left_limit.saturating_sub(1));
        for segment in &mut segments[start..=end] {
            *segment = kind;
        }
    }

    // Editor selection paints last so it reads clearly over tab styling, and is
    // still capped to the left region.
    if let Some((start, end)) = edit_selection
        && start < left_limit
    {
        let end = end.min(left_limit.saturating_sub(1));
        for segment in &mut segments[start..=end] {
            *segment = SegmentKind::EditSelection;
        }
    }
    segments
}

const fn render_style_from_status_segment(style: SegmentStyle) -> RenderStyle {
    RenderStyle {
        fg: Some(RenderColor::Rgb {
            r: style.fg.r,
            g: style.fg.g,
            b: style.fg.b,
        }),
        bg: Some(RenderColor::Rgb {
            r: style.bg.r,
            g: style.bg.g,
            b: style.bg.b,
        }),
        bold: style.bold,
        underline: style.underline,
        italic: false,
        reverse: false,
        dim: style.dim,
        blink: false,
        strikethrough: false,
    }
}

const fn style_for_segment(
    segment: SegmentKind,
    _config: &StatusBarConfig,
    appearance: &ResolvedStatusAppearance,
) -> SegmentStyle {
    match segment {
        SegmentKind::Base => appearance.base,
        SegmentKind::ActiveTab => appearance.active_tab,
        SegmentKind::InactiveTab => appearance.inactive_tab,
        SegmentKind::HoveredActiveTab => appearance.hovered_active_tab,
        SegmentKind::HoveredInactiveTab => appearance.hovered_inactive_tab,
        SegmentKind::EditSelection => appearance.edit_selection,
        SegmentKind::Mode => appearance.mode,
        SegmentKind::Module => appearance.module,
        SegmentKind::Overflow => appearance.overflow,
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
    let adjust = |channel: u8| -> u8 {
        u8::try_from((i16::from(channel) + delta).clamp(0, 255)).unwrap_or(0)
    };
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

/// Drop or trim tab hitboxes so they only ever cover columns whose tab text is
/// actually visible in the rendered line.
fn clamp_hitboxes_to_visible_width(
    hitboxes: &mut Vec<AttachStatusTabHitbox>,
    visible_width: usize,
) {
    if visible_width == 0 {
        hitboxes.clear();
        return;
    }
    let max = u16::try_from(visible_width.saturating_sub(1)).unwrap_or(u16::MAX);
    hitboxes.retain_mut(|entry| {
        if entry.start_col > max {
            return false;
        }
        entry.end_col = entry.end_col.min(max);
        entry.start_col <= entry.end_col
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_badge_uses_mode_indicator_color() {
        let appearance = RuntimeAppearance {
            background: "#010203".to_string(),
            status: bmux_appearance::RuntimeStatusAppearance {
                mode_indicator: "#112233".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let status = build_attach_status_line(
            80,
            &StatusBarConfig::default(),
            &appearance,
            "session",
            1,
            "context",
            &AttachTabStripInput::new(&[]),
            None,
            "NORMAL",
            "write",
            None,
            None,
            "",
        );

        assert!(status.rendered.contains("NORMAL"));
        assert!(
            status
                .rendered
                .contains("\x1b[0;1;38;2;1;2;3;48;2;17;34;51m")
        );
        let mode_span = status
            .spans
            .iter()
            .find(|span| span.text.contains("NORMAL"))
            .expect("mode span should be declarative");
        assert!(mode_span.style.bold);
        assert_eq!(
            mode_span.style.bg,
            Some(RenderColor::Rgb {
                r: 17,
                g: 34,
                b: 51,
            })
        );
    }

    #[test]
    fn frozen_modifier_preserves_scroll_mode_badge_and_hint() {
        let config = StatusBarConfig {
            hint_policy: StatusHintPolicy::ScrollOnly,
            ..StatusBarConfig::default()
        };
        let status = build_attach_status_line(
            100,
            &config,
            &RuntimeAppearance::default(),
            "session",
            1,
            "context",
            &AttachTabStripInput::new(&[]),
            None,
            "SCROLL",
            "write",
            None,
            Some("FROZEN"),
            "scroll hint",
        );
        let rendered = plain_rendered(&status);
        assert!(rendered.contains("SCROLL"));
        assert!(rendered.contains("FROZEN"));
        assert!(rendered.contains("scroll hint"));
    }

    #[test]
    fn disabled_status_line_has_no_declarative_spans() {
        let status = build_attach_status_line(
            80,
            &StatusBarConfig {
                enabled: false,
                ..StatusBarConfig::default()
            },
            &RuntimeAppearance::default(),
            "session",
            1,
            "context",
            &AttachTabStripInput::new(&[]),
            None,
            "NORMAL",
            "write",
            None,
            None,
            "",
        );

        assert!(status.rendered.is_empty());
        assert!(status.spans.is_empty());
    }

    fn sim_tabs(count: usize, active: usize) -> Vec<AttachTab> {
        (0..count)
            .map(|index| AttachTab {
                label: format!("win{index}"),
                active: index == active,
                context_id: Some(Uuid::from_u128(index as u128 + 1)),
            })
            .collect()
    }

    /// Config using the legacy indexed template. Tab-packing and hitbox tests
    /// prefer it because `N:winX` tokens are unambiguous substrings.
    fn indexed_config() -> StatusBarConfig {
        StatusBarConfig {
            tab_template: Some("{index}:{name}".to_string()),
            ..StatusBarConfig::default()
        }
    }

    fn status_line_for(
        width: u16,
        config: &StatusBarConfig,
        tabs: &[AttachTab],
    ) -> AttachStatusLine {
        status_line_hovering(width, config, tabs, None)
    }

    fn status_line_hovering(
        width: u16,
        config: &StatusBarConfig,
        tabs: &[AttachTab],
        hovered: Option<Uuid>,
    ) -> AttachStatusLine {
        build_attach_status_line(
            width,
            config,
            &RuntimeAppearance::default(),
            "session",
            1,
            "context",
            &AttachTabStripInput::new(tabs).hovered(hovered),
            None,
            "NORMAL",
            "write",
            None,
            None,
            "",
        )
    }

    /// Background color of the span covering the given tab label.
    fn tab_span_bg(status: &AttachStatusLine, label: &str) -> Option<RenderColor> {
        status
            .spans
            .iter()
            .find(|span| span.text.contains(label))
            .expect("tab span should exist")
            .style
            .bg
    }

    fn plain_rendered(status: &AttachStatusLine) -> String {
        status
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>()
    }

    #[test]
    fn wide_width_shows_every_tab_without_overflow_marker() {
        let tabs = sim_tabs(20, 0);
        let status = status_line_for(300, &indexed_config(), &tabs);
        let rendered = plain_rendered(&status);

        for index in 0..20 {
            assert!(
                rendered.contains(&format!("{}:win{index}", index + 1)),
                "tab {index} should be visible at width 300: {rendered:?}"
            );
        }
        assert!(!rendered.contains('◀'), "{rendered:?}");
        assert!(!rendered.contains('▶'), "{rendered:?}");
        assert_eq!(status.tab_hitboxes.len(), 20);
    }

    #[test]
    fn narrow_width_collapses_tabs_and_keeps_active_visible() {
        let tabs = sim_tabs(20, 19);
        let status = status_line_for(40, &indexed_config(), &tabs);
        let rendered = plain_rendered(&status);

        assert_eq!(display_width(&rendered), 40, "{rendered:?}");
        assert!(rendered.contains("20:win19"), "{rendered:?}");
        assert!(rendered.contains('◀'), "{rendered:?}");
        assert!(status.tab_hitboxes.len() < 20);
    }

    #[test]
    fn explicit_max_tabs_caps_tab_count_on_wide_terminals() {
        let tabs = sim_tabs(20, 0);
        let config = StatusBarConfig {
            max_tabs: Some(3),
            ..indexed_config()
        };
        let status = status_line_for(300, &config, &tabs);
        let rendered = plain_rendered(&status);

        assert_eq!(status.tab_hitboxes.len(), 3, "{rendered:?}");
        assert!(rendered.contains("1:win0"), "{rendered:?}");
        assert!(rendered.contains("3:win2"), "{rendered:?}");
        assert!(!rendered.contains("4:win3"), "{rendered:?}");
        assert!(rendered.contains('▶'), "{rendered:?}");
    }

    #[test]
    fn hitboxes_match_rendered_tab_columns_at_wide_width() {
        let tabs = sim_tabs(6, 2);
        let status = status_line_for(200, &indexed_config(), &tabs);
        let rendered = plain_rendered(&status);
        let cells = rendered.chars().collect::<Vec<_>>();

        for (index, hitbox) in status.tab_hitboxes.iter().enumerate() {
            let start = usize::from(hitbox.start_col);
            let end = usize::from(hitbox.end_col);
            let token = cells[start..=end].iter().collect::<String>();
            assert_eq!(
                token.trim(),
                format!("{}:win{index}", index + 1),
                "hitbox {index} should cover its token: {rendered:?}"
            );
        }
    }

    #[test]
    fn active_tab_stays_visible_when_it_is_the_last_of_many() {
        let tabs = sim_tabs(30, 29);
        let status = status_line_for(60, &indexed_config(), &tabs);
        let rendered = plain_rendered(&status);

        assert!(rendered.contains("30:win29"), "{rendered:?}");
        let active_context = Uuid::from_u128(30);
        assert!(
            status
                .tab_hitboxes
                .iter()
                .any(|hitbox| hitbox.context_id == active_context),
            "active tab should keep a hitbox: {rendered:?}"
        );
    }

    /// Column where the right-hand module zone begins, located via the mode
    /// badge text (preset-independent).
    fn modules_start_col(plain: &str) -> Option<usize> {
        let byte_index = plain.find("NORMAL")?;
        Some(display_width(&plain[..byte_index]))
    }

    #[test]
    fn hovering_a_later_tab_undims_it_in_declarative_spans() {
        // Regression for a renderer-level attribute leak: with dim_inactive on,
        // hovering any tab after the first used to keep rendering dim because
        // the dim attribute from an earlier tab was never cleared.
        let tabs = sim_tabs(6, 0);
        let config = StatusBarConfig {
            hover_highlight: true,
            ..StatusBarConfig::default()
        };
        assert!(
            config.style.dim_inactive,
            "test relies on the default dim_inactive"
        );

        for hovered_index in [0_usize, 1, 2, 4, 5] {
            let hovered_id = tabs[hovered_index].context_id;
            let status = status_line_hovering(200, &config, &tabs, hovered_id);
            let label = format!("win{hovered_index}");
            let span = status
                .spans
                .iter()
                .find(|span| span.text.contains(&label))
                .unwrap_or_else(|| panic!("tab {hovered_index} should render a span"));
            assert!(
                !span.style.dim,
                "hovered tab {hovered_index} should not be dim: {:?}",
                span.style
            );
        }
    }

    #[test]
    fn hovered_tab_ansi_output_clears_dim_from_earlier_tabs() {
        // The ANSI string is what terminals actually receive, so assert the
        // hovered token is not left dim by a preceding inactive tab.
        let tabs = sim_tabs(6, 0);
        let hovered_id = tabs[3].context_id;
        let status = status_line_hovering(200, &StatusBarConfig::default(), &tabs, hovered_id);

        let hovered_at = status
            .rendered
            .find("win3")
            .expect("hovered tab should be rendered");
        // Find the SGR sequence introducing the hovered token.
        let prefix = &status.rendered[..hovered_at];
        let sgr_start = prefix
            .rfind("\u{1b}[")
            .expect("hovered token should be preceded by an SGR sequence");
        let sgr = &prefix[sgr_start..];
        assert!(
            !sgr.split('m').next().unwrap_or_default().contains(";2;") || !sgr.contains(";2;38"),
            "hovered token SGR should not enable dim: {sgr:?}"
        );
        // The status line builds each cell's SGR from scratch with a leading 0
        // reset, so the hovered run must start from a cleared state.
        assert!(
            sgr.starts_with("\u{1b}[0"),
            "hovered token SGR should reset before styling: {sgr:?}"
        );
    }

    #[test]
    fn tab_hitboxes_only_cover_visible_tab_text() {
        let tabs = sim_tabs(12, 0);
        for width in [10_u16, 14, 18, 24, 30, 45, 60, 80, 120, 200] {
            let status = status_line_for(width, &indexed_config(), &tabs);
            let plain = plain_rendered(&status);
            let cells = plain.chars().collect::<Vec<_>>();
            for hitbox in &status.tab_hitboxes {
                let start = usize::from(hitbox.start_col);
                let end = usize::from(hitbox.end_col);
                assert!(
                    end < cells.len(),
                    "width {width}: hitbox {start}..={end} outside rendered line {plain:?}"
                );
                let index = tabs
                    .iter()
                    .position(|tab| tab.context_id == Some(hitbox.context_id))
                    .expect("hitbox should map to a tab");
                let covered = cells[start..=end].iter().collect::<String>();
                let full_token = format!("{}:win{index}", index + 1);
                // The hitbox may cover a truncated prefix of the token, but
                // whatever it covers must be real, visible tab text.
                assert!(
                    !covered.trim().is_empty() && full_token.starts_with(covered.trim()),
                    "width {width}: hitbox {start}..={end} covered {covered:?}, \
                     which is not visible text of {full_token:?} in {plain:?}"
                );
            }
        }
    }

    #[test]
    fn tab_hitboxes_never_reach_the_right_module_zone() {
        // Long session/context tails push the tab strip toward the modules,
        // which previously left invisible-but-clickable hitboxes underneath.
        let tabs = sim_tabs(14, 13);
        let config = StatusBarConfig {
            show_session_name: true,
            show_context_name: true,
            ..StatusBarConfig::default()
        };
        for width in [24_u16, 32, 48, 64, 100] {
            let status = build_attach_status_line(
                width,
                &config,
                &RuntimeAppearance::default(),
                "a-fairly-long-session-name",
                4,
                "a-fairly-long-context-name",
                &AttachTabStripInput::new(&tabs),
                Some("tab:14/14"),
                "NORMAL",
                "write",
                None,
                None,
                "",
            );
            let plain = plain_rendered(&status);
            let Some(modules_start) = modules_start_col(&plain) else {
                continue;
            };
            for hitbox in &status.tab_hitboxes {
                assert!(
                    usize::from(hitbox.end_col) < modules_start,
                    "width {width}: hitbox {}..={} overlaps modules at {modules_start} in {plain:?}",
                    hitbox.start_col,
                    hitbox.end_col
                );
            }
        }
    }

    #[test]
    fn oversized_active_tab_does_not_leak_a_hitbox_under_modules() {
        let tabs = vec![AttachTab {
            label: "an-extremely-long-window-name-that-cannot-fit".to_string(),
            active: true,
            context_id: Some(Uuid::from_u128(1)),
        }];
        let config = StatusBarConfig {
            tab_label_max_width: 60,
            ..StatusBarConfig::default()
        };
        let status = status_line_for(30, &config, &tabs);
        let plain = plain_rendered(&status);
        let modules_start =
            modules_start_col(&plain).expect("mode badge should be rendered at width 30");

        for hitbox in &status.tab_hitboxes {
            assert!(
                usize::from(hitbox.end_col) < modules_start,
                "truncated active tab leaked a hitbox into the module zone: {plain:?}"
            );
        }
    }

    #[test]
    fn tab_hitboxes_are_dropped_when_only_modules_fit() {
        let tabs = sim_tabs(6, 0);
        let status = status_line_for(6, &indexed_config(), &tabs);

        assert!(
            status.tab_hitboxes.is_empty(),
            "no tab text is visible, so no tab should be clickable: {:?}",
            plain_rendered(&status)
        );
    }

    #[test]
    fn module_zone_keeps_module_styling_at_every_width() {
        let tabs = sim_tabs(14, 13);
        for width in [20_u16, 28, 40, 70, 140] {
            let status = status_line_for(width, &indexed_config(), &tabs);
            let plain = plain_rendered(&status);
            let Some(modules_start) = modules_start_col(&plain) else {
                continue;
            };
            // Walk spans to find the style covering the mode badge and assert
            // it is the mode style, not a tab style bleeding rightward.
            let mut col = 0usize;
            let mut mode_span_bg = None;
            for span in &status.spans {
                let span_width = display_width(&span.text);
                if col <= modules_start && modules_start < col + span_width {
                    mode_span_bg = Some(span.style.bg);
                    break;
                }
                col += span_width;
            }
            let appearance = ResolvedStatusAppearance::resolve(
                &StatusBarConfig::default(),
                &RuntimeAppearance::default(),
            );
            let expected = render_style_from_status_segment(appearance.mode).bg;
            assert_eq!(
                mode_span_bg,
                Some(expected),
                "width {width}: mode badge should keep mode styling in {plain:?}"
            );
        }
    }

    #[test]
    fn hovering_an_inactive_tab_lifts_its_background() {
        let tabs = sim_tabs(4, 0);
        let hovered_id = tabs[2].context_id;
        let plain = status_line_for(120, &indexed_config(), &tabs);
        let hovered = status_line_hovering(120, &indexed_config(), &tabs, hovered_id);

        let normal_bg = tab_span_bg(&plain, "3:win2");
        let hovered_bg = tab_span_bg(&hovered, "3:win2");
        assert_ne!(
            normal_bg, hovered_bg,
            "hovered inactive tab should change background"
        );
        // Other tabs stay untouched.
        assert_eq!(
            tab_span_bg(&plain, "2:win1"),
            tab_span_bg(&hovered, "2:win1"),
            "non-hovered tabs should be unaffected"
        );
    }

    #[test]
    fn hovering_the_active_tab_lifts_its_background() {
        let tabs = sim_tabs(4, 1);
        let hovered_id = tabs[1].context_id;
        let plain = status_line_for(120, &indexed_config(), &tabs);
        let hovered = status_line_hovering(120, &indexed_config(), &tabs, hovered_id);

        let normal_bg = tab_span_bg(&plain, "2:win1");
        let hovered_bg = tab_span_bg(&hovered, "2:win1");
        assert_ne!(
            normal_bg, hovered_bg,
            "hovered active tab should change background"
        );
    }

    #[test]
    fn hover_highlight_disabled_matches_unhovered_output() {
        let tabs = sim_tabs(4, 0);
        let hovered_id = tabs[2].context_id;
        let config = StatusBarConfig {
            hover_highlight: false,
            ..StatusBarConfig::default()
        };

        let without_hover = status_line_for(120, &config, &tabs);
        let with_hover = status_line_hovering(120, &config, &tabs, hovered_id);

        assert_eq!(
            without_hover.rendered, with_hover.rendered,
            "hover_highlight=false should render identically"
        );
        assert_eq!(without_hover.spans.len(), with_hover.spans.len());
    }

    #[test]
    fn hover_respects_explicit_color_overrides() {
        let tabs = sim_tabs(3, 0);
        let hovered_id = tabs[1].context_id;
        let mut config = indexed_config();
        config.colors.tab_hover_bg = Some("#123456".to_string());

        let hovered = status_line_hovering(120, &config, &tabs, hovered_id);

        assert_eq!(
            tab_span_bg(&hovered, "2:win1"),
            Some(RenderColor::Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56
            }),
            "explicit hover background should win"
        );
    }

    #[test]
    fn hovering_an_unknown_context_changes_nothing() {
        let tabs = sim_tabs(3, 0);
        let plain = status_line_for(120, &indexed_config(), &tabs);
        let hovered = status_line_hovering(120, &indexed_config(), &tabs, Some(Uuid::nil()));

        assert_eq!(plain.rendered, hovered.rendered);
    }

    #[test]
    fn hover_styling_does_not_leak_into_the_module_zone() {
        let tabs = sim_tabs(14, 13);
        let hovered_id = tabs[13].context_id;
        for width in [24_u16, 40, 70] {
            let status = status_line_hovering(width, &indexed_config(), &tabs, hovered_id);
            let plain = plain_rendered(&status);
            let Some(modules_start) = modules_start_col(&plain) else {
                continue;
            };
            let appearance = ResolvedStatusAppearance::resolve(
                &StatusBarConfig::default(),
                &RuntimeAppearance::default(),
            );
            let hover_bg = render_style_from_status_segment(appearance.hovered_active_tab).bg;
            let mut col = 0usize;
            for span in &status.spans {
                let span_width = display_width(&span.text);
                if col >= modules_start {
                    assert_ne!(
                        span.style.bg, hover_bg,
                        "width {width}: hover styling leaked into modules in {plain:?}"
                    );
                }
                col += span_width;
            }
        }
    }

    #[test]
    fn default_template_shows_only_the_tab_name() {
        let tabs = sim_tabs(3, 0);
        let status = status_line_for(120, &StatusBarConfig::default(), &tabs);
        let rendered = plain_rendered(&status);

        assert!(rendered.contains("win0"), "{rendered:?}");
        assert!(
            !rendered.contains("1:win0"),
            "default template must not add an index prefix: {rendered:?}"
        );
        assert!(!rendered.contains("2:win1"), "{rendered:?}");
    }

    #[test]
    fn indexed_template_reproduces_legacy_output() {
        let tabs = sim_tabs(3, 0);
        let status = status_line_for(120, &indexed_config(), &tabs);
        let rendered = plain_rendered(&status);

        assert!(rendered.contains("1:win0"), "{rendered:?}");
        assert!(rendered.contains("3:win2"), "{rendered:?}");
    }

    #[test]
    fn legacy_show_tab_index_still_selects_the_indexed_template() {
        let config = StatusBarConfig {
            show_tab_index: Some(true),
            ..StatusBarConfig::default()
        };
        assert_eq!(config.resolved_tab_template(), "{index}:{name}");

        let tabs = sim_tabs(2, 0);
        let rendered = plain_rendered(&status_line_for(120, &config, &tabs));
        assert!(rendered.contains("1:win0"), "{rendered:?}");
    }

    #[test]
    fn explicit_template_overrides_legacy_show_tab_index() {
        let config = StatusBarConfig {
            tab_template: Some("{name}".to_string()),
            show_tab_index: Some(true),
            ..StatusBarConfig::default()
        };
        let tabs = sim_tabs(2, 0);
        let rendered = plain_rendered(&status_line_for(120, &config, &tabs));

        assert!(rendered.contains("win0"), "{rendered:?}");
        assert!(!rendered.contains("1:win0"), "{rendered:?}");
    }

    #[test]
    fn template_supports_all_documented_placeholders() {
        let fields = TabTemplateFields {
            name: "editor",
            index: 3,
            session: "work",
            active: true,
        };

        assert_eq!(render_tab_template("{name}", &fields), "editor");
        assert_eq!(render_tab_template("{index}", &fields), "4");
        assert_eq!(render_tab_template("{index0}", &fields), "3");
        assert_eq!(render_tab_template("{session}", &fields), "work");
        assert_eq!(render_tab_template("{marker}", &fields), "*");
        assert_eq!(
            render_tab_template("[{index}] {name}{marker}", &fields),
            "[4] editor*"
        );
    }

    #[test]
    fn template_marker_is_empty_for_inactive_tabs() {
        let fields = TabTemplateFields {
            name: "shell",
            index: 0,
            session: "work",
            active: false,
        };
        assert_eq!(render_tab_template("{name}{marker}", &fields), "shell");
    }

    #[test]
    fn template_renders_unknown_and_unterminated_placeholders_literally() {
        let fields = TabTemplateFields {
            name: "editor",
            index: 0,
            session: "work",
            active: false,
        };

        assert_eq!(render_tab_template("{bogus}", &fields), "{bogus}");
        assert_eq!(
            render_tab_template("{name} {bogus} x", &fields),
            "editor {bogus} x"
        );
        // Unterminated placeholder keeps its text without inventing a brace.
        assert_eq!(render_tab_template("{name", &fields), "{name");
    }

    #[test]
    fn template_escapes_double_braces() {
        let fields = TabTemplateFields {
            name: "editor",
            index: 0,
            session: "work",
            active: false,
        };

        assert_eq!(render_tab_template("{{{name}}}", &fields), "{editor}");
        assert_eq!(render_tab_template("{{literal}}", &fields), "{literal}");
    }

    #[test]
    fn template_name_still_respects_tab_label_max_width() {
        let tabs = vec![AttachTab {
            label: "an-extremely-long-window-name".to_string(),
            active: true,
            context_id: Some(Uuid::from_u128(1)),
        }];
        let config = StatusBarConfig {
            tab_template: Some("[{name}]".to_string()),
            tab_label_max_width: 6,
            ..StatusBarConfig::default()
        };
        let rendered = plain_rendered(&status_line_for(120, &config, &tabs));

        // Template chrome survives; only the name is truncated.
        assert!(rendered.contains("[an-ext]"), "{rendered:?}");
    }

    #[test]
    fn template_width_feeds_the_packing_budget() {
        let tabs = sim_tabs(12, 0);
        let verbose = StatusBarConfig {
            tab_template: Some("<<{index}::{name}>>".to_string()),
            ..StatusBarConfig::default()
        };

        // Wide: every tab fits with the longer template.
        let wide = plain_rendered(&status_line_for(300, &verbose, &tabs));
        assert!(wide.contains("<<1::win0>>"), "{wide:?}");
        assert!(wide.contains("<<12::win11>>"), "{wide:?}");

        // Narrow: the longer template collapses more tabs than the plain one.
        let narrow_verbose = status_line_for(70, &verbose, &tabs);
        let narrow_plain = status_line_for(70, &StatusBarConfig::default(), &tabs);
        assert!(
            narrow_verbose.tab_hitboxes.len() < narrow_plain.tab_hitboxes.len(),
            "verbose template should fit fewer tabs: {} vs {}",
            narrow_verbose.tab_hitboxes.len(),
            narrow_plain.tab_hitboxes.len()
        );
        assert_eq!(
            display_width(&plain_rendered(&narrow_verbose)),
            70,
            "verbose template must still render exactly to width"
        );
    }

    #[test]
    // `{index}`/`{name}` are tab-template placeholders, not format arguments.
    #[allow(clippy::literal_string_with_formatting_args)]
    fn hitboxes_track_template_widths() {
        let tabs = sim_tabs(5, 0);
        let config = StatusBarConfig {
            tab_template: Some("[{index}] {name}".to_string()),
            ..StatusBarConfig::default()
        };
        let status = status_line_for(200, &config, &tabs);
        let plain = plain_rendered(&status);
        let cells = plain.chars().collect::<Vec<_>>();

        for (index, hitbox) in status.tab_hitboxes.iter().enumerate() {
            let covered = cells[usize::from(hitbox.start_col)..=usize::from(hitbox.end_col)]
                .iter()
                .collect::<String>();
            assert!(
                covered.contains(&format!("[{}] win{index}", index + 1)),
                "hitbox {index} should cover the templated token, got {covered:?}"
            );
        }
    }

    #[test]
    fn tab_strip_never_overlaps_right_modules() {
        let tabs = sim_tabs(12, 0);
        for width in [30_u16, 45, 60, 80, 120] {
            let status = status_line_for(width, &indexed_config(), &tabs);
            let rendered = plain_rendered(&status);
            assert_eq!(
                display_width(&rendered),
                usize::from(width),
                "width {width} should render exactly: {rendered:?}"
            );
            assert!(
                rendered.contains("NORMAL"),
                "width {width} should keep the mode badge: {rendered:?}"
            );
        }
    }
}
