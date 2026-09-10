use bmux_attach_view_protocol::AttachLocalPresentationSnapshot;
use bmux_tui::component::{Component, Constraints, LayoutCx, LayoutNode};
use bmux_tui::composition::{Row, TextBlock};
use bmux_tui::measured_list::MeasuredListIndex;
use std::cell::RefCell;
use std::collections::BTreeMap;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    ActiveAlignment, Density, HintPolicy, OverflowStyle, Preset, SeparatorSet, Settings, tabs_list,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Base,
    Workspace,
    EditingWorkspace,
    ActiveTab,
    InactiveTab,
    HoveredActiveTab,
    HoveredInactiveTab,
    EditingTab,
    Mode,
    Module,
    Overflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedSegment {
    pub(super) text: String,
    pub(super) kind: SegmentKind,
    pub(super) tab_id: Option<Uuid>,
    pub(super) edit_cursor_offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedBar {
    pub(super) segments: Vec<ProjectedSegment>,
    layout: LayoutNode,
    previous_anchor: Option<Uuid>,
    next_anchor: Option<Uuid>,
}

pub struct ProjectedTabRange {
    pub(super) tab_id: Uuid,
    pub(super) start: u16,
    pub(super) end: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropSide {
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedInsertion {
    pub(super) tab_id: Uuid,
    pub(super) side: DropSide,
    pub(super) marker_col: u16,
}

/// Interaction geometry is independent of segment text, decoration, and styling.
/// Only visible tab bounds contribute insertion anchors; clipped tabs do not.
pub struct TabBarGeometry {
    pub(super) tabs: Vec<ProjectedTabRange>,
}

impl TabBarGeometry {
    pub fn resolve_insertion(&self, col: u16) -> Option<ResolvedInsertion> {
        // Compare cell centers to edge coordinates in half-cell units. Equal
        // distances favor the right anchor, including a single-cell separator.
        let pointer = u32::from(col) * 2 + 1;
        self.tabs
            .iter()
            .filter(|tab| tab.start < tab.end)
            .flat_map(|tab| {
                [
                    ResolvedInsertion {
                        tab_id: tab.tab_id,
                        side: DropSide::Before,
                        marker_col: tab.start,
                    },
                    ResolvedInsertion {
                        tab_id: tab.tab_id,
                        side: DropSide::After,
                        marker_col: tab.end,
                    },
                ]
            })
            .min_by_key(|anchor| {
                (
                    pointer.abs_diff(u32::from(anchor.marker_col) * 2),
                    std::cmp::Reverse(anchor.marker_col),
                )
            })
    }
}

impl ProjectedBar {
    pub fn interaction_geometry(&self) -> TabBarGeometry {
        TabBarGeometry {
            tabs: self.tab_ranges(),
        }
    }
    fn new(segments: Vec<ProjectedSegment>) -> Self {
        let layout = {
            let row = segments.iter().fold(Row::new(), |row, segment| {
                row.child(TextBlock::new(segment.text.as_str()))
            });
            row.layout(
                Constraints::new(0, u64::MAX, 1, Some(1)),
                &mut LayoutCx::new(),
            )
        };
        Self {
            segments,
            layout,
            previous_anchor: None,
            next_anchor: None,
        }
    }

    pub const fn scroll_target(&self, forward: bool) -> Option<Uuid> {
        if forward {
            self.next_anchor
        } else {
            self.previous_anchor
        }
    }

    pub fn positioned_segments(&self) -> impl Iterator<Item = (&ProjectedSegment, u16, u16)> {
        self.segments
            .iter()
            .zip(&self.layout.children)
            .map(|(segment, child)| {
                let x = u16::try_from(child.x).unwrap_or(u16::MAX);
                let width = u16::try_from(child.node.size.width).unwrap_or(u16::MAX);
                (segment, x, width)
            })
    }

    pub fn tab_ranges(&self) -> Vec<ProjectedTabRange> {
        let mut ranges: Vec<ProjectedTabRange> = Vec::new();
        for (segment, x, width) in self.positioned_segments() {
            if let Some(tab_id) = segment.tab_id {
                if let Some(last) = ranges.last_mut()
                    && last.tab_id == tab_id
                    && last.end == x
                {
                    last.end = x.saturating_add(width);
                } else {
                    ranges.push(ProjectedTabRange {
                        tab_id,
                        start: x,
                        end: x.saturating_add(width),
                    });
                }
            }
        }
        ranges
    }

    pub fn drop_target_at_col(&self, col: u16) -> Option<ResolvedInsertion> {
        self.interaction_geometry().resolve_insertion(col)
    }

    #[cfg(test)]
    pub fn tab_at_col(&self, col: u16) -> Option<Uuid> {
        for (segment, x, width) in self.positioned_segments() {
            if col >= x && col < x.saturating_add(width) {
                return segment.tab_id;
            }
        }
        None
    }

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
    tab_id: Uuid,
    edit_cursor_offset: Option<usize>,
    edit_selection: Option<(usize, usize)>,
}

struct TabTab {
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

#[derive(Clone, Debug)]
pub struct ProjectionMeasurements(RefCell<MeasuredListIndex<Uuid>>);

impl ProjectionMeasurements {
    pub fn step_anchor(&self, first: Uuid, last: Uuid, forward: bool) -> Option<Uuid> {
        bmux_tui_components::scroll_view::ScrollView::step_item_anchor(
            &self.0.borrow(),
            &first,
            &last,
            forward,
        )
    }
}

impl Default for ProjectionMeasurements {
    fn default() -> Self {
        Self(RefCell::new(MeasuredListIndex::new(0)))
    }
}

#[derive(Default)]
pub struct ProjectionInteraction<'a> {
    pub(super) measurements: Option<&'a ProjectionMeasurements>,
    pub(super) scroll_anchor: Option<Uuid>,
    pub(super) editing_tab_id: Option<Uuid>,
    pub(super) edit_text: Option<&'a str>,
    pub(super) edit_selection: Option<(usize, usize)>,
    pub(super) menu_tab_id: Option<Uuid>,
    pub(super) menu_selected: usize,
    pub(super) drag_marker_col: Option<u16>,
    pub(super) workspace_label: Option<&'a str>,
    pub(super) editing_workspace: bool,
}

#[allow(clippy::too_many_lines)] // Projection is one ordered width-budgeting pass; splitting obscures shared constraints.
pub fn project_bar(
    settings: &Settings,
    tabs: &[tabs_list::TabListEntry],
    local: &AttachLocalPresentationSnapshot,
    hovered_tab_id: Option<Uuid>,
    interaction: &ProjectionInteraction<'_>,
) -> ProjectedBar {
    let style = RenderStyle::from_settings(settings);
    let mut right = right_segments(settings, local, &style);
    let mut tail = left_tail(settings, local, &style);
    let width = if local.viewport_cols == 0 {
        usize::from(u16::MAX)
    } else {
        usize::from(local.viewport_cols)
    };
    // Tabs are the interactive content. Optional status modules may use the
    // remaining space, but must not remove every tab on a narrow viewport.
    let inner_width = width
        .saturating_sub(settings.left_padding)
        .saturating_sub(settings.right_padding);
    let tab_reserve = if tabs.is_empty() {
        0
    } else {
        inner_width.min(2)
    };
    truncate_segments(
        &mut right,
        inner_width.saturating_sub(tab_reserve).saturating_sub(1),
    );
    let right_width = segments_width(&right);
    let module_budget = inner_width
        .saturating_sub(tab_reserve)
        .saturating_sub(right_width)
        .saturating_sub(usize::from(!right.is_empty()));
    truncate_segments(&mut tail, module_budget);
    let tail_width = segments_width(&tail);
    let workspace_name = interaction
        .workspace_label
        .or_else(|| tabs.first().map(|tab| tab.workspace.as_str()));
    let workspace_draft = interaction.edit_text.map(editor_label);
    let workspace_name = if interaction.editing_workspace {
        workspace_draft.as_deref().or(workspace_name)
    } else {
        workspace_name
    };
    let workspace_budget = width.saturating_sub(right_width + tail_width + 20).min(24);
    let workspace_label =
        workspace_name.map_or_else(String::new, |name| truncate_cells(name, workspace_budget));
    let show_workspace = workspace_name.is_some() && workspace_budget > 0;
    let tab_budget = width
        .saturating_sub(UnicodeWidthStr::width(workspace_label.as_str()))
        .saturating_sub(if show_workspace { 3 } else { 0 })
        .saturating_sub(right_width)
        .saturating_sub(usize::from(!right.is_empty()))
        .saturating_sub(settings.left_padding)
        .saturating_sub(settings.right_padding)
        .saturating_sub(tail_width);
    let tokens = tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let editing = interaction.editing_tab_id == Some(tab.id);
            let draft = editing.then(|| editing_entry(tab, interaction.edit_text));
            let label = render_tab_template(settings, draft.as_ref().unwrap_or(tab), index, local);
            let text = style.tab(&label, tab.active);
            TabToken {
                width: UnicodeWidthStr::width(text.as_str()),
                text,
                active: tab.active,
                hovered: settings.hover_highlight && hovered_tab_id == Some(tab.id),
                tab_id: tab.id,
                edit_cursor_offset: editing.then_some(0),
                edit_selection: if editing {
                    interaction.edit_selection
                } else {
                    None
                },
            }
        })
        .collect::<Vec<_>>();
    let fallback_measurements = ProjectionMeasurements::default();
    let measurements = interaction.measurements.unwrap_or(&fallback_measurements);
    let tab = visible_tabs_for_layout(
        &tokens,
        settings,
        &style,
        tab_budget,
        interaction.scroll_anchor,
        &mut measurements.0.borrow_mut(),
    );
    let mut left = vec![ProjectedSegment {
        text: " ".repeat(settings.left_padding),
        kind: SegmentKind::Base,
        tab_id: None,
        edit_cursor_offset: None,
    }];
    left.push(ProjectedSegment {
        text: workspace_label,
        kind: if interaction.editing_workspace {
            SegmentKind::EditingWorkspace
        } else {
            SegmentKind::Workspace
        },
        tab_id: None,
        edit_cursor_offset: interaction.editing_workspace.then_some(0),
    });
    if show_workspace {
        left.push(ProjectedSegment {
            text: " │ ".to_string(),
            kind: SegmentKind::Base,
            tab_id: None,
            edit_cursor_offset: None,
        });
    }
    if tokens.is_empty() {
        left.push(ProjectedSegment {
            text: "[no tabs]".to_string(),
            kind: SegmentKind::Base,
            tab_id: None,
            edit_cursor_offset: None,
        });
    } else {
        append_tabs(&mut left, &tokens, &tab, settings, &style, tab_budget);
    }
    left.extend(tail);

    let minimum_spacer = usize::from(!right.is_empty());
    let available_left = width
        .saturating_sub(settings.right_padding)
        .saturating_sub(right_width)
        .saturating_sub(minimum_spacer);
    truncate_segments(&mut left, available_left);
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
            tab_id: None,
            edit_cursor_offset: None,
        });
    }
    segments.extend(right);
    if settings.right_padding > 0 {
        segments.push(ProjectedSegment {
            text: " ".repeat(settings.right_padding),
            kind: SegmentKind::Base,
            tab_id: None,
            edit_cursor_offset: None,
        });
    }
    append_menu(&mut segments, interaction, &style);
    truncate_segments(&mut segments, width);
    let current_width = segments_width(&segments);
    if current_width < width {
        segments.push(ProjectedSegment {
            text: " ".repeat(width - current_width),
            kind: SegmentKind::Base,
            tab_id: None,
            edit_cursor_offset: None,
        });
    }
    let mut projected = ProjectedBar::new(segments);
    let ranges = projected.tab_ranges();
    if let Some((first, last)) = ranges.first().zip(ranges.last()) {
        projected.previous_anchor = measurements.step_anchor(first.tab_id, last.tab_id, false);
        projected.next_anchor = measurements.step_anchor(first.tab_id, last.tab_id, true);
    }
    projected
}

