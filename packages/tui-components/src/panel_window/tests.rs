use std::cell::{Cell, RefCell};
use std::time::Duration;

use bmux_tui::buffer::Buffer;
use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx, LayoutNode};
use bmux_tui::composition::Column;
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::PaintCx;

use super::{
    DOUBLE_CLICK, Instant, PanelMember, PanelResize, PanelShape, PanelTree, PanelTreeGroup,
    PanelWindows, TitleClicks, TitleTarget, WindowCommand, WindowCommandOutcome, WindowControl,
    WindowControls, WindowGlyphs, WindowRefusal, minimized_content,
};
use crate::pane::{Pane, PaneComponent, PaneState, PaneTitleAction};
use crate::panel_group::{
    PanelDividerMode, PanelGroup, PanelGroupAxis, PanelGroupComponent, PanelGroupConstraints,
    PanelGroupOutcome, PanelGroupPolicy, PanelGroupState, PanelSize,
};

/// A change to the window state.
type Setup = fn(&mut PanelWindows<P>);

/// The panels of the test tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum P {
    /// Left of the column.
    Left,
    /// The document, the tree's fill panel; it does not close.
    Doc,
    /// Under the document.
    Under,
    /// Top of the right column.
    Top,
    /// Bottom of the right column.
    Bottom,
}

const ALL: [P; 5] = [P::Left, P::Doc, P::Under, P::Top, P::Bottom];
const ROOT: usize = 0;
const COLUMN: usize = 1;
const SIDE: usize = 2;

impl P {
    const fn id(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Doc => "doc",
            Self::Under => "under",
            Self::Top => "top",
            Self::Bottom => "bottom",
        }
    }

    /// A title with wide and combining graphemes for some panels.
    const fn title(self) -> &'static str {
        match self {
            Self::Left => "Files",
            Self::Doc => "文書 review 📄",
            Self::Under => "Threads",
            Self::Top => "Gui\u{0301}de",
            Self::Bottom => "スレッド",
        }
    }
}

/// `[Left | [Doc / Under] | [Top / Bottom]]`, the document filling and kept expanded.
fn tree() -> PanelTree<P> {
    PanelTree::new(vec![
        PanelTreeGroup::new(
            PanelGroupAxis::Horizontal,
            vec![
                PanelMember::Panel(P::Left),
                PanelMember::Group(COLUMN),
                PanelMember::Group(SIDE),
            ],
        )
        .fill(1)
        .keep_one_expanded(true),
        PanelTreeGroup::new(
            PanelGroupAxis::Vertical,
            vec![PanelMember::Panel(P::Doc), PanelMember::Panel(P::Under)],
        )
        .fill(0)
        .keep_one_expanded(true),
        PanelTreeGroup::new(
            PanelGroupAxis::Vertical,
            vec![PanelMember::Panel(P::Top), PanelMember::Panel(P::Bottom)],
        ),
    ])
}

fn windows() -> PanelWindows<P> {
    let mut windows = PanelWindows::new().with_closable(P::Doc, false);
    windows.set_tree(tree());
    windows
}

const BORDERS_H: PanelDividerMode = PanelDividerMode::Overlay {
    before: 0,
    after: 1,
};
const BORDERS_V: PanelDividerMode = PanelDividerMode::Overlay {
    before: 1,
    after: 0,
};

fn group(axis: PanelGroupAxis) -> PanelGroup {
    PanelGroup::new(axis)
        .policy(PanelGroupPolicy::resize_only())
        .divider_mode(match axis {
            PanelGroupAxis::Horizontal => BORDERS_H,
            PanelGroupAxis::Vertical => BORDERS_V,
        })
}

/// Lengths filling `total` cells: a quarter for each side member, the rest for the fill.
fn lengths(total: u16, members: usize, fill: usize) -> Vec<u16> {
    let side = total / 4;
    let mut lengths = vec![side; members];
    lengths[fill] = total.saturating_sub(side * u16::try_from(members - 1).unwrap_or(0));
    lengths
}

/// An application holding the test tree: window state, group states and pane states.
struct App {
    windows: PanelWindows<P>,
    /// Show controls only on the focused and hovered panels, when set.
    view: Option<View>,
    groups: [RefCell<PanelGroupState>; 3],
    panes: [Cell<PaneState>; 5],
    clicks: TitleClicks<P>,
    size: (u16, u16),
}

/// The focused panel and what the pointer is over.
#[derive(Debug, Clone, Copy, Default)]
struct View {
    focused: Option<P>,
    hovered: Option<P>,
    control: Option<WindowControl>,
}

/// One painted frame.
struct Painted {
    buffer: Buffer,
    hits: Vec<(String, Rect)>,
    layout: LayoutNode,
}

impl Painted {
    fn hit(&self, id: &str) -> Option<Rect> {
        self.hits
            .iter()
            .find(|(hit, _)| hit == id)
            .map(|(_, area)| *area)
    }

    fn text_in(&self, area: Rect) -> String {
        (area.x..area.right())
            .filter_map(|x| self.buffer.get(Point::new(x, area.y)))
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    /// The absolute area of layout node `id`.
    fn area(&self, id: &str) -> Option<Rect> {
        fn find(node: &LayoutNode, x: u64, y: u64, id: &str) -> Option<Rect> {
            if node.id.as_str() == id {
                return Some(Rect::new(
                    u16::try_from(x).ok()?,
                    u16::try_from(y).ok()?,
                    u16::try_from(node.size.width).ok()?,
                    u16::try_from(node.size.height).ok()?,
                ));
            }
            node.children
                .iter()
                .find_map(|child| find(&child.node, x + child.x, y + child.y, id))
        }
        find(&self.layout, 0, 0, id)
    }
}

impl App {
    fn new(width: u16, height: u16) -> Self {
        let app = Self {
            windows: windows(),
            view: None,
            groups: std::array::from_fn(|_| RefCell::new(PanelGroupState::new(Vec::new()))),
            panes: std::array::from_fn(|_| Cell::new(PaneState::new(Rect::default()))),
            clicks: TitleClicks::default(),
            size: (width, height),
        };
        app.sync();
        app
    }

