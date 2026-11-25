//! Window controls for panels: minimize, maximize, restore and close.
//!
//! Everything here is opt-in; panes and panel groups that do not use it are unaffected.
//!
//! * **Title-row controls.** [`WindowControls`] appends a panel's controls to a [`Pane`] as its
//!   last title-row actions, one cluster joined to the border (`┤ ▾ ⤢ × ├`), left to right:
//!   minimize (restore while minimized), maximize (restore while maximized) and close. Their
//!   painting, their hit regions (`<pane id>.action.minimize`, `.maximize`, `.close`) and
//!   [`WindowControls::target`] all come from the pane's one title layout. The caller decides
//!   when they show ([`WindowControls::shown`], conventionally on the focused or hovered panel;
//!   a minimized panel always shows its restore control) and which one is hovered
//!   ([`WindowControls::hovered`]). The title keeps [`WindowControls::min_title_width`] cells
//!   (or all of a shorter title): with less room the pane drops the leftmost actions first
//!   (the panel's own actions, then minimize, then maximize; close goes last, taking the caps
//!   with it), then clips the title.
//! * **Shapes.** A minimized panel collapses along the axis of the group it sits in
//!   ([`PanelShape::minimized_along`]): to its title row when stacked ([`PanelShape::Row`]), or
//!   to a strip [`WindowControls::strip_width`] columns wide when side by side
//!   ([`PanelShape::Strip`]), showing only its restore control and its name down the strip
//!   ([`minimized_content`]).
//! * **Windows.** [`PanelWindows`] keeps which panels are minimized and which is maximized for
//!   a tree of nested panel groups ([`PanelTree`]), resolves how each panel is shown, builds
//!   each group's [`PanelGroupState`] with its minimized panels collapsed and the maximized
//!   branch laid out alone, and runs [`WindowCommand`]s on a panel.
//! * **Double clicks.** [`TitleClicks`] detects two presses on the same panel's title row within
//!   [`DOUBLE_CLICK`], which conventionally toggle maximize.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use bmux_tui::composition::{Column, TextBlock};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span, Text, TextWrap};

use crate::pane::{Pane, PaneTitleAction, PaneTitleGroup};
use crate::panel_group::{PanelGroupAxis, PanelGroupConstraints, PanelGroupState, PanelSize};

/// Two presses on a panel's title row within this time make a double click.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Cells of a panel's title its controls leave it at least, unless configured otherwise.
pub const DEFAULT_MIN_TITLE: u16 = 16;

/// How a panel is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelShape {
    /// At its size.
    #[default]
    Normal,
    /// Minimized to its title row (in a stacked group).
    Row,
    /// Minimized to a strip (in a side-by-side group).
    Strip,
    /// Maximized.
    Maximized,
}

impl PanelShape {
    /// Whether the panel is minimized (a row or a strip).
    #[must_use]
    pub const fn minimized(self) -> bool {
        matches!(self, Self::Row | Self::Strip)
    }

    /// The shape of a panel minimized in a group laid out along `axis`.
    #[must_use]
    pub const fn minimized_along(axis: PanelGroupAxis) -> Self {
        match axis {
            PanelGroupAxis::Horizontal => Self::Strip,
            PanelGroupAxis::Vertical => Self::Row,
        }
    }
}

/// A title-row window control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WindowControl {
    /// Minimize, or restore a minimized panel.
    Minimize,
    /// Maximize, or restore a maximized panel.
    Maximize,
    /// Close the panel.
    Close,
}

impl WindowControl {
    /// Every control, in title-row order.
    pub const ALL: [Self; 3] = [Self::Minimize, Self::Maximize, Self::Close];

    /// The title action id (the hit region is `<pane id>.action.<id>`).
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Maximize => "maximize",
            Self::Close => "close",
        }
    }

    /// The control with title action id `id`.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|control| control.id() == id)
    }

    /// The command the control runs.
    #[must_use]
    pub const fn command(self) -> WindowCommand {
        match self {
            Self::Minimize => WindowCommand::Minimize,
            Self::Maximize => WindowCommand::Maximize,
            Self::Close => WindowCommand::Close,
        }
    }
}

/// Glyphs of the window controls, and the caps that join their cluster to the border.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowGlyphs {
    /// The minimize control.
    pub minimize: &'static str,
    /// The restore control of a minimized panel.
    pub restore_minimized: &'static str,
    /// The maximize control.
    pub maximize: &'static str,
    /// The restore control of a maximized panel.
    pub restore_maximized: &'static str,
    /// The close control.
    pub close: &'static str,
    /// The cap before the cluster, joining the border line on its left.
    pub cap_start: &'static str,
    /// The cap after the cluster, joining the border line on its right.
    pub cap_end: &'static str,
}

impl WindowGlyphs {
    /// Symbol glyphs, with caps for a single-line border.
    pub const UNICODE: Self = Self {
        minimize: "▾",
        restore_minimized: "▸",
        maximize: "⤢",
        restore_maximized: "⤡",
        close: "×",
        cap_start: "┤",
        cap_end: "├",
    };