fn append_menu(
    segments: &mut Vec<ProjectedSegment>,
    interaction: &ProjectionInteraction<'_>,
    style: &RenderStyle,
) {
    if interaction.menu_tab_id.is_none() {
        return;
    }
    segments.clear();
    for (index, label) in ["Switch", "Rename", "Close"].iter().enumerate() {
        if index > 0 {
            push_separator(segments, &style.module_separator);
        }
        segments.push(ProjectedSegment {
            text: style.badge(label),
            kind: if index == interaction.menu_selected {
                SegmentKind::Mode
            } else {
                SegmentKind::Module
            },
            tab_id: None,
            edit_cursor_offset: None,
        });
    }
}

fn append_tabs(
    output: &mut Vec<ProjectedSegment>,
    tokens: &[TabToken],
    tab: &TabTab,
    settings: &Settings,
    style: &RenderStyle,
    budget: usize,
) {
    let hidden_left = tab.start;
    let hidden_right = tokens.len().saturating_sub(tab.end);
    // Chrome must not consume the anchor's entire budget on narrow resizes.
    // Keep the marker only when it leaves room for the first tab's label.
    let leading_width = UnicodeWidthStr::width(
        style
            .overflow(hidden_left, settings.overflow_style)
            .as_str(),
    )
    .saturating_add(UnicodeWidthStr::width(style.tab_separator.as_str()));
    let anchor_width = tokens.get(tab.start).map_or(0, |token| token.width);
    if hidden_left > 0 && leading_width.saturating_add(anchor_width) <= budget {
        output.push(ProjectedSegment {
            text: style.overflow(hidden_left, settings.overflow_style),
            kind: SegmentKind::Overflow,
            tab_id: None,
            edit_cursor_offset: None,
        });
        push_separator(output, &style.tab_separator);
    }
    for (offset, token) in tokens[tab.start..tab.end].iter().enumerate() {
        if offset > 0 {
            push_separator(output, &style.tab_separator);
        }
        output.push(ProjectedSegment {
            text: token.text.clone(),
            kind: if token.edit_cursor_offset.is_some() || token.edit_selection.is_some() {
                SegmentKind::EditingTab
            } else {
                match (token.active, token.hovered) {
                    (true, true) => SegmentKind::HoveredActiveTab,
                    (true, false) => SegmentKind::ActiveTab,
                    (false, true) => SegmentKind::HoveredInactiveTab,
                    (false, false) => SegmentKind::InactiveTab,
                }
            },
            tab_id: Some(token.tab_id),
            edit_cursor_offset: token.edit_cursor_offset,
        });
    }
    if hidden_right > 0 {
        push_separator(output, &style.tab_separator);
        output.push(ProjectedSegment {
            text: style.overflow(hidden_right, settings.overflow_style),
            kind: SegmentKind::Overflow,
            tab_id: None,
            edit_cursor_offset: None,
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
            tab_id: None,
            edit_cursor_offset: None,
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
            tab_id: None,
            edit_cursor_offset: None,
        });
    }
    result
}