    /// Rebuild the group states from the application's lengths and the window state.
    fn sync(&self) {
        let (width, height) = self.size;
        let constraints = |count: usize| vec![PanelGroupConstraints::new(3, None); count];
        *self.groups[ROOT].borrow_mut() =
            self.windows
                .group_state(ROOT, lengths(width, 3, 1), constraints(3));
        *self.groups[COLUMN].borrow_mut() =
            self.windows
                .group_state(COLUMN, lengths(height, 2, 0), constraints(2));
        *self.groups[SIDE].borrow_mut() =
            self.windows
                .group_state(SIDE, lengths(height, 2, 0), constraints(2));
    }

    fn chrome(&self, panel: P) -> Pane<'static> {
        self.controls(panel).apply(
            Pane::new()
                .title(panel.title().to_owned())
                .action(PaneTitleAction::new("own", "+ own")),
        )
    }

    /// The controls of `panel` as painted: shown on the focused and hovered panels while the
    /// view is set.
    fn controls(&self, panel: P) -> WindowControls {
        let controls = self.windows.controls(panel);
        let Some(view) = self.view else {
            return controls;
        };
        let hovered = view.hovered == Some(panel);
        controls
            .shown(view.focused == Some(panel) || hovered)
            .hovered(view.control.filter(|_| hovered))
    }

    /// The pointer moved to `position`: the panel under it shows its controls, and the
    /// control under it (hit tested on that panel's title row with its controls shown) is
    /// hovered.
    fn hover(&mut self, painted: &Painted, position: Point) {
        let mut view = self.view.unwrap_or_default();
        view.hovered = ALL.into_iter().find(|panel| {
            painted
                .area(panel.id())
                .is_some_and(|area| area.contains(position))
        });
        view.control = view.hovered.and_then(|panel| {
            let controls = self.windows.controls(panel);
            let area = painted.area(panel.id())?;
            let chrome = controls.apply(
                Pane::new()
                    .title(panel.title().to_owned())
                    .action(PaneTitleAction::new("own", "+ own")),
            );
            match controls.target(&chrome, area, position)? {
                TitleTarget::Control(control) => Some(control),
                TitleTarget::Action(_) | TitleTarget::Title => None,
            }
        });
        self.view = Some(view);
    }

    fn pane(&self, panel: P) -> PaneComponent<'static, '_> {
        let shape = self.windows.shape(panel);
        let content = minimized_content(
            format!("{}.strip", panel.id()),
            shape,
            panel.title(),
            bmux_tui::style::Style::new(),
        )
        .unwrap_or_else(Column::new);
        let state = &self.panes[ALL.iter().position(|at| *at == panel).unwrap_or(0)];
        PaneComponent::new(panel.id(), self.chrome(panel), state, content)
    }

    fn root(&self) -> PanelGroupComponent<'_> {
        PanelGroupComponent::new(
            "root",
            group(PanelGroupAxis::Horizontal),
            &self.groups[ROOT],
        )
        .child(self.pane(P::Left))
        .child(
            PanelGroupComponent::new(
                "column",
                group(PanelGroupAxis::Vertical),
                &self.groups[COLUMN],
            )
            .child(self.pane(P::Doc))
            .child(self.pane(P::Under)),
        )
        .child(
            PanelGroupComponent::new("side", group(PanelGroupAxis::Vertical), &self.groups[SIDE])
                .child(self.pane(P::Top))
                .child(self.pane(P::Bottom)),
        )
    }

    fn paint(&self) -> Painted {
        let (width, height) = self.size;
        let root = self.root();
        let layout = root.layout(
            Constraints::new(
                width.into(),
                width.into(),
                height.into(),
                Some(height.into()),
            ),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = Frame::new(&mut buffer);
        root.paint(&layout, &mut PaintCx::new(&mut frame));
        let hits = frame
            .hits()
            .regions()
            .iter()
            .map(|region| (region.id.as_str().to_owned(), region.area))
            .collect();
        drop(frame);
        Painted {
            buffer,
            hits,
            layout,
        }
    }

    /// A press at `position` as the application routes it: a control runs its command, and
    /// two presses on a title row toggle maximize. Returns what the press was on.
    fn press(&mut self, painted: &Painted, position: Point) -> Option<(P, TitleTarget)> {
        let (panel, target) = ALL.into_iter().find_map(|panel| {
            let area = painted.area(panel.id())?;
            self.controls(panel)
                .target(&self.chrome(panel), area, position)
                .map(|target| (panel, target))
        })?;
        match target {
            TitleTarget::Control(control) => {
                self.clicks.reset();
                self.command(panel, control.command());
            }
            TitleTarget::Title | TitleTarget::Action(_) => {
                if self.clicks.press(panel) {
                    self.command(panel, WindowCommand::Maximize);
                }
            }
        }
        Some((panel, target))
    }

    fn command(&mut self, panel: P, command: WindowCommand) -> WindowCommandOutcome {
        let outcome = self.windows.command(panel, command, command.delta(None, 2));
        if let WindowCommandOutcome::Resize(resize) = outcome {
            let (width, height) = self.size;
            let axis = if resize.group == ROOT {
                PanelGroupAxis::Horizontal
            } else {
                PanelGroupAxis::Vertical
            };
            group(axis).resize_divider(
                Rect::new(0, 0, width, height),
                &mut self.groups[resize.group].borrow_mut(),
                resize.divider,
                resize.delta,
            );
            return outcome;
        }
        self.sync();
        outcome
    }

    /// Every window control painted shows the glyph of its panel's state on its own cells,
    /// inside the frame, and only shown, laid-out panels have regions.
    fn assert_controls_agree(&self, painted: &Painted, context: &str) {
        let frame = Rect::new(0, 0, self.size.0, self.size.1);
        for (id, area) in &painted.hits {
            assert!(
                area.width > 0 && area.height > 0,
                "{context}: {id} {area:?}"
            );
            assert_eq!(frame.intersection(*area), *area, "{context}: {id} {area:?}");
            for panel in ALL {
                let Some(control) = id
                    .strip_prefix(&format!("{}.action.", panel.id()))
                    .and_then(WindowControl::from_id)
                else {
                    continue;
                };
                let maximized = self.windows.maximized();
                assert!(
                    maximized.is_none_or(|maximized| maximized == panel),
                    "{context}: {id} while {maximized:?} is maximized"
                );
                let glyph = self.controls(panel).glyph(control);
                assert_eq!(
                    painted.text_in(*area).trim(),
                    glyph,
                    "{context}: {id} {area:?}"
                );
                // The control lies on its pane's title row, as hit testing reads it.
                let pane = painted.area(panel.id()).expect("laid out");
                assert_eq!(
                    self.controls(panel).target(
                        &self.chrome(panel),
                        pane,
                        Point::new(area.x, area.y)
                    ),
                    Some(TitleTarget::Control(control)),
                    "{context}: {id}"
                );
            }
        }
    }
}