    /// ASCII glyphs, for terminals without the symbols.
    pub const ASCII: Self = Self {
        minimize: "v",
        restore_minimized: ">",
        maximize: "+",
        restore_maximized: "=",
        close: "x",
        cap_start: "|",
        cap_end: "|",
    };
}

impl Default for WindowGlyphs {
    fn default() -> Self {
        Self::UNICODE
    }
}

/// The window controls of one panel in one shape.
///
/// On a title row the controls form one cluster, `┤ ▾ ⤢ × ├`: the caps join the border, a
/// space sits between the glyphs, and a border cell separates the cluster from the title and
/// the right title. A minimized strip shows restore by default and can also offer close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowControls {
    shape: PanelShape,
    options: WindowControlOptions,
    shown: bool,
    glyphs: WindowGlyphs,
    style: Style,
    hover: Style,
    close_hover: Style,
    hovered: Option<WindowControl>,
    min_title: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowControlOptions {
    closable: bool,
    maximizable: bool,
    close_in_strip: bool,
}

impl WindowControls {
    /// Shown controls of a closable panel shown as `shape`, with [`WindowGlyphs::UNICODE`],
    /// no style, bold while hovered, and a [`DEFAULT_MIN_TITLE`] title.
    #[must_use]
    pub const fn new(shape: PanelShape) -> Self {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        Self {
            shape,
            options: WindowControlOptions {
                closable: true,
                maximizable: true,
                close_in_strip: false,
            },
            shown: true,
            glyphs: WindowGlyphs::UNICODE,
            style: Style::new(),
            hover: bold,
            close_hover: bold,
            hovered: None,
            min_title: DEFAULT_MIN_TITLE,
        }
    }

    /// Whether the panel offers a close control.
    #[must_use]
    pub const fn closable(mut self, closable: bool) -> Self {
        self.options.closable = closable;
        self
    }

    /// Whether the expanded panel offers a maximize control.
    #[must_use]
    pub const fn maximizable(mut self, maximizable: bool) -> Self {
        self.options.maximizable = maximizable;
        self
    }

    /// Whether a minimized strip also offers its close control.
    #[must_use]
    pub const fn close_in_strip(mut self, close_in_strip: bool) -> Self {
        self.options.close_in_strip = close_in_strip;
        self
    }

    /// Whether the controls are shown (the default). Applications conventionally show them
    /// only on the focused panel and the panel under the pointer, so other panels' title rows
    /// keep the room. A minimized panel always shows its restore control, since nothing else
    /// on it restores it by pointer. Hidden controls are not added to the pane, so they have
    /// no hit regions and [`Self::target`] finds none.
    #[must_use]
    pub const fn shown(mut self, shown: bool) -> Self {
        self.shown = shown;
        self
    }

    /// The glyphs of the controls.
    #[must_use]
    pub const fn glyphs(mut self, glyphs: WindowGlyphs) -> Self {
        self.glyphs = glyphs;
        self
    }

    /// The style of the controls' glyphs.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// The styles layered over a hovered control: `hover` for minimize and maximize,
    /// `close_hover` for close.
    #[must_use]
    pub const fn hover_styles(mut self, hover: Style, close_hover: Style) -> Self {
        self.hover = hover;
        self.close_hover = close_hover;
        self
    }

    /// The control under the pointer, if any (see [`Self::target`]).
    #[must_use]
    pub const fn hovered(mut self, control: Option<WindowControl>) -> Self {
        self.hovered = control;
        self
    }

    /// Cells of the title the controls leave it at least (the whole title when it is
    /// shorter); controls that would leave it less drop, leftmost first. A strip ignores it.
    #[must_use]
    pub const fn min_title_width(mut self, cells: u16) -> Self {
        self.min_title = cells;
        self
    }

    /// How the panel is shown.
    #[must_use]
    pub const fn shape(&self) -> PanelShape {
        self.shape
    }

    /// Whether the controls are painted: shown, or the panel is minimized.
    #[must_use]
    pub const fn is_shown(&self) -> bool {
        self.shown || self.shape.minimized()
    }

    /// The controls the panel offers in title-row order.
    #[must_use]
    pub fn list(&self) -> Vec<WindowControl> {
        let mut list = vec![WindowControl::Minimize];
        if !self.shape.minimized() && self.options.maximizable {
            list.push(WindowControl::Maximize);
        }
        if self.options.closable
            && (!self.shape.minimized()
                || (self.shape == PanelShape::Strip && self.options.close_in_strip))
        {
            list.push(WindowControl::Close);
        }
        list
    }

    /// The controls [`Self::apply`] adds: [`Self::list`] while [`Self::is_shown`], else none.
    #[must_use]
    pub fn painted(&self) -> Vec<WindowControl> {
        if self.is_shown() {
            self.list()
        } else {
            Vec::new()
        }
    }