fn push_separator(output: &mut Vec<ProjectedSegment>, separator: &str) {
    output.push(ProjectedSegment {
        text: separator.to_string(),
        kind: SegmentKind::Base,
        tab_id: None,
        edit_cursor_offset: None,
    });
}

fn visible_tabs_for_layout(
    tokens: &[TabToken],
    settings: &Settings,
    style: &RenderStyle,
    budget: usize,
    scroll_anchor: Option<Uuid>,
    measurements: &mut MeasuredListIndex<Uuid>,
) -> TabTab {
    if tokens.is_empty() {
        measurements.sync([], 0, 0, |_| 0);
        return TabTab { start: 0, end: 0 };
    }
    // Index the horizontal main axis with the shared measured extent index.
    // Whole-tab admission and overflow chrome remain presentation policy here.
    let gap = UnicodeWidthStr::width(style.tab_separator.as_str()) as u64;
    if measurements.gap() != gap {
        *measurements = MeasuredListIndex::new(gap);
    }
    // Painting, hit testing, and wheel dispatch can project the same snapshot.
    // Reuse the index unchanged on those paths, including paint-only updates.
    if measurements.len() != tokens.len()
        || tokens.iter().enumerate().any(|(index, token)| {
            measurements
                .item(index)
                .is_none_or(|item| item.key != token.tab_id || item.height != token.width as u64)
        })
    {
        let widths: BTreeMap<_, _> = tokens
            .iter()
            .map(|token| (token.tab_id, token.width as u64))
            .collect();
        measurements.sync(
            tokens
                .iter()
                .map(|token| (token.tab_id, token.width as u64)),
            0,
            0,
            |key| widths[key],
        );
    }
    let anchor = scroll_anchor
        .filter(|key| measurements.index_of(key).is_some())
        .unwrap_or_else(|| {
            tokens
                .iter()
                .find(|token| token.active)
                .unwrap_or(&tokens[0])
                .tab_id
        });
    let range = bmux_tui_components::scroll_view::ScrollView::item_viewport(
        measurements,
        &anchor,
        bmux_tui_components::scroll_view::ItemViewportPolicy {
            extent: budget as u64,
            maximum_items: settings.maximum_visible_tabs.unwrap_or(usize::MAX),
            extend_before: scroll_anchor.is_none(),
            prefer_before: matches!(settings.align_active, ActiveAlignment::FocusBias),
        },
        |range| overflow_width(tokens.len(), &range, style, settings.overflow_style),
    )
    .expect("the selected anchor belongs to the synchronized measurements");
    TabTab {
        start: range.start,
        end: range.end,
    }
}