#[test]
fn controls_list_glyphs_and_strips() {
    let normal = WindowControls::new(PanelShape::Normal);
    assert_eq!(
        normal.list(),
        [
            WindowControl::Minimize,
            WindowControl::Maximize,
            WindowControl::Close
        ]
    );
    assert_eq!(
        normal.closable(false).list(),
        [WindowControl::Minimize, WindowControl::Maximize]
    );
    // A minimized panel offers only its restore control, as a row or a strip.
    for shape in [PanelShape::Row, PanelShape::Strip] {
        assert_eq!(WindowControls::new(shape).list(), [WindowControl::Minimize]);
    }
    let glyphs = WindowGlyphs::UNICODE;
    assert_eq!(normal.glyph(WindowControl::Minimize), glyphs.minimize);
    assert_eq!(normal.glyph(WindowControl::Maximize), glyphs.maximize);
    for shape in [PanelShape::Row, PanelShape::Strip] {
        let controls = WindowControls::new(shape);
        assert_eq!(
            controls.glyph(WindowControl::Minimize),
            glyphs.restore_minimized
        );
    }
    let maximized = WindowControls::new(PanelShape::Maximized);
    assert_eq!(
        maximized.glyph(WindowControl::Maximize),
        glyphs.restore_maximized
    );
    let ascii = WindowControls::new(PanelShape::Maximized).glyphs(WindowGlyphs::ASCII);
    assert_eq!(ascii.glyph(WindowControl::Maximize), "=");
    assert_eq!(ascii.glyph(WindowControl::Minimize), "v");
    assert_eq!(ascii.glyph(WindowControl::Close), "x");
    assert_eq!(
        WindowControls::new(PanelShape::Row)
            .glyphs(WindowGlyphs::ASCII)
            .glyph(WindowControl::Minimize),
        ">"
    );
    // A strip holds its padded restore control between the corners.
    assert_eq!(WindowControls::new(PanelShape::Strip).strip_width(), 5);
    let wide = WindowGlyphs {
        restore_minimized: "⧉⧉",
        ..WindowGlyphs::UNICODE
    };
    assert_eq!(
        WindowControls::new(PanelShape::Strip)
            .glyphs(wide)
            .strip_width(),
        6
    );
    for control in WindowControl::ALL {
        assert_eq!(WindowControl::from_id(control.id()), Some(control));
    }
    assert_eq!(WindowControl::from_id("own"), None);
}

#[test]
fn narrow_title_rows_drop_own_actions_then_minimize_then_maximize_and_keep_the_title() {
    let pane = |width: u16| {
        let pane = WindowControls::new(PanelShape::Normal)
            .min_title_width(4)
            .apply(
                Pane::new()
                    .title("文書タイトル")
                    .action(PaneTitleAction::new("own", "+ own")),
            );
        let ids: Vec<String> = pane
            .title_layout(Rect::new(0, 0, width, 3))
            .actions
            .iter()
            .map(|(index, _)| pane.actions()[*index].id.clone())
            .collect();
        let title = pane.title_layout(Rect::new(0, 0, width, 3)).title;
        (ids, title)
    };
    // Corners (2) and the title kept at 4 cells with a gap (5) leave the rest to the actions:
    // the cluster is its caps (4), its glyphs with a space between, and a cell before the
    // corner; the own action takes 5 cells and a gap.
    assert_eq!(
        pane(23).0,
        ["own", "minimize", "maximize", "close"],
        "everything fits"
    );
    assert_eq!(pane(22).0, ["minimize", "maximize", "close"]);
    assert_eq!(pane(17).0, ["minimize", "maximize", "close"]);
    assert_eq!(pane(16).0, ["maximize", "close"]);
    assert_eq!(pane(15).0, ["maximize", "close"]);
    assert_eq!(pane(14).0, ["close"]);
    assert_eq!(pane(13).0, ["close"]);
    assert_eq!(pane(12).0, Vec::<String>::new());
    for width in 0..40 {
        let (_, title) = pane(width);
        if width >= 7 {
            // The title always keeps its minimum (two wide graphemes are four cells).
            assert!(title.is_some_and(|title| title.width >= 4), "{width}");
        }
    }
    // A strip keeps only its padded restore control, without a title minimum.
    let strip = WindowControls::new(PanelShape::Strip).apply(Pane::new().title("Files"));
    let layout = strip.title_layout(Rect::new(0, 0, 5, 10));
    assert_eq!(layout.actions, [(0, Rect::new(1, 0, 3, 1))]);
    assert_eq!(layout.groups, []);
}

#[test]
fn title_targets_agree_with_the_painted_regions_at_every_width() {
    for width in 0..40u16 {
        let controls = WindowControls::new(PanelShape::Normal).min_title_width(4);
        let chrome = controls.apply(
            Pane::new()
                .title("スレッド 🧵")
                .action(PaneTitleAction::new("own", "+ own")),
        );
        let state = Cell::new(PaneState::new(Rect::new(0, 0, width, 3)));
        let component = PaneComponent::new("pane", chrome.clone(), &state, Column::new());
        let layout = component.layout(
            Constraints::new(width.into(), width.into(), 3, Some(3)),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), 3));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let hits: Vec<(String, Rect)> = frame
            .hits()
            .regions()
            .iter()
            .map(|region| (region.id.as_str().to_owned(), region.area))
            .collect();
        drop(frame);
        let area = Rect::new(0, 0, width, 3);
        for x in 0..width {
            let target = controls.target(&chrome, area, Point::new(x, 0));
            let painted = hits
                .iter()
                .find(|(_, rect)| rect.contains(Point::new(x, 0)))
                .map(|(id, _)| id.as_str());
            let expected = match painted {
                Some("pane.action.own") => Some(TitleTarget::Action(0)),
                Some(id) => id
                    .strip_prefix("pane.action.")
                    .and_then(WindowControl::from_id)
                    .map(TitleTarget::Control),
                None => None,
            };
            if x == 0 || x + 1 >= width {
                assert_eq!(target, None, "corner at {width}: {x}");
            } else {
                assert_eq!(
                    target,
                    Some(expected.unwrap_or(TitleTarget::Title)),
                    "{width}: {x}"
                );
            }
        }
        // Off the title row nothing is targeted.
        assert_eq!(controls.target(&chrome, area, Point::new(2, 1)), None);
    }
}