    /// The glyph of `control` in this shape.
    #[must_use]
    pub const fn glyph(&self, control: WindowControl) -> &'static str {
        match control {
            WindowControl::Minimize if self.shape.minimized() => self.glyphs.restore_minimized,
            WindowControl::Minimize => self.glyphs.minimize,
            WindowControl::Maximize if matches!(self.shape, PanelShape::Maximized) => {
                self.glyphs.restore_maximized
            }
            WindowControl::Maximize => self.glyphs.maximize,
            WindowControl::Close => self.glyphs.close,
        }
    }

    /// The label of `control`: its glyph, padded by a space on each side in a strip.
    #[must_use]
    pub fn label(&self, control: WindowControl) -> Line {
        let glyph = self.glyph(control);
        if self.shape == PanelShape::Strip {
            Line::from_spans(vec![Span::styled(format!(" {glyph} "), self.style)])
        } else {
            Line::from_spans(vec![Span::styled(glyph, self.style)])
        }
    }

    /// Paint controls at the end of a borderless title row. Returns the first
    /// occupied column so the caller can fit the rest of the title around them.
    pub fn paint_inline(
        &self,
        id: &str,
        area: Rect,
        cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
    ) -> u16 {
        use bmux_tui::hit::{HitRegion, HitRole};
        use bmux_tui::paint::LocalRect;
        use bmux_tui::pointer::PointerIntent;

        let controls = self.painted();
        let glyph_width =
            |control| u16::try_from(Line::raw(self.glyph(control)).width()).unwrap_or(u16::MAX);
        let total = controls
            .iter()
            .copied()
            .map(glyph_width)
            .fold(0, u16::saturating_add);
        let gaps = u16::try_from(controls.len().saturating_sub(1)).unwrap_or(u16::MAX);
        let gap = u16::from(total.saturating_add(gaps) <= area.width);
        let mut x = area
            .right()
            .saturating_sub(total.saturating_add(gaps * gap))
            .max(area.x);
        let start = x;
        for control in controls {
            let width = glyph_width(control);
            if x.saturating_add(width) > area.right() || area.height == 0 {
                break;
            }
            let style = if self.hovered == Some(control) {
                self.style.patch(if control == WindowControl::Close {
                    self.close_hover
                } else {
                    self.hover
                })
            } else {
                self.style
            };
            let line = Line::from_spans(vec![Span::styled(self.glyph(control), style)]);
            cx.write_line_with_fallback_style(
                LocalRect::new(i32::from(x), i64::from(area.y), width, 1),
                &line,
                style,
            );
            cx.push_hit(
                HitRegion::new(
                    format!("{id}.action.{}", control.id()),
                    Rect::new(x, area.y, width, 1),
                )
                .role(HitRole::Action)
                .pointer_intent(PointerIntent::Auto),
            );
            x = x.saturating_add(width).saturating_add(gap);
        }
        start
    }

    /// The cluster's caps and separator: each cap a space inside its border glyph.
    #[must_use]
    pub fn group(&self) -> PaneTitleGroup {
        PaneTitleGroup::new(
            format!("{} ", self.glyphs.cap_start),
            " ",
            format!(" {}", self.glyphs.cap_end),
        )
    }

    /// Width of a minimized strip: its padded controls between the pane's two corners.
    #[must_use]
    pub fn strip_width(&self) -> u16 {
        let strip = Self {
            shape: PanelShape::Strip,
            ..*self
        };
        let width = strip
            .list()
            .into_iter()
            .map(|control| strip.label(control).width())
            .sum::<usize>();
        u16::try_from(width).unwrap_or(u16::MAX).saturating_add(2)
    }

    /// The title action of `control`.
    fn action(&self, control: WindowControl) -> PaneTitleAction {
        let hover = if control == WindowControl::Close {
            self.close_hover
        } else {
            self.hover
        };
        PaneTitleAction::new(control.id(), self.label(control)).hover_style(hover)
    }

    /// `pane` with the painted controls appended as its last title-row actions: one cluster
    /// ([`Self::group`]) on a title row, or individual controls in a strip. Unless the
    /// panel is a strip, the title keeps [`Self::min_title_width`].
    #[must_use]
    pub fn apply<'a>(&self, mut pane: Pane<'a>) -> Pane<'a> {
        let painted = self.painted();
        if self.shape == PanelShape::Strip {
            for control in painted {
                pane = pane.action(self.action(control));
            }
        } else {
            pane = pane.min_title_width(self.min_title);
            if !painted.is_empty() {
                pane = pane.action_group(
                    self.group(),
                    painted.iter().map(|control| self.action(*control)),
                );
            }
        }
        match self
            .hovered
            .filter(|control| self.painted().contains(control))
        {
            Some(control) => pane.hovered_action(Some(control.id())),
            None => pane,
        }
    }

    /// What `position` is on in the title row of `pane` (built with [`Self::apply`]) laid out
    /// at `area` (absolute): a painted control, one of the pane's own actions, or elsewhere on
    /// the row. It hit tests the pane's own title layout, so it agrees with what the pane
    /// paints. `None` off the title row, and on its corners (which a divider may cover).
    #[must_use]
    pub fn target(&self, pane: &Pane<'_>, area: Rect, position: Point) -> Option<TitleTarget> {
        if area.width < 3
            || area.height == 0
            || position.y != area.y
            || position.x <= area.x
            || position.x >= area.right().saturating_sub(1)
        {
            return None;
        }
        let list = self.painted();
        let own = pane.actions().len().saturating_sub(list.len());
        let layout = pane.title_layout(Rect::new(0, 0, area.width, area.height));
        for (index, rect) in layout.actions {
            let absolute = Rect::new(area.x + rect.x, area.y + rect.y, rect.width, rect.height);
            if absolute.contains(position) {
                return Some(
                    index
                        .checked_sub(own)
                        .and_then(|slot| list.get(slot).copied())
                        .map_or(TitleTarget::Action(index), TitleTarget::Control),
                );
            }
        }
        Some(TitleTarget::Title)
    }
}