fn overflow_width(
    count: usize,
    range: &std::ops::Range<usize>,
    style: &RenderStyle,
    overflow_style: OverflowStyle,
) -> u64 {
    let separator = UnicodeWidthStr::width(style.tab_separator.as_str()) as u64;
    [range.start, count.saturating_sub(range.end)]
        .into_iter()
        .filter(|hidden| *hidden > 0)
        .fold(0_u64, |width, hidden| {
            width
                .saturating_add(UnicodeWidthStr::width(
                    style.overflow(hidden, overflow_style).as_str(),
                ) as u64)
                .saturating_add(separator)
        })
}

#[cfg(test)]
fn tab_window_width(
    measurements: &MeasuredListIndex<Uuid>,
    tab: &TabTab,
    style: &RenderStyle,
    overflow_style: OverflowStyle,
) -> u64 {
    let start = measurements.item_offset(tab.start).unwrap_or(0);
    let last = tab.end.saturating_sub(1);
    let end = measurements
        .item_offset(last)
        .unwrap_or(0)
        .saturating_add(measurements.item(last).map_or(0, |item| item.height));
    let mut width = end.saturating_sub(start);
    let separator = UnicodeWidthStr::width(style.tab_separator.as_str()) as u64;
    if tab.start > 0 {
        width = width
            .saturating_add(UnicodeWidthStr::width(
                style.overflow(tab.start, overflow_style).as_str(),
            ) as u64)
            .saturating_add(separator);
    }
    let hidden_right = measurements.len().saturating_sub(tab.end);
    if hidden_right > 0 {
        width = width
            .saturating_add(UnicodeWidthStr::width(
                style.overflow(hidden_right, overflow_style).as_str(),
            ) as u64)
            .saturating_add(separator);
    }
    width
}

// Reserve a display cell for the end cursor without changing the caller-owned draft.
fn editor_label(text: &str) -> String {
    format!("{text} ")
}

pub fn editing_entry(
    tab: &tabs_list::TabListEntry,
    draft: Option<&str>,
) -> tabs_list::TabListEntry {
    let mut entry = tab.clone();
    if let Some(draft) = draft {
        entry.name = editor_label(draft);
    }
    entry
}

fn render_tab_template(
    settings: &Settings,
    tab: &tabs_list::TabListEntry,
    index: usize,
    local: &AttachLocalPresentationSnapshot,
) -> String {
    render_tab_template_ranges(settings, tab, index, local).0
}

pub fn name_ranges(
    settings: &Settings,
    tab: &tabs_list::TabListEntry,
    index: usize,
    local: &AttachLocalPresentationSnapshot,
) -> Vec<(usize, usize)> {
    let style = RenderStyle::from_settings(settings);
    let prefix = if tab.active {
        style.active_prefix
    } else {
        style.inactive_prefix
    };
    let offset = UnicodeWidthStr::width(prefix);
    render_tab_template_ranges(settings, tab, index, local)
        .1
        .into_iter()
        .map(|(start, width)| (start + offset, width))
        .collect()
}