#[test]
fn strip_content_writes_the_name_down_the_strip_by_grapheme() {
    let name = "a中😀e\u{0301}";
    let content = minimized_content(
        "strip",
        PanelShape::Strip,
        name,
        bmux_tui::style::Style::new(),
    )
    .expect("strip content");
    let layout = content.layout(Constraints::new(3, 3, 0, Some(10)), &mut LayoutCx::new());
    let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 10));
    let mut frame = Frame::new(&mut buffer);
    content.paint(&layout, &mut PaintCx::new(&mut frame));
    drop(frame);
    let rows: Vec<String> = (0..4)
        .map(|y| buffer.row_symbols(y).unwrap_or_default())
        .collect();
    assert_eq!(rows[0].trim(), "a");
    assert!(rows[1].contains('中'), "{rows:?}");
    assert!(rows[2].contains('😀'), "{rows:?}");
    assert!(rows[3].contains("e\u{0301}"), "{rows:?}");
    // Clipped to a narrow strip: the wide grapheme does not spill.
    let layout = content.layout(Constraints::new(2, 2, 0, Some(10)), &mut LayoutCx::new());
    assert!(layout.size.width <= 2);
    assert!(
        minimized_content("row", PanelShape::Row, name, bmux_tui::style::Style::new()).is_some_and(
            |row| row
                .layout(Constraints::new(3, 3, 0, Some(10)), &mut LayoutCx::new())
                .children
                .is_empty()
        )
    );
    assert!(
        minimized_content("x", PanelShape::Normal, name, bmux_tui::style::Style::new()).is_none()
    );
    assert!(
        minimized_content(
            "x",
            PanelShape::Maximized,
            name,
            bmux_tui::style::Style::new()
        )
        .is_none()
    );
}

#[test]
fn a_double_click_needs_two_quick_presses_on_the_same_title() {
    let mut clicks = TitleClicks::default();
    let start = Instant::now();
    assert!(!clicks.press_at(P::Left, start));
    assert!(!clicks.press_at(P::Doc, start), "another panel starts over");
    assert!(clicks.press_at(P::Doc, start + Duration::from_millis(100)));
    assert!(
        !clicks.press_at(P::Doc, start + Duration::from_millis(150)),
        "a third press starts over"
    );
    assert!(!clicks.press_at(P::Doc, start + DOUBLE_CLICK + Duration::from_millis(200)));
    clicks.reset();
    assert!(!clicks.press_at(P::Doc, start + DOUBLE_CLICK + Duration::from_millis(250)));
}

#[test]
fn commands_resize_by_steps() {
    assert_eq!(WindowCommand::Grow.delta(Some(3), 4), 12);
    assert_eq!(WindowCommand::Shrink.delta(None, 4), -4);
    for command in [
        WindowCommand::Maximize,
        WindowCommand::Minimize,
        WindowCommand::Close,
        WindowCommand::ResetSize,
    ] {
        assert_eq!(command.delta(Some(2), 4), 0);
    }
}

#[test]
fn minimized_panels_collapse_along_their_group_and_free_cells_to_the_fill() {
    let mut app = App::new(80, 24);
    assert_eq!(app.windows.toggle_minimize(P::Left), Ok(true));
    assert_eq!(app.windows.shape(P::Left), PanelShape::Strip);
    assert_eq!(app.windows.toggle_minimize(P::Under), Ok(true));
    assert_eq!(app.windows.shape(P::Under), PanelShape::Row);
    assert_eq!(app.windows.toggle_minimize(P::Top), Ok(true));
    assert_eq!(app.windows.shape(P::Top), PanelShape::Row);
    app.sync();
    let painted = app.paint();
    assert_eq!(painted.area("left").map(|area| area.width), Some(5));
    assert_eq!(painted.area("under").map(|area| area.height), Some(1));
    assert_eq!(painted.area("top").map(|area| area.height), Some(1));
    // The fill takes what the strip freed; the right column keeps its width.
    assert_eq!(
        painted.area("doc").map(|area| area.width),
        Some(80 - 5 - 20)
    );
    assert_eq!(painted.area("doc").map(|area| area.height), Some(23));
    assert_eq!(painted.area("bottom").map(|area| area.height), Some(23));
    app.assert_controls_agree(&painted, "minimized");
    // Restoring brings back the same sizes.
    for panel in [P::Left, P::Under, P::Top] {
        assert_eq!(app.windows.toggle_minimize(panel), Ok(false));
    }
    app.sync();
    let painted = app.paint();
    assert_eq!(painted.area("left").map(|area| area.width), Some(20));
    assert_eq!(painted.area("under").map(|area| area.height), Some(6));
    assert_eq!(painted.area("top").map(|area| area.height), Some(18));
}

#[test]
fn a_nested_group_with_every_panel_minimized_becomes_a_strip_in_its_parent() {
    let mut app = App::new(80, 24);
    assert_eq!(app.windows.toggle_minimize(P::Top), Ok(true));
    assert_eq!(app.windows.toggle_minimize(P::Bottom), Ok(true));
    assert_eq!(app.windows.shape(P::Top), PanelShape::Strip);
    assert_eq!(app.windows.shape(P::Bottom), PanelShape::Strip);
    assert_eq!(app.windows.collapses(ROOT), [(2, 5)]);
    assert_eq!(app.windows.collapses(SIDE), []);
    app.sync();
    let painted = app.paint();
    assert_eq!(painted.area("side").map(|area| area.width), Some(5));
    assert_eq!(painted.area("top").map(|area| area.width), Some(5));
    // Each strip shows only its restore control and its name.
    assert!(painted.hit("top.action.maximize").is_none());
    assert!(painted.hit("top.action.minimize").is_some());
    app.assert_controls_agree(&painted, "strips");
    // Restoring one leaves the other minimized to its row.
    assert_eq!(app.windows.toggle_minimize(P::Top), Ok(false));
    assert_eq!(app.windows.shape(P::Bottom), PanelShape::Row);
    assert_eq!(app.windows.shape(P::Top), PanelShape::Normal);
}