/// What a press on a pane's title row is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleTarget {
    /// One of the window controls.
    Control(WindowControl),
    /// One of the pane's own title actions (index into them).
    Action(usize),
    /// Elsewhere on the title row, inside the corners.
    Title,
}

/// The content of a panel minimized as `shape`, `None` while it is not minimized.
///
/// For a strip, the panel's `name` down the strip, a grapheme a row after a cell of padding,
/// clipped to the strip by the layout; for a row, nothing (the pane shows only its title row).
#[must_use]
pub fn minimized_content(
    id: impl Into<bmux_tui::component::LayoutId>,
    shape: PanelShape,
    name: &str,
    style: Style,
) -> Option<Column<'static>> {
    match shape {
        PanelShape::Strip => {
            let mut column = Column::new().id(id);
            for grapheme in bmux_unicode_segmentation::graphemes(name) {
                column = column.child(
                    TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                        Span::styled(format!(" {grapheme}"), style),
                    ])]))
                    .wrap(TextWrap::None),
                );
            }
            Some(column)
        }
        PanelShape::Row => Some(Column::new().id(id)),
        PanelShape::Normal | PanelShape::Maximized => None,
    }
}

/// Double-click detection on panel title rows, keyed by panel.
#[derive(Debug, Clone, Copy)]
pub struct TitleClicks<K> {
    last: Option<(K, Instant)>,
}

impl<K> Default for TitleClicks<K> {
    fn default() -> Self {
        Self { last: None }
    }
}

impl<K: PartialEq + Copy> TitleClicks<K> {
    /// A press on `panel`'s title row now: whether it completes a double click.
    pub fn press(&mut self, panel: K) -> bool {
        self.press_at(panel, Instant::now())
    }

    /// A press on `panel`'s title row at `now`: whether it completes a double click (two
    /// presses on the same panel within [`DOUBLE_CLICK`]). A completed double click starts
    /// over, so a third press is a first press again.
    pub fn press_at(&mut self, panel: K, now: Instant) -> bool {
        let double = self.last.is_some_and(|(last, at)| {
            last == panel && now.saturating_duration_since(at) <= DOUBLE_CLICK
        });
        self.last = (!double).then_some((panel, now));
        double
    }

    /// Forget the last press (the title row moved or went away).
    pub const fn reset(&mut self) {
        self.last = None;
    }
}

/// A window command acting on one panel (from a control, a key or a menu).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCommand {
    /// Maximize, or restore a maximized panel.
    Maximize,
    /// Minimize, or restore a minimized panel.
    Minimize,
    /// Close the panel.
    Close,
    /// Grow the panel.
    Grow,
    /// Shrink the panel.
    Shrink,
    /// Forget the panel's resized size.
    ResetSize,
}

impl WindowCommand {
    /// Every command.
    pub const ALL: [Self; 6] = [
        Self::Maximize,
        Self::Minimize,
        Self::Close,
        Self::Grow,
        Self::Shrink,
        Self::ResetSize,
    ];

    /// Signed cells a grow or shrink moves the panel's edge: `step` cells `count` times
    /// (once without a count); zero for the other commands.
    #[must_use]
    pub fn delta(self, count: Option<u32>, step: i32) -> i32 {
        let step = step.saturating_mul(i32::try_from(count.unwrap_or(1)).unwrap_or(i32::MAX));
        match self {
            Self::Grow => step,
            Self::Shrink => step.saturating_neg(),
            Self::Maximize | Self::Minimize | Self::Close | Self::ResetSize => 0,
        }
    }
}

/// Why a window command did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRefusal {
    /// The panel is not shown.
    NotShown,
    /// The panel must stay expanded: no other panel of its group is expanded to take its
    /// room ([`PanelTreeGroup::keep_one_expanded`]).
    KeepExpanded,
    /// The panel cannot be closed.
    NotClosable,
    /// The panel cannot be resized now (minimized, maximized, or nothing to resize against).
    NotResizable,
}