fn render_tab_template_ranges(
    settings: &Settings,
    tab: &tabs_list::TabListEntry,
    index: usize,
    local: &AttachLocalPresentationSnapshot,
) -> (String, Vec<(usize, usize)>) {
    let mut ranges = Vec::new();
    let name = truncate_cells(&tab.name, usize::from(settings.maximum_label_width));
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
                    "marker" => Some(if tab.active { "*" } else { "" }.to_string()),
                    "id" => Some(tab.id.to_string()),
                    "active" => Some(if tab.active { "active" } else { "idle" }.to_string()),
                    _ => None,
                };
                if terminated && let Some(value) = value {
                    if placeholder == "name" {
                        ranges.push((
                            UnicodeWidthStr::width(output.as_str()),
                            UnicodeWidthStr::width(value.as_str()),
                        ));
                    }
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
    (output, ranges)
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

    #[test]
    fn scroll_targets_remain_bound_to_the_projected_order() {
        let settings = Settings {
            maximum_visible_tabs: Some(1),
            ..Settings::default()
        };
        let measurements = ProjectionMeasurements::default();
        let interaction = ProjectionInteraction {
            measurements: Some(&measurements),
            ..ProjectionInteraction::default()
        };
        let mut windows = vec![
            tab(1, "one", true),
            tab(2, "two", false),
            tab(3, "three", false),
        ];
        let displayed = project_bar(&settings, &windows, &local(80), None, &interaction);
        assert_eq!(displayed.scroll_target(true), Some(Uuid::from_u128(2)));
        windows.swap(1, 2);
        let pending = project_bar(&settings, &windows, &local(80), None, &interaction);
        assert_eq!(pending.scroll_target(true), Some(Uuid::from_u128(3)));
        assert_eq!(displayed.scroll_target(true), Some(Uuid::from_u128(2)));
        windows.clear();
        project_bar(&settings, &windows, &local(80), None, &interaction);
        assert_eq!(displayed.scroll_target(true), Some(Uuid::from_u128(2)));
    }

    #[test]
    fn retained_projection_reconciles_width_order_and_empty_content() {
        let settings = Settings::default();
        let measurements = ProjectionMeasurements::default();
        let interaction = ProjectionInteraction {
            measurements: Some(&measurements),
            ..ProjectionInteraction::default()
        };
        let mut tabs = vec![tab(1, "one", true), tab(2, "two", false)];
        let initial = project_bar(&settings, &tabs, &local(80), None, &interaction);
        let index = measurements.0.borrow().clone();
        assert_eq!(index.len(), 2);
        assert_eq!(
            project_bar(&settings, &tabs, &local(80), None, &interaction),
            initial,
        );
        assert_eq!(*measurements.0.borrow(), index);
        tabs[1].name = "a substantially longer name".into();
        project_bar(&settings, &tabs, &local(80), None, &interaction);
        assert!(measurements.0.borrow().total_height() > index.total_height());
        tabs.reverse();
        project_bar(&settings, &tabs, &local(80), None, &interaction);
        assert_eq!(measurements.0.borrow().item(0).unwrap().key, tabs[0].id);
        let independent = measurements.clone();
        project_bar(&settings, &[], &local(80), None, &interaction);
        assert!(measurements.0.borrow().is_empty());
        assert_eq!(independent.0.borrow().len(), 2);
    }

    #[test]
    fn measured_tab_windows_preserve_gaps_and_overflow_at_wide_offsets() {
        let settings = Settings::default();
        let style = RenderStyle::from_settings(&settings);
        let gap = UnicodeWidthStr::width(style.tab_separator.as_str()) as u64;
        let mut measurements = MeasuredListIndex::new(gap);
        measurements.sync((1..=3).map(|key| (Uuid::from_u128(key), 0)), 0, 0, |_| {
            70_000
        });
        for start in 0..3 {
            for end in start + 1..=3 {
                let count = (end - start) as u64;
                let mut expected = count * 70_000 + (count - 1) * gap;
                for hidden in [start, 3 - end] {
                    if hidden > 0 {
                        expected += UnicodeWidthStr::width(
                            style.overflow(hidden, settings.overflow_style).as_str(),
                        ) as u64
                            + gap;
                    }
                }
                assert_eq!(
                    tab_window_width(
                        &measurements,
                        &TabTab { start, end },
                        &style,
                        settings.overflow_style,
                    ),
                    expected,
                );
            }
        }
    }

    fn tab(index: u128, name: &str, active: bool) -> tabs_list::TabListEntry {
        tabs_list::TabListEntry {
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
    fn default_modules_leave_narrow_manual_anchor_interactive() {
        let tabs = [tab(1, "first", true), tab(2, "second", false)];
        for width in 3..40 {
            let projected = project_bar(
                &Settings::default(),
                &tabs,
                &local(width),
                None,
                &ProjectionInteraction {
                    scroll_anchor: Some(Uuid::from_u128(2)),
                    ..ProjectionInteraction::default()
                },
            );
            assert_eq!(
                projected.tab_ranges().first().map(|range| range.tab_id),
                Some(tabs[1].id),
                "width {width}: {}",
                projected.plain_text()
            );
            assert_eq!(
                UnicodeWidthStr::width(projected.plain_text().as_str()),
                usize::from(width)
            );
        }
    }

    #[test]
    fn narrow_manual_anchor_takes_priority_over_overflow_marker() {
        let settings = Settings {
            show_mode: false,
            show_role: false,
            show_follow: false,
            show_hint: false,
            ..Settings::default()
        };
        let tabs = [tab(1, "first", true), tab(2, "second", false)];
        for width in 6..20 {
            let projected = project_bar(
                &settings,
                &tabs,
                &local(width),
                None,
                &ProjectionInteraction {
                    scroll_anchor: Some(Uuid::from_u128(2)),
                    ..ProjectionInteraction::default()
                },
            );
            assert_eq!(
                projected.tab_ranges().first().map(|range| range.tab_id),
                Some(tabs[1].id),
                "width {width}: {}",
                projected.plain_text()
            );
            assert_eq!(
                UnicodeWidthStr::width(projected.plain_text().as_str()),
                usize::from(width)
            );
        }
    }

    #[test]
    fn default_projection_omits_session_and_context_modules() {
        let settings = Settings::default();
        let tabs = [tab(1, "main", true)];
        let local = AttachLocalPresentationSnapshot {
            session_label: Some("bcode".to_string()),
            session_count: 1,
            context_label: Some("bcode".to_string()),
            viewport_cols: 80,
            ..AttachLocalPresentationSnapshot::initial()
        };
        let projected = project_bar(
            &settings,
            &tabs,
            &local,
            None,
            &ProjectionInteraction::default(),
        );
        let text = projected.plain_text();
        assert!(!text.contains("session:"), "{text}");
        assert!(!text.contains("context:"), "{text}");
        assert!(!text.contains("ctx:"), "{text}");
    }

    #[test]
    fn default_projection_is_full_width_and_includes_mode_and_role() {
        let projected = project_bar(
            &Settings::default(),
            &[tab(1, "main", true)],
            &local(40),
            None,
            &ProjectionInteraction::default(),
        );
        let text = projected.plain_text();
        assert_eq!(UnicodeWidthStr::width(text.as_str()), 40);
        assert!(text.contains(" main "));
        assert!(text.contains(" NORMAL "));
        assert!(text.contains(" write "));
    }

    #[test]
    fn empty_workspace_keeps_noninteractive_label() {
        let projected = project_bar(
            &Settings::default(),
            &[],
            &local(80),
            None,
            &ProjectionInteraction {
                workspace_label: Some("project"),
                ..ProjectionInteraction::default()
            },
        );
        assert!(projected.plain_text().contains("project │"));
        assert!(projected.plain_text().contains("[no tabs]"));
        assert!(
            projected
                .segments
                .iter()
                .all(|segment| segment.tab_id.is_none())
        );
    }

    #[test]
    fn narrow_projection_keeps_active_tab_and_uses_overflow() {
        let tabs = (0..8)
            .map(|index| tab(index + 1, &format!("tab-{index}"), index == 7))
            .collect::<Vec<_>>();
        let projected = project_bar(
            &Settings::default(),
            &tabs,
            &local(50),
            None,
            &ProjectionInteraction::default(),
        );
        let text = projected.plain_text();
        assert_eq!(UnicodeWidthStr::width(text.as_str()), 50);
        assert!(text.contains("default"));
        assert!(text.contains("tab-7"));
        assert!(text.contains('◀'));
    }

    #[test]
    fn right_module_zone_survives_every_representative_width() {
        let tabs = (0..12)
            .map(|index| tab(index + 1, &format!("tab-{index}"), index == 11))
            .collect::<Vec<_>>();
        for width in [20, 40, 80, 120, 240] {
            let projected = project_bar(
                &Settings::default(),
                &tabs,
                &local(width),
                None,
                &ProjectionInteraction::default(),
            );
            let text = projected.plain_text();
            assert_eq!(UnicodeWidthStr::width(text.as_str()), usize::from(width));
            assert!(text.contains("NORMAL"), "width {width}: {text:?}");
            if width >= 40 {
                assert!(text.contains("write"), "width {width}: {text:?}");
            }
            assert!(
                !projected.tab_ranges().is_empty(),
                "width {width}: {text:?}"
            );
        }
    }

    #[test]
    fn local_modes_follow_hint_policy_and_optional_modules() {
        let settings = Settings {
            show_session_name: true,
            show_context_name: true,
            ..Settings::default()
        };
        let local = AttachLocalPresentationSnapshot {
            mode_id: "scroll".to_string(),
            mode_label: "SCROLL".to_string(),
            role_label: "read-only".to_string(),
            follow_label: Some("following abcdef12".to_string()),
            mode_modifier: Some("FROZEN".to_string()),
            hint: "scroll hint".to_string(),
            session_label: Some("session".to_string()),
            session_count: 2,
            context_label: Some("context".to_string()),
            viewport_cols: 120,
            ..AttachLocalPresentationSnapshot::initial()
        };
        let text = project_bar(
            &settings,
            &[tab(1, "main", true)],
            &local,
            None,
            &ProjectionInteraction::default(),
        )
        .plain_text();
        for expected in [
            "session:session (2)",
            "ctx:context",
            "SCROLL",
            "FROZEN",
            "read-only",
            "following abcdef12",
            "scroll hint",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text:?}");
        }
    }

    #[test]
    fn drop_target_uses_tab_halves_and_exposes_insertion_column() {
        let settings = Settings::default();
        let tabs = [tab(1, "one", true), tab(2, "two", false)];
        let projected = project_bar(
            &settings,
            &tabs,
            &local(80),
            None,
            &ProjectionInteraction::default(),
        );
        let ranges = projected.tab_ranges();
        let second = &ranges[1];
        assert_eq!(
            projected.drop_target_at_col(second.start),
            Some(ResolvedInsertion {
                tab_id: second.tab_id,
                side: DropSide::Before,
                marker_col: second.start
            })
        );
        assert_eq!(
            projected.drop_target_at_col(second.end.saturating_sub(1)),
            Some(ResolvedInsertion {
                tab_id: second.tab_id,
                side: DropSide::After,
                marker_col: second.end
            })
        );
    }

    #[test]
    fn insertion_geometry_resolves_gaps_ties_and_outer_edges() {
        let geometry = TabBarGeometry {
            tabs: vec![
                ProjectedTabRange {
                    tab_id: Uuid::from_u128(1),
                    start: 4,
                    end: 10,
                },
                ProjectedTabRange {
                    tab_id: Uuid::from_u128(2),
                    start: 13,
                    end: 19,
                },
                ProjectedTabRange {
                    tab_id: Uuid::from_u128(3),
                    start: 20,
                    end: 26,
                },
            ],
        };
        for (col, id, side, marker_col) in [
            (0, 1, DropSide::Before, 4),
            (4, 1, DropSide::Before, 4),
            (7, 1, DropSide::After, 10),
            (10, 1, DropSide::After, 10),
            (11, 2, DropSide::Before, 13),
            (12, 2, DropSide::Before, 13),
            (19, 3, DropSide::Before, 20),
            (u16::MAX, 3, DropSide::After, 26),
        ] {
            assert_eq!(
                geometry.resolve_insertion(col),
                Some(ResolvedInsertion {
                    tab_id: Uuid::from_u128(id),
                    side,
                    marker_col,
                }),
                "column {col}"
            );
        }
        assert_eq!(TabBarGeometry { tabs: vec![] }.resolve_insertion(0), None);
    }

    #[test]
    fn insertion_ignores_separator_text_and_uses_display_cell_widths() {
        for separator in [">", "│", "界", "   ", " → ", "", "e\u{301}"] {
            let projected = ProjectedBar::new(vec![
                ProjectedSegment {
                    text: "界ab".into(),
                    kind: SegmentKind::ActiveTab,
                    tab_id: Some(Uuid::from_u128(1)),
                    edit_cursor_offset: None,
                },
                ProjectedSegment {
                    text: separator.into(),
                    kind: SegmentKind::Base,
                    tab_id: None,
                    edit_cursor_offset: None,
                },
                ProjectedSegment {
                    text: "next".into(),
                    kind: SegmentKind::InactiveTab,
                    tab_id: Some(Uuid::from_u128(2)),
                    edit_cursor_offset: None,
                },
            ]);
            let ranges = projected.tab_ranges();
            assert_eq!(ranges[0].end, 4);
            for col in ranges[0].end..ranges[1].start {
                let insertion = projected.drop_target_at_col(col).unwrap();
                assert!(
                    insertion
                        == ResolvedInsertion {
                            tab_id: ranges[0].tab_id,
                            side: DropSide::After,
                            marker_col: ranges[0].end
                        }
                        || insertion
                            == ResolvedInsertion {
                                tab_id: ranges[1].tab_id,
                                side: DropSide::Before,
                                marker_col: ranges[1].start
                            }
                );
            }
        }
    }

    #[test]
    fn rename_preserves_layout_across_presets_templates_and_overflow() {
        for preset in [Preset::TabRail, Preset::Minimal, Preset::Classic] {
            for active in [false, true] {
                for width in [25, 40, 80] {
                    let settings = Settings {
                        preset,
                        label_template: "{index}: {name} ({name})".to_string(),
                        ..Settings::default()
                    };
                    let tabs = [tab(1, "界hello", active), tab(2, "other", !active)];
                    let normal = project_bar(
                        &settings,
                        &tabs,
                        &local(width),
                        None,
                        &ProjectionInteraction::default(),
                    );
                    for interaction in [
                        ProjectionInteraction {
                            editing_tab_id: Some(tabs[0].id),
                            ..ProjectionInteraction::default()
                        },
                        ProjectionInteraction {
                            editing_workspace: true,
                            ..ProjectionInteraction::default()
                        },
                    ] {
                        let edited =
                            project_bar(&settings, &tabs, &local(width), None, &interaction);
                        assert_eq!(normal.plain_text(), edited.plain_text());
                        let normal_ranges = normal
                            .tab_ranges()
                            .into_iter()
                            .map(|r| (r.tab_id, r.start, r.end))
                            .collect::<Vec<_>>();
                        let edited_ranges = edited
                            .tab_ranges()
                            .into_iter()
                            .map(|r| (r.tab_id, r.start, r.end))
                            .collect::<Vec<_>>();
                        assert_eq!(normal_ranges, edited_ranges);
                    }
                    let ranges = name_ranges(&settings, &tabs[0], 0, &local(width));
                    assert_eq!(ranges.len(), 2);
                    assert_eq!(ranges[0].1, 7);
                    assert!(ranges[1].0 > ranges[0].0);
                }
            }
        }
    }

    #[test]
    fn live_drafts_resize_tabs_and_workspaces_with_bounded_cell_geometry() {
        let settings = Settings::default();
        let tabs = [tab(1, "saved", true), tab(2, "other", false)];
        for workspace in [false, true] {
            let mut widths = Vec::new();
            for text in ["x", "界界long", "", "x"] {
                let interaction = ProjectionInteraction {
                    editing_tab_id: (!workspace).then_some(tabs[0].id),
                    editing_workspace: workspace,
                    edit_text: Some(text),
                    ..ProjectionInteraction::default()
                };
                let projected = project_bar(&settings, &tabs, &local(120), None, &interaction);
                let kind = if workspace {
                    SegmentKind::EditingWorkspace
                } else {
                    SegmentKind::EditingTab
                };
                let segment = projected
                    .segments
                    .iter()
                    .find(|segment| segment.kind == kind)
                    .unwrap();
                widths.push(UnicodeWidthStr::width(segment.text.as_str()));
                for width in [1, 8, 25] {
                    let narrow = project_bar(&settings, &tabs, &local(width), None, &interaction);
                    assert!(
                        UnicodeWidthStr::width(narrow.plain_text().as_str()) <= usize::from(width)
                    );
                }
            }
            assert!(widths[1] > widths[0]);
            assert!(widths[2] < widths[0]);
            assert_eq!(widths[0], widths[3]);
            assert!(widths[2] >= 1);
        }
        assert_eq!(tabs[0].name, "saved");
    }

    #[test]
    fn editing_and_menu_state_are_projected() {
        let mut interaction = ProjectionInteraction {
            editing_tab_id: Some(Uuid::from_u128(1)),
            edit_selection: Some((0, 7)),
            ..ProjectionInteraction::default()
        };
        let settings = Settings::default();
        let tabs = [tab(1, "main", true)];
        let edited = project_bar(&settings, &tabs, &local(80), None, &interaction);
        assert_eq!(
            edited.plain_text(),
            project_bar(
                &settings,
                &tabs,
                &local(80),
                None,
                &ProjectionInteraction::default()
            )
            .plain_text()
        );
        assert!(edited.segments.iter().any(|segment| {
            segment.kind == SegmentKind::EditingTab && segment.edit_cursor_offset.is_some()
        }));

        interaction.menu_tab_id = Some(Uuid::from_u128(1));
        interaction.menu_selected = 1;
        let menu = project_bar(&settings, &tabs, &local(120), None, &interaction);
        let text = menu.plain_text();
        assert!(text.contains("Switch"));
        assert!(text.contains("Rename"));
        assert!(text.contains("Close"));
    }

    #[test]
    fn empty_unicode_hover_and_notification_cases_are_stable() {
        let settings = Settings::default();
        let empty = project_bar(
            &settings,
            &[],
            &local(40),
            None,
            &ProjectionInteraction::default(),
        );
        assert!(empty.plain_text().contains("[no tabs]"));

        let unicode = [tab(1, "界e\u{301}", true), tab(2, "other", false)];
        let hovered = project_bar(
            &settings,
            &unicode,
            &local(80),
            Some(Uuid::from_u128(2)),
            &ProjectionInteraction::default(),
        );
        assert!(hovered.plain_text().contains("界e\u{301}"));
        assert!(hovered.segments.iter().any(|segment| {
            segment.tab_id == Some(Uuid::from_u128(2))
                && segment.kind == SegmentKind::HoveredInactiveTab
        }));

        let notification_settings = Settings {
            hint_policy: HintPolicy::Always,
            ..Settings::default()
        };
        let notification = AttachLocalPresentationSnapshot {
            hint: "saved".to_string(),
            viewport_cols: 80,
            ..AttachLocalPresentationSnapshot::initial()
        };
        assert!(
            project_bar(
                &notification_settings,
                &unicode,
                &notification,
                None,
                &ProjectionInteraction::default(),
            )
            .plain_text()
            .contains("saved")
        );
    }

    #[test]
    #[ignore = "manual release projection benchmark; run with --release --ignored --nocapture"]
    fn projection_performance_baseline() {
        use std::hint::black_box;
        use std::time::Instant;

        const ITERATIONS: u32 = 20_000;
        let one_tab = vec![tab(1, "one", true)];
        let tabs = (0..64)
            .map(|index| tab(index + 1, &format!("tab-{index}"), index == 32))
            .collect::<Vec<_>>();
        let settings = Settings::default();
        let local = local(240);
        let measure = |tabs: &[tabs_list::TabListEntry],
                       hovered: Option<Uuid>,
                       interaction: &ProjectionInteraction<'_>,
                       local: &AttachLocalPresentationSnapshot| {
            let started = Instant::now();
            let mut bytes = 0_usize;
            for _ in 0..ITERATIONS {
                let projected = project_bar(
                    black_box(&settings),
                    black_box(tabs),
                    black_box(local),
                    hovered,
                    interaction,
                );
                bytes = bytes.saturating_add(projected.plain_text().len());
                black_box(projected);
            }
            (started.elapsed().as_nanos() / u128::from(ITERATIONS), bytes)
        };
        let default_interaction = ProjectionInteraction::default();
        let (one_ns, one_bytes) = measure(&one_tab, None, &default_interaction, &local);
        let (many_ns, many_bytes) = measure(&tabs, None, &default_interaction, &local);
        let (idle_ns, idle_bytes) = measure(&tabs, None, &default_interaction, &local);
        let (hover_ns, hover_bytes) = measure(
            &tabs,
            Some(Uuid::from_u128(41)),
            &default_interaction,
            &local,
        );
        let mut reordered = tabs.clone();
        reordered.rotate_left(17);
        let (reorder_ns, reorder_bytes) = measure(&reordered, None, &default_interaction, &local);
        let editing = ProjectionInteraction {
            editing_tab_id: Some(Uuid::from_u128(33)),
            ..ProjectionInteraction::default()
        };
        let (rename_ns, rename_bytes) = measure(&tabs, None, &editing, &local);
        let module_local = AttachLocalPresentationSnapshot {
            mode_label: "SCROLL".to_string(),
            role_label: "read-only".to_string(),
            mode_modifier: Some("FROZEN".to_string()),
            hint: "scroll hint".to_string(),
            viewport_cols: 240,
            ..AttachLocalPresentationSnapshot::initial()
        };
        let (module_ns, module_bytes) = measure(&tabs, None, &default_interaction, &module_local);
        println!(
            "projection iterations={ITERATIONS} one_ns={one_ns} one_bytes={one_bytes} many_ns={many_ns} many_bytes={many_bytes} idle_ns={idle_ns} idle_bytes={idle_bytes} hover_ns={hover_ns} hover_bytes={hover_bytes} reorder_ns={reorder_ns} reorder_bytes={reorder_bytes} rename_ns={rename_ns} rename_bytes={rename_bytes} module_ns={module_ns} module_bytes={module_bytes}"
        );
        for (label, average_ns) in [
            ("one", one_ns),
            ("many", many_ns),
            ("idle", idle_ns),
            ("hover", hover_ns),
            ("reorder", reorder_ns),
            ("rename", rename_ns),
            ("module", module_ns),
        ] {
            assert!(
                average_ns <= 35_000,
                "{label} projection average {average_ns}ns exceeded 35us budget"
            );
        }
    }

    #[test]
    fn legacy_template_escaping_and_unknown_tokens_are_preserved() {
        let settings = Settings {
            label_template: "{{{index}}}:{name}:{unknown}".to_string(),
            ..Settings::default()
        };
        let projected = project_bar(
            &settings,
            &[tab(1, "main", true)],
            &local(80),
            None,
            &ProjectionInteraction::default(),
        );
        let text = projected.plain_text();
        assert!(text.contains("{1}:main:{unknown}"));
    }
}