#[test]
fn the_kept_panel_minimizes_only_while_another_member_stays_expanded() {
    let mut windows = windows();
    assert_eq!(windows.toggle_minimize(P::Doc), Ok(true));
    assert_eq!(windows.shape(P::Doc), PanelShape::Row);
    assert_eq!(windows.toggle_minimize(P::Doc), Ok(false));
    assert_eq!(windows.toggle_minimize(P::Under), Ok(true));
    assert_eq!(
        windows.toggle_minimize(P::Doc),
        Err(WindowRefusal::KeepExpanded)
    );
    assert!(!windows.is_minimized(P::Doc));
    assert_eq!(windows.shape(P::Doc), PanelShape::Normal);
    // Minimized first, it expands again while nothing else is expanded, and stays marked.
    let mut windows = self::windows();
    windows.set_minimized(P::Doc, true);
    windows.set_minimized(P::Under, true);
    assert_eq!(windows.shape(P::Doc), PanelShape::Normal);
    assert!(windows.is_minimized(P::Doc));
    windows.set_minimized(P::Under, false);
    assert_eq!(windows.shape(P::Doc), PanelShape::Row);
    // Freed rows go to the expanded member when the fill is collapsed.
    let state = windows.group_state(
        COLUMN,
        vec![18, 6],
        vec![PanelGroupConstraints::new(3, Some(10)); 2],
    );
    let layout = group(PanelGroupAxis::Vertical).layout(Rect::new(0, 0, 10, 24), &state);
    assert_eq!(
        layout
            .panels
            .iter()
            .map(|panel| panel.height)
            .collect::<Vec<_>>(),
        [1, 23]
    );
}

#[test]
fn a_nested_maximize_lays_out_only_its_branch_and_restores_the_same_sizes() {
    let mut app = App::new(80, 24);
    let before = app.paint();
    for panel in ALL {
        assert_eq!(
            app.command(panel, WindowCommand::Maximize),
            WindowCommandOutcome::Maximized(true)
        );
        let painted = app.paint();
        assert_eq!(app.windows.shape(panel), PanelShape::Maximized);
        assert_eq!(
            painted.area(panel.id()),
            Some(Rect::new(0, 0, 80, 24)),
            "{panel:?}"
        );
        for other in ALL.into_iter().filter(|other| *other != panel) {
            assert_eq!(
                painted.area(other.id()),
                None,
                "{panel:?}: {other:?} laid out"
            );
            let prefix = format!("{}.", other.id());
            assert!(
                !painted.hits.iter().any(|(id, _)| id.starts_with(&prefix)),
                "{panel:?}: {other:?} left regions"
            );
        }
        assert!(
            !painted.hits.iter().any(|(id, _)| id.contains(".divider.")),
            "{panel:?}: a divider shows"
        );
        app.assert_controls_agree(&painted, &format!("{panel:?} maximized"));
        assert_eq!(
            app.command(panel, WindowCommand::Maximize),
            WindowCommandOutcome::Maximized(false)
        );
        let painted = app.paint();
        for other in ALL {
            assert_eq!(
                painted.area(other.id()),
                before.area(other.id()),
                "{other:?}"
            );
        }
    }
    // Maximizing a minimized panel restores it; a restore key backs out.
    app.windows.set_minimized(P::Under, true);
    assert_eq!(app.windows.toggle_maximize(P::Under), Ok(true));
    assert!(!app.windows.is_minimized(P::Under));
    assert!(app.windows.restore_maximized());
    assert!(!app.windows.restore_maximized());
}

#[test]
fn a_maximized_group_routes_events_only_to_its_laid_out_child() {
    let state = RefCell::new(PanelGroupState::new([
        PanelSize::flex(1),
        PanelSize::flex(1),
    ]));
    state.borrow_mut().set_maximized(Some(1));
    let seen = [Cell::new(false), Cell::new(false)];
    let component = PanelGroupComponent::new(
        "group",
        PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::resize_only()),
        &state,
    )
    .child(Probe(&seen[0]))
    .child(Probe(&seen[1]));
    let layout = component.layout(Constraints::new(10, 10, 4, Some(4)), &mut LayoutCx::new());
    assert_eq!(layout.children.len(), 1);
    assert_eq!(layout.children[0].node.size.width, 10);
    let mut cx = EventCx::new(&layout);
    // The state changing after layout does not misroute the frame's events.
    state.borrow_mut().set_maximized(None);
    component.event(
        &Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(1, 1),
        )),
        &layout,
        &mut cx,
    );
    assert!(!seen[0].get());
    assert!(seen[1].get());
    // Maximized, no divider resizes, and the layout API agrees.
    let mut state = PanelGroupState::new([PanelSize::flex(1), PanelSize::flex(1)]);
    assert!(state.set_maximized(Some(0)));
    assert!(!state.set_maximized(Some(0)));
    assert!(!state.set_maximized(Some(5)) || state.maximized().is_none());
    state.set_maximized(Some(0));
    assert!(!state.divider_resizable(0));
    let group = PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::resize_only());
    let area = Rect::new(2, 1, 10, 4);
    let layout = group.layout(area, &state);
    assert_eq!(layout.panels[0], area);
    assert!(layout.panels[1].is_empty());
    assert!(layout.dividers.iter().all(|divider| divider.is_empty()));
    assert_eq!(
        group.resize_divider(area, &mut state, 0, 2),
        PanelGroupOutcome::Ignored
    );
}

/// A leaf filling its constraints that records a press inside it.
struct Probe<'a>(&'a Cell<bool>);

impl Component for Probe<'_> {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            bmux_tui::component::LayoutId::new("probe"),
            constraints.constrain(bmux_tui::component::LogicalSize::new(
                constraints.max_width(),
                constraints.max_height().unwrap_or(1),
            )),
        )
    }

    fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Event::Mouse(mouse) = event else {
            return EventOutcome::Ignored;
        };
        let area = cx.visible_rect(bmux_tui::component::LogicalRect::new(
            0,
            0,
            layout.size.width,
            layout.size.height,
        ));
        if !area.contains(mouse.position) {
            return EventOutcome::Ignored;
        }
        self.0.set(true);
        EventOutcome::Handled
    }
}