/// A divider move that resizes a panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelResize {
    /// The group (index into the [`PanelTree`]).
    pub group: usize,
    /// The divider in that group.
    pub divider: usize,
    /// Cells to move it by (positive grows the panel before it), for
    /// [`crate::panel_group::PanelGroup::resize_divider`].
    pub delta: i32,
}

/// What a [`WindowCommand`] did, or asks of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCommandOutcome {
    /// The panel is now maximized (`true`) or restored.
    Maximized(bool),
    /// The panel is now minimized (`true`) or restored.
    Minimized(bool),
    /// Nothing changed.
    Refused(WindowRefusal),
    /// The application should close (hide) the panel. Its window state is already forgotten.
    CloseRequested,
    /// The application should move a divider of its group state.
    Resize(PanelResize),
    /// The application should forget the panel's resized size.
    ResetSizeRequested,
}

/// A member of a [`PanelTreeGroup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMember<K> {
    /// A panel.
    Panel(K),
    /// A nested group (index into the [`PanelTree`]).
    Group(usize),
}

/// One panel group of a [`PanelTree`]: its axis, its members in order, and its policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelTreeGroup<K> {
    axis: PanelGroupAxis,
    members: Vec<PanelMember<K>>,
    fill: Option<usize>,
    keep_one_expanded: bool,
}

impl<K> PanelTreeGroup<K> {
    /// A group laid out along `axis` holding `members`.
    #[must_use]
    pub const fn new(axis: PanelGroupAxis, members: Vec<PanelMember<K>>) -> Self {
        Self {
            axis,
            members,
            fill: None,
            keep_one_expanded: false,
        }
    }

    /// Member `index` takes the cells minimized members free (unless it is minimized too;
    /// otherwise the expanded members share them), and grow and shrink move other panels'
    /// edges toward it.
    #[must_use]
    pub const fn fill(mut self, index: usize) -> Self {
        self.fill = Some(index);
        self
    }

    /// Keep one member expanded: the fill member (the first without one) minimizes only while
    /// another member of the group stays expanded to take its room.
    #[must_use]
    pub const fn keep_one_expanded(mut self, keep: bool) -> Self {
        self.keep_one_expanded = keep;
        self
    }

    /// The axis.
    #[must_use]
    pub const fn axis(&self) -> PanelGroupAxis {
        self.axis
    }

    /// The members, in order.
    #[must_use]
    pub fn members(&self) -> &[PanelMember<K>] {
        &self.members
    }
}

/// Nested panel groups, the first being the root. A panel is shown when it is reachable from
/// the root; groups the root does not reach are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelTree<K> {
    groups: Vec<PanelTreeGroup<K>>,
}

impl<K> Default for PanelTree<K> {
    fn default() -> Self {
        Self { groups: Vec::new() }
    }
}

impl<K> PanelTree<K> {
    /// A tree of `groups`, the first being the root (no groups: nothing is shown).
    #[must_use]
    pub const fn new(groups: Vec<PanelTreeGroup<K>>) -> Self {
        Self { groups }
    }

    /// Group `index`.
    #[must_use]
    pub fn group(&self, index: usize) -> Option<&PanelTreeGroup<K>> {
        self.groups.get(index)
    }
}

/// How the panels of a tree are shown, resolved from the window state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved<K> {
    /// Panels reachable from the root, with their path: `(group, member)` from the root.
    paths: BTreeMap<K, Vec<(usize, usize)>>,
    /// Minimized panels that collapse, with their shape.
    shapes: BTreeMap<K, PanelShape>,
    /// Collapsed members of each group, as `(member, cells)`.
    collapses: Vec<Vec<(usize, u16)>>,
}

impl<K> Default for Resolved<K> {
    fn default() -> Self {
        Self {
            paths: BTreeMap::new(),
            shapes: BTreeMap::new(),
            collapses: Vec::new(),
        }
    }
}

/// Window state of the panels of a [`PanelTree`]: which are minimized and which is maximized.
///
/// * A minimized panel collapses along its group's axis, to one row stacked or to a strip side
///   by side; a nested group whose members are all minimized collapses in its parent instead,
///   its panels shown as the parent's shape. The cells a collapse frees go to the group's
///   fill member, else the expanded members share them.
/// * The maximized panel is the only one laid out: every group along its path lays out only
///   that branch ([`PanelGroupState::set_maximized`]). It is effective only while the panel is
///   shown, and maximizing a minimized panel restores it.
/// * Sizes stay with the application: [`Self::group_state`] builds a group's state from the
///   lengths the application chose, so restoring brings back the same sizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelWindows<K> {
    tree: PanelTree<K>,
    minimized: BTreeSet<K>,
    maximized: Option<K>,
    unclosable: BTreeSet<K>,
    strip: u16,
    resolved: Resolved<K>,
}