#[test]
fn clicking_a_control_shows_the_new_state_on_the_first_frame_after() {
    let mut app = App::new(100, 30);
    for panel in ALL {
        for control in [WindowControl::Minimize, WindowControl::Maximize] {
            for _ in 0..2 {
                let painted = app.paint();
                let at = painted
                    .hit(&format!("{}.action.{}", panel.id(), control.id()))
                    .unwrap_or_else(|| panic!("{panel:?} {control:?}"));
                let pressed = app.press(&painted, Point::new(at.x + at.width / 2, at.y));
                assert_eq!(pressed, Some((panel, TitleTarget::Control(control))));
                let first = app.paint();
                app.assert_controls_agree(&first, &format!("{panel:?} {control:?}"));
            }
            assert_eq!(app.windows.shape(panel), PanelShape::Normal, "{panel:?}");
        }
    }
    // A double click on a title row toggles maximize.
    let painted = app.paint();
    let title = painted.area("under").expect("laid out");
    let at = Point::new(title.x + 2, title.y);
    app.press(&painted, at);
    assert_eq!(app.windows.shape(P::Under), PanelShape::Normal);
    app.press(&painted, at);
    assert_eq!(app.windows.shape(P::Under), PanelShape::Maximized);
    app.assert_controls_agree(&app.paint(), "double click");
}

#[test]
fn close_resize_and_reset_ask_the_application() {
    let mut app = App::new(100, 30);
    assert_eq!(
        app.command(P::Doc, WindowCommand::Close),
        WindowCommandOutcome::Refused(WindowRefusal::NotClosable)
    );
    app.windows.set_minimized(P::Left, true);
    assert_eq!(
        app.command(P::Left, WindowCommand::Close),
        WindowCommandOutcome::CloseRequested
    );
    assert!(
        !app.windows.is_minimized(P::Left),
        "closing forgets the state"
    );
    assert_eq!(
        app.command(P::Left, WindowCommand::ResetSize),
        WindowCommandOutcome::ResetSizeRequested
    );
    // Each panel's edge moves toward the fill panel, in the innermost group holding both.
    let resize = |panel| app.windows.resize(panel, 2);
    assert_eq!(
        resize(P::Left),
        Some(PanelResize {
            group: ROOT,
            divider: 0,
            delta: 2
        })
    );
    assert_eq!(
        resize(P::Doc),
        Some(PanelResize {
            group: COLUMN,
            divider: 0,
            delta: 2
        })
    );
    assert_eq!(
        resize(P::Under),
        Some(PanelResize {
            group: COLUMN,
            divider: 0,
            delta: -2
        })
    );
    assert_eq!(
        resize(P::Top),
        Some(PanelResize {
            group: ROOT,
            divider: 1,
            delta: -2
        })
    );
    assert_eq!(
        resize(P::Bottom),
        Some(PanelResize {
            group: ROOT,
            divider: 1,
            delta: -2
        })
    );
    let before = app.paint().area("left").expect("laid out");
    assert!(matches!(
        app.command(P::Left, WindowCommand::Grow),
        WindowCommandOutcome::Resize(_)
    ));
    assert_eq!(
        app.paint().area("left").map(|area| area.width),
        Some(before.width + 2)
    );
    // Minimized or while a panel is maximized, nothing resizes.
    app.windows.set_minimized(P::Left, true);
    assert_eq!(
        app.command(P::Left, WindowCommand::Grow),
        WindowCommandOutcome::Refused(WindowRefusal::NotResizable)
    );
    app.windows.toggle_maximize(P::Doc).expect("shown");
    assert_eq!(app.windows.resize(P::Under, 2), None);
}

#[test]
fn unshown_panels_refuse_and_unreached_groups_are_ignored() {
    let mut windows = PanelWindows::new();
    windows.set_tree(PanelTree::new(vec![
        PanelTreeGroup::new(PanelGroupAxis::Horizontal, vec![PanelMember::Panel(P::Doc)]),
        PanelTreeGroup::new(PanelGroupAxis::Vertical, vec![PanelMember::Panel(P::Under)]),
    ]));
    assert!(windows.shown(P::Doc));
    assert!(!windows.shown(P::Under));
    assert_eq!(
        windows.toggle_minimize(P::Under),
        Err(WindowRefusal::NotShown)
    );
    assert_eq!(
        windows.toggle_maximize(P::Under),
        Err(WindowRefusal::NotShown)
    );
    // A lone fill panel has nothing to resize against.
    assert_eq!(windows.resize(P::Doc, 2), None);
    // A maximized panel that is no longer shown is not maximized.
    windows.toggle_maximize(P::Doc).expect("shown");
    windows.set_tree(PanelTree::default());
    assert_eq!(windows.maximized(), None);
    assert_eq!(windows.shape(P::Doc), PanelShape::Normal);
    // A cycle in the tree is not followed.
    windows.set_tree(PanelTree::new(vec![PanelTreeGroup::new(
        PanelGroupAxis::Horizontal,
        vec![PanelMember::Panel(P::Doc), PanelMember::Group(0)],
    )]));
    assert!(windows.shown(P::Doc));
}

#[test]
fn controls_agree_with_painting_at_tiny_and_resized_sizes() {
    let sizes = [
        (100, 30),
        (80, 24),
        (60, 12),
        (40, 8),
        (24, 5),
        (12, 4),
        (5, 3),
        (3, 2),
        (1, 1),
        (0, 0),
        (90, 26),
    ];
    let states: [(&str, Setup); 4] = [
        ("normal", |_| {}),
        ("minimized", |windows| {
            for panel in [P::Left, P::Under, P::Top] {
                windows.set_minimized(panel, true);
            }
        }),
        ("strips", |windows| {
            windows.set_minimized(P::Top, true);
            windows.set_minimized(P::Bottom, true);
        }),
        ("maximized", |windows| {
            windows.toggle_maximize(P::Bottom).expect("shown");
        }),
    ];
    for (name, apply) in states {
        let mut app = App::new(100, 30);
        apply(&mut app.windows);
        for (width, height) in sizes {
            app.size = (width, height);
            app.sync();
            let painted = app.paint();
            app.assert_controls_agree(&painted, &format!("{name} at {width}x{height}"));
        }
    }
}

#[test]
fn group_states_ignore_out_of_range_lengths() {
    let windows = windows();
    let state = windows.group_state(7, vec![3, 4], Vec::new());
    assert_eq!(state.sizes(), [PanelSize::flex(3), PanelSize::flex(4)]);
    assert_eq!(state.maximized(), None);
}

/// The title row of `panel` in `painted`.
fn title_row(painted: &Painted, panel: P) -> String {
    let area = painted.area(panel.id()).expect("laid out");
    painted.text_in(Rect::new(area.x, area.y, area.width, 1))
}

/// The ids of `panel`'s painted controls.
fn painted_controls(painted: &Painted, panel: P) -> Vec<WindowControl> {
    let prefix = format!("{}.action.", panel.id());
    painted
        .hits
        .iter()
        .filter_map(|(id, _)| id.strip_prefix(&prefix).and_then(WindowControl::from_id))
        .collect()
}

#[test]
fn the_cluster_is_one_run_joined_to_the_border_with_the_state_glyphs() {
    let row = |controls: WindowControls| {
        let chrome = controls.apply(Pane::new().title("Guide").right_title("0/3"));
        let state = Cell::new(PaneState::new(Rect::new(0, 0, 24, 3)));
        let component = PaneComponent::new("pane", chrome, &state, Column::new());
        let layout = component.layout(Constraints::new(24, 24, 3, Some(3)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 3));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        drop(frame);
        buffer.row_symbols(0).unwrap_or_default()
    };
    let normal = WindowControls::new(PanelShape::Normal).min_title_width(4);
    assert_eq!(row(normal), "┌Guide───0/3─┤ ▾ ⤢ × ├─┐");
    assert_eq!(
        row(WindowControls::new(PanelShape::Maximized).min_title_width(4)),
        "┌Guide───0/3─┤ ▾ ⤡ × ├─┐"
    );
    // A minimized row offers only its restore control, shown even when the others hide.
    assert_eq!(
        row(WindowControls::new(PanelShape::Row).shown(false)),
        "┌Guide───────0/3─┤ ▸ ├─┐"
    );
    assert_eq!(
        row(normal.glyphs(WindowGlyphs::ASCII)),
        "┌Guide───0/3─| v + x |─┐"
    );
    assert_eq!(row(normal.closable(false)), "┌Guide─────0/3─┤ ▾ ⤢ ├─┐");
    // Hidden, the title row has no controls and the right title takes the room.
    assert_eq!(row(normal.shown(false)), "┌Guide──────────────0/3┐");
}

#[test]
fn hidden_controls_have_no_hit_regions_or_targets() {
    let controls = WindowControls::new(PanelShape::Normal).shown(false);
    assert_eq!(controls.painted(), []);
    assert_eq!(controls.list().len(), 3);
    let chrome = controls.apply(
        Pane::new()
            .title("Files")
            .action(PaneTitleAction::new("own", "+ own")),
    );
    assert_eq!(chrome.actions().len(), 1);
    let area = Rect::new(3, 2, 30, 6);
    for x in area.x + 1..area.right() - 1 {
        let target = controls.target(&chrome, area, Point::new(x, area.y));
        assert!(
            matches!(target, Some(TitleTarget::Title | TitleTarget::Action(0))),
            "{x}: {target:?}"
        );
    }
    // A hovered control that is not painted styles nothing.
    let hovered = controls
        .hovered(Some(WindowControl::Close))
        .apply(Pane::new());
    assert_eq!(
        hovered,
        Pane::new().min_title_width(super::DEFAULT_MIN_TITLE)
    );
}

#[test]
fn hovered_controls_take_their_hover_styles() {
    use bmux_tui::style::{Color, Style};
    let hover = Style::new().fg(Color::Blue);
    let close = Style::new().fg(Color::Red);
    let muted = Style::new().fg(Color::Yellow);
    let fg_at = |hovered: Option<WindowControl>| {
        let chrome = WindowControls::new(PanelShape::Normal)
            .style(muted)
            .hover_styles(hover, close)
            .hovered(hovered)
            .apply(Pane::new());
        let state = Cell::new(PaneState::new(Rect::new(0, 0, 13, 3)));
        let component = PaneComponent::new("pane", chrome, &state, Column::new());
        let layout = component.layout(Constraints::new(13, 13, 3, Some(3)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 13, 3));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        drop(frame);
        assert_eq!(buffer.row_symbols(0).as_deref(), Some("┌─┤ ▾ ⤢ × ├─┐"));
        [4u16, 6, 8].map(|x| buffer.get(Point::new(x, 0)).and_then(|cell| cell.style.fg))
    };
    let muted = muted.fg;
    assert_eq!(fg_at(None), [muted, muted, muted]);
    assert_eq!(
        fg_at(Some(WindowControl::Close)),
        [muted, muted, Some(Color::Red)]
    );
    assert_eq!(
        fg_at(Some(WindowControl::Maximize)),
        [muted, Some(Color::Blue), muted]
    );
    assert_eq!(
        fg_at(Some(WindowControl::Minimize)),
        [Some(Color::Blue), muted, muted]
    );
}

#[test]
fn controls_show_on_the_focused_and_hovered_panels_and_minimized_restores_always() {
    let mut app = App::new(100, 30);
    app.windows.set_minimized(P::Under, true);
    app.windows.set_minimized(P::Top, true);
    app.windows.set_minimized(P::Bottom, true);
    app.sync();
    app.view = Some(View {
        focused: Some(P::Doc),
        ..View::default()
    });
    let painted = app.paint();
    app.assert_controls_agree(&painted, "focused");
    assert_eq!(painted_controls(&painted, P::Doc).len(), 2, "doc: no close");
    assert_eq!(painted_controls(&painted, P::Left), []);
    assert!(!title_row(&painted, P::Left).contains('▾'));
    // Minimized panels keep their restore control: a row's cluster and a strip's control.
    for panel in [P::Under, P::Top, P::Bottom] {
        assert_eq!(
            painted_controls(&painted, panel),
            [WindowControl::Minimize],
            "{panel:?}"
        );
    }
    assert!(title_row(&painted, P::Under).contains("┤ ▸ ├"));
    // The pointer over the left panel shows its controls on the first frame after.
    let left = painted.area("left").expect("laid out");
    app.hover(&painted, Point::new(left.x + 2, left.y + 3));
    let painted = app.paint();
    app.assert_controls_agree(&painted, "hovered");
    assert_eq!(painted_controls(&painted, P::Left).len(), 3);
    assert!(title_row(&painted, P::Left).contains("┤ ▾ ⤢ × ├"));
    // Over its close control, that control is hovered; a press closes the panel.
    let close = painted.hit("left.action.close").expect("painted");
    let at = Point::new(close.x, close.y);
    app.hover(&painted, at);
    assert_eq!(
        app.view.and_then(|view| view.control),
        Some(WindowControl::Close)
    );
    let painted = app.paint();
    assert_eq!(painted.hit("left.action.close"), Some(close));
    assert_eq!(
        app.press(&painted, at),
        Some((P::Left, TitleTarget::Control(WindowControl::Close)))
    );
    // Leaving the panel hides its controls with their regions; a press where the close
    // control was is on the title.
    let mut app = App::new(100, 30);
    app.view = Some(View::default());
    let painted = app.paint();
    let left = painted.area("left").expect("laid out");
    app.hover(&painted, Point::new(left.x + 2, left.y + 3));
    let shown = app.paint();
    let close = shown.hit("left.action.close").expect("painted");
    let doc = shown.area("doc").expect("laid out");
    app.hover(&shown, Point::new(doc.x + 2, doc.y + 3));
    let hidden = app.paint();
    app.assert_controls_agree(&hidden, "left");
    assert_eq!(painted_controls(&hidden, P::Left), []);
    assert!(matches!(
        app.press(&hidden, Point::new(close.x, close.y)),
        Some((P::Left, TitleTarget::Title | TitleTarget::Action(_)))
    ));
    assert_eq!(app.windows.shape(P::Left), PanelShape::Normal);
}