impl<K: Ord + Copy> Default for PanelWindows<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Copy> PanelWindows<K> {
    /// No panels, none minimized or maximized, every panel closable, and strips as wide as
    /// [`WindowControls::strip_width`] with the default glyphs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tree: PanelTree::default(),
            minimized: BTreeSet::new(),
            maximized: None,
            unclosable: BTreeSet::new(),
            strip: WindowControls::new(PanelShape::Strip).strip_width(),
            resolved: Resolved::default(),
        }
    }

    /// Start with `panels` minimized (for example, as saved).
    #[must_use]
    pub fn with_minimized(mut self, panels: impl IntoIterator<Item = K>) -> Self {
        self.minimized.extend(panels);
        self.resolve();
        self
    }

    /// Make `panel` closable or not (every panel is closable unless set otherwise).
    #[must_use]
    pub fn with_closable(mut self, panel: K, closable: bool) -> Self {
        if closable {
            self.unclosable.remove(&panel);
        } else {
            self.unclosable.insert(panel);
        }
        self
    }

    /// Whether `panel` can be closed.
    #[must_use]
    pub fn closable(&self, panel: K) -> bool {
        !self.unclosable.contains(&panel)
    }

    /// Lay out the panels as `tree`. Returns whether it changed.
    pub fn set_tree(&mut self, tree: PanelTree<K>) -> bool {
        if self.tree == tree {
            return false;
        }
        self.tree = tree;
        self.resolve();
        true
    }

    /// The tree the panels are laid out as.
    #[must_use]
    pub const fn tree(&self) -> &PanelTree<K> {
        &self.tree
    }

    /// Set the width of a minimized strip (see [`WindowControls::strip_width`]). Returns
    /// whether it changed.
    pub fn set_strip_width(&mut self, cells: u16) -> bool {
        let cells = cells.max(1);
        if self.strip == cells {
            return false;
        }
        self.strip = cells;
        self.resolve();
        true
    }

    /// The width of a minimized strip.
    #[must_use]
    pub const fn strip_width(&self) -> u16 {
        self.strip
    }

    /// Whether the user minimized `panel` (it may still show expanded, for example as the
    /// group's kept member).
    #[must_use]
    pub fn is_minimized(&self, panel: K) -> bool {
        self.minimized.contains(&panel)
    }

    /// The panels the user minimized.
    pub fn minimized_panels(&self) -> impl Iterator<Item = K> + '_ {
        self.minimized.iter().copied()
    }

    /// Mark `panel` minimized or not, without checks.
    pub fn set_minimized(&mut self, panel: K, minimized: bool) {
        let changed = if minimized {
            self.minimized.insert(panel)
        } else {
            self.minimized.remove(&panel)
        };
        if changed {
            self.resolve();
        }
    }

    /// Whether `panel` is shown (reachable from the tree's root).
    #[must_use]
    pub fn shown(&self, panel: K) -> bool {
        self.resolved.paths.contains_key(&panel)
    }

    /// The maximized panel, while it is shown.
    #[must_use]
    pub fn maximized(&self) -> Option<K> {
        self.maximized.filter(|panel| self.shown(*panel))
    }

    /// How `panel` is shown.
    #[must_use]
    pub fn shape(&self, panel: K) -> PanelShape {
        if self.maximized() == Some(panel) {
            return PanelShape::Maximized;
        }
        self.resolved
            .shapes
            .get(&panel)
            .copied()
            .unwrap_or_default()
    }

    /// The window controls of `panel` as it is shown (add glyphs and a style).
    #[must_use]
    pub fn controls(&self, panel: K) -> WindowControls {
        WindowControls::new(self.shape(panel)).closable(self.closable(panel))
    }

    /// Restore the maximized panel; returns whether one was maximized.
    pub const fn restore_maximized(&mut self) -> bool {
        self.maximized.take().is_some()
    }

    /// Maximize `panel` (restoring it when minimized), or restore it when it is maximized.
    /// Returns whether it is now maximized.
    ///
    /// # Errors
    ///
    /// * [`WindowRefusal::NotShown`] when the panel is not shown.
    pub fn toggle_maximize(&mut self, panel: K) -> Result<bool, WindowRefusal> {
        if self.maximized() == Some(panel) {
            self.maximized = None;
            return Ok(false);
        }
        if !self.shown(panel) {
            return Err(WindowRefusal::NotShown);
        }
        self.set_minimized(panel, false);
        self.maximized = Some(panel);
        Ok(true)
    }

    /// Minimize `panel`, or restore it when it is minimized; a maximized panel is restored
    /// first. Returns whether it is now minimized.
    ///
    /// # Errors
    ///
    /// * [`WindowRefusal::NotShown`] when the panel is not shown.
    /// * [`WindowRefusal::KeepExpanded`] when it must stay expanded to fill its group.
    pub fn toggle_minimize(&mut self, panel: K) -> Result<bool, WindowRefusal> {
        if !self.shown(panel) {
            return Err(WindowRefusal::NotShown);
        }
        if self.maximized == Some(panel) {
            self.maximized = None;
        }
        if self.shape(panel).minimized() {
            self.set_minimized(panel, false);
            return Ok(false);
        }
        self.set_minimized(panel, true);
        if self.shape(panel).minimized() {
            return Ok(true);
        }
        self.set_minimized(panel, false);
        Err(WindowRefusal::KeepExpanded)
    }

    /// Forget `panel`'s window state: it was opened (it shows at its size) or closed (it opens
    /// at its size next time).
    pub fn forget(&mut self, panel: K) {
        if self.maximized == Some(panel) {
            self.maximized = None;
        }
        self.set_minimized(panel, false);
    }

    /// Run `command` on `panel`. Grow and shrink move its edge by `delta` cells (see
    /// [`WindowCommand::delta`]) through [`Self::resize`].
    pub fn command(
        &mut self,
        panel: K,
        command: WindowCommand,
        delta: i32,
    ) -> WindowCommandOutcome {
        let outcome = match command {
            WindowCommand::Maximize => self
                .toggle_maximize(panel)
                .map(WindowCommandOutcome::Maximized),
            WindowCommand::Minimize => self
                .toggle_minimize(panel)
                .map(WindowCommandOutcome::Minimized),
            WindowCommand::Close if !self.closable(panel) => Err(WindowRefusal::NotClosable),
            WindowCommand::Close => {
                self.forget(panel);
                Ok(WindowCommandOutcome::CloseRequested)
            }
            WindowCommand::Grow | WindowCommand::Shrink => self
                .resize(panel, delta)
                .map(WindowCommandOutcome::Resize)
                .ok_or(WindowRefusal::NotResizable),
            WindowCommand::ResetSize => Ok(WindowCommandOutcome::ResetSizeRequested),
        };
        outcome.unwrap_or_else(WindowCommandOutcome::Refused)
    }

    /// The divider move that grows (positive `delta`) or shrinks `panel` by `delta` cells,
    /// moving its edge toward the tree's fill panel (followed from the root through each
    /// group's fill member): in the innermost group holding both, the divider between their
    /// branches next to the panel's. The fill panel itself moves its edge toward the member
    /// after it (before it when last) in its innermost group of several members. `None`
    /// while a panel is maximized, or when `panel` is not shown, is minimized, or has nothing
    /// to resize against.
    #[must_use]
    pub fn resize(&self, panel: K, delta: i32) -> Option<PanelResize> {
        if self.maximized().is_some() || self.shape(panel).minimized() {
            return None;
        }
        let path = self.resolved.paths.get(&panel)?;
        let fill = self.fill_path();
        let (group, index, fill_index) = if fill.last() == path.last() {
            let (group, index) = path.iter().rev().copied().find(|(group, _)| {
                self.tree
                    .group(*group)
                    .is_some_and(|group| group.members.len() > 1)
            })?;
            (group, index, index)
        } else {
            let (group, index) = path
                .iter()
                .copied()
                .rev()
                .find(|(group, _)| fill.iter().any(|(at, _)| at == group))?;
            let (_, fill_index) = fill.iter().copied().find(|(at, _)| *at == group)?;
            (group, index, fill_index)
        };
        let count = self.tree.group(group)?.members.len();
        // A panel after the fill moves the divider before it; one before the fill (and the fill
        // itself, toward a member after it) the divider after it.
        let (divider, sign) = match index.cmp(&fill_index) {
            std::cmp::Ordering::Equal if index + 1 < count => (index, 1),
            std::cmp::Ordering::Equal if index > 0 => (index - 1, -1),
            std::cmp::Ordering::Equal => return None,
            std::cmp::Ordering::Greater => (index - 1, -1),
            std::cmp::Ordering::Less => (index, 1),
        };
        Some(PanelResize {
            group,
            divider,
            delta: delta.saturating_mul(sign),
        })
    }

    /// Collapsed members of group `group`, as `(member, cells)`.
    #[must_use]
    pub fn collapses(&self, group: usize) -> &[(usize, u16)] {
        self.resolved
            .collapses
            .get(group)
            .map_or(&[], Vec::as_slice)
    }

    /// The member of group `group` laid out alone while a panel is maximized.
    #[must_use]
    pub fn maximized_member(&self, group: usize) -> Option<usize> {
        let panel = self.maximized()?;
        self.resolved
            .paths
            .get(&panel)?
            .iter()
            .find(|(at, _)| *at == group)
            .map(|(_, member)| *member)
    }

    /// The state of group `group` with its members at `lengths` (flex weights equal to their
    /// lengths, so lengths summing to the group's size lay out exactly) within `constraints`:
    /// its collapsed members collapsed, and its maximized member laid out alone. The cells a
    /// collapse frees go to the group's fill member unless it is collapsed too, else the
    /// expanded members share them; either way the expanded members lose their upper limits,
    /// so they fill the group.
    #[must_use]
    pub fn group_state(
        &self,
        group: usize,
        mut lengths: Vec<u16>,
        mut constraints: Vec<PanelGroupConstraints>,
    ) -> PanelGroupState {
        let collapsed = self.collapses(group);
        let is_collapsed = |index: usize| collapsed.iter().any(|(at, _)| *at == index);
        if !collapsed.is_empty() {
            let mut freed: u16 = 0;
            for (index, cells) in collapsed {
                if let Some(length) = lengths.get(*index) {
                    freed = freed.saturating_add(length.saturating_sub(*cells));
                }
            }
            for (index, constraint) in constraints.iter_mut().enumerate() {
                if !is_collapsed(index) {
                    constraint.max = None;
                }
            }
            let fill = self.tree.group(group).and_then(|group| group.fill);
            if let Some(length) = fill
                .filter(|index| !is_collapsed(*index))
                .and_then(|index| lengths.get_mut(index))
            {
                *length = length.saturating_add(freed);
            }
        }
        let mut state = PanelGroupState::new(
            lengths
                .iter()
                .map(|length| PanelSize::flex((*length).max(1)))
                .collect::<Vec<_>>(),
        );
        state.set_constraints(constraints);
        for (index, cells) in collapsed {
            state.collapse(*index, *cells);
        }
        state.set_maximized(self.maximized_member(group));
        state
    }

    /// The path of the tree's fill panel: from the root, each group's fill member.
    fn fill_path(&self) -> Vec<(usize, usize)> {
        let mut path = Vec::new();
        let mut group = 0;
        while let Some(tree_group) = self.tree.group(group) {
            if path.len() > self.tree.groups.len() {
                break;
            }
            let index = tree_group.fill.unwrap_or(0);
            match tree_group.members.get(index) {
                Some(PanelMember::Panel(_)) => {
                    path.push((group, index));
                    break;
                }
                Some(PanelMember::Group(next)) => {
                    path.push((group, index));
                    group = *next;
                }
                None => break,
            }
        }
        path
    }

    /// Whether every member of `group` collapses by its own rules.
    fn all_collapse(&self, group: usize, depth: usize) -> bool {
        let flags = self.member_collapses(group, depth);
        !flags.is_empty() && flags.iter().all(|flag| *flag)
    }

    /// Which members of `group` collapse by its own rules (before its parent's).
    fn member_collapses(&self, group: usize, depth: usize) -> Vec<bool> {
        let Some(tree_group) = self.tree.group(group) else {
            return Vec::new();
        };
        if depth > self.tree.groups.len() {
            return vec![false; tree_group.members.len()];
        }
        let kept = tree_group
            .keep_one_expanded
            .then(|| tree_group.fill.unwrap_or(0));
        let mut flags: Vec<bool> = tree_group
            .members
            .iter()
            .enumerate()
            .map(|(index, member)| match member {
                PanelMember::Panel(panel) => Some(index) != kept && self.is_minimized(*panel),
                PanelMember::Group(nested) => self.all_collapse(*nested, depth + 1),
            })
            .collect();
        if let Some(kept) = kept
            && let Some(PanelMember::Panel(panel)) = tree_group.members.get(kept)
            && self.is_minimized(*panel)
            && flags
                .iter()
                .enumerate()
                .any(|(index, collapsed)| index != kept && !collapsed)
        {
            flags[kept] = true;
        }
        flags
    }

    /// Resolve how every panel is shown from the window state and the tree.
    fn resolve(&mut self) {
        let mut resolved = Resolved {
            collapses: vec![Vec::new(); self.tree.groups.len()],
            ..Resolved::default()
        };
        if !self.tree.groups.is_empty() {
            self.visit(0, None, &mut Vec::new(), &mut resolved);
        }
        self.resolved = resolved;
    }

    /// Resolve group `group` reached through `path`; `inherited` is the shape of its panels
    /// when the group itself collapsed in its parent.
    fn visit(
        &self,
        group: usize,
        inherited: Option<PanelShape>,
        path: &mut Vec<(usize, usize)>,
        resolved: &mut Resolved<K>,
    ) {
        let Some(tree_group) = self.tree.group(group) else {
            return;
        };
        if path.len() > self.tree.groups.len() || path.iter().any(|(at, _)| *at == group) {
            return;
        }
        let flags = if inherited.is_some() {
            vec![false; tree_group.members.len()]
        } else {
            self.member_collapses(group, 0)
        };
        let shape = PanelShape::minimized_along(tree_group.axis);
        let cells = match tree_group.axis {
            PanelGroupAxis::Horizontal => self.strip,
            PanelGroupAxis::Vertical => 1,
        };
        for (index, member) in tree_group.members.iter().enumerate() {
            let collapsed = flags.get(index).copied().unwrap_or(false);
            if collapsed {
                resolved.collapses[group].push((index, cells));
            }
            let member_shape = inherited.or_else(|| collapsed.then_some(shape));
            path.push((group, index));
            match member {
                PanelMember::Panel(panel) => {
                    resolved.paths.insert(*panel, path.clone());
                    if let Some(shape) = member_shape {
                        resolved.shapes.insert(*panel, shape);
                    }
                }
                PanelMember::Group(nested) => self.visit(*nested, member_shape, path, resolved),
            }
            path.pop();
        }
    }
}

#[cfg(test)]
mod tests;