#[test]
fn shown_and_hidden_controls_agree_with_painting_at_every_size() {
    let sizes = [
        (100, 30),
        (40, 8),
        (24, 5),
        (12, 4),
        (5, 3),
        (3, 2),
        (1, 1),
        (0, 0),
    ];
    for focused in [None, Some(P::Doc), Some(P::Bottom)] {
        let mut app = App::new(100, 30);
        app.windows.set_minimized(P::Top, true);
        app.view = Some(View {
            focused,
            hovered: Some(P::Left),
            control: Some(WindowControl::Close),
        });
        for (width, height) in sizes {
            app.size = (width, height);
            app.sync();
            let painted = app.paint();
            app.assert_controls_agree(&painted, &format!("{focused:?} at {width}x{height}"));
            for panel in [P::Under, P::Doc] {
                if focused != Some(panel) {
                    assert_eq!(painted_controls(&painted, panel), [], "{panel:?}");
                }
            }
        }
    }
}

/// Controls painted inline at the end of a borderless row: right-aligned, a space between each
/// two while they fit, packed when not, and cut from the right when even that does not fit; each
/// with its hit region and its hover style.
#[test]
fn inline_controls_paint_at_the_row_end_with_hit_regions_and_hover_styles() {
    use bmux_tui::style::{Color, Style};
    let paint = |controls: WindowControls, width: u16| {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
        let mut frame = Frame::new(&mut buffer);
        let start = controls.paint_inline(
            "panel",
            Rect::new(0, 0, width, 1),
            &mut PaintCx::new(&mut frame),
        );
        let hits = frame
            .hits()
            .regions()
            .iter()
            .map(|region| (region.id.as_str().to_owned(), region.area))
            .collect::<Vec<_>>();
        drop(frame);
        (start, buffer, hits)
    };
    let row = |buffer: &Buffer| buffer.row_symbols(0).unwrap_or_default();
    let controls = WindowControls::new(PanelShape::Normal).maximizable(false);
    assert_eq!(controls.inline_width(), 3);
    let (start, buffer, hits) = paint(controls, 10);
    assert_eq!((start, row(&buffer).as_str()), (7, "       ▾ ×"));
    assert_eq!(
        hits,
        [
            ("panel.action.minimize".to_owned(), Rect::new(7, 0, 1, 1)),
            ("panel.action.close".to_owned(), Rect::new(9, 0, 1, 1)),
        ]
    );
    let (start, buffer, _) = paint(controls, 2);
    assert_eq!((start, row(&buffer).as_str()), (0, "▾×"));
    let (_, buffer, hits) = paint(controls, 1);
    assert_eq!(row(&buffer), "▾");
    assert_eq!(hits.len(), 1);

    // All three, and only restore on a strip.
    let all = WindowControls::new(PanelShape::Normal);
    assert_eq!(all.inline_width(), 5);
    assert_eq!(row(&paint(all, 6).1), " ▾ ⤢ ×");
    let strip = WindowControls::new(PanelShape::Strip);
    assert_eq!(strip.inline_width(), 1);
    assert_eq!(row(&paint(strip, 3).1), "  ▸");

    // Hidden controls paint nothing.
    let (_, buffer, hits) = paint(controls.shown(false), 4);
    assert_eq!((row(&buffer).as_str(), hits.len()), ("    ", 0));

    // The hovered control takes its hover style; close its own.
    let styled = |hovered| {
        let controls = controls
            .style(Style::new().fg(Color::Yellow))
            .hover_styles(Style::new().fg(Color::Blue), Style::new().fg(Color::Red))
            .hovered(hovered);
        let (_, buffer, _) = paint(controls, 3);
        [0, 2].map(|x| buffer.get(Point::new(x, 0)).unwrap().style.fg)
    };
    assert_eq!(styled(None), [Some(Color::Yellow), Some(Color::Yellow)]);
    assert_eq!(
        styled(Some(WindowControl::Minimize)),
        [Some(Color::Blue), Some(Color::Yellow)]
    );
    assert_eq!(
        styled(Some(WindowControl::Close)),
        [Some(Color::Yellow), Some(Color::Red)]
    );
}

/// `maximizable(false)` drops maximize from an expanded panel's controls; a minimized panel
/// offers only restore whatever its options, and its strip is as wide as that control.
#[test]
fn maximizable_and_strip_width_follow_the_offered_controls() {
    use WindowControl::{Close, Maximize, Minimize};
    assert_eq!(
        WindowControls::new(PanelShape::Normal).list(),
        [Minimize, Maximize, Close]
    );
    assert_eq!(
        WindowControls::new(PanelShape::Normal)
            .maximizable(false)
            .list(),
        [Minimize, Close]
    );
    assert_eq!(
        WindowControls::new(PanelShape::Maximized)
            .maximizable(false)
            .closable(false)
            .list(),
        [Minimize]
    );
    for shape in [PanelShape::Row, PanelShape::Strip] {
        assert_eq!(
            WindowControls::new(shape).maximizable(false).list(),
            [Minimize]
        );
    }
    // A strip holds its padded restore control between two corners, from any shape.
    for controls in [
        WindowControls::new(PanelShape::Normal),
        WindowControls::new(PanelShape::Normal).maximizable(false),
        WindowControls::new(PanelShape::Maximized).closable(false),
        WindowControls::new(PanelShape::Normal).glyphs(WindowGlyphs::ASCII),
    ] {
        assert_eq!(controls.strip_width(), 5);
    }
}
