//! Split button: a primary action plus a `▾` toggle that opens an anchored
//! menu of alternatives (for example "Copy as CSV" with other formats in the
//! menu).
//!
//! The control is immediate-mode, like [`Button`] and [`Menu`]: the caller
//! owns [`SplitButtonState`], routes events through
//! [`SplitButton::handle_event`], paints the buttons with
//! [`SplitButton::paint`], and paints the popup last (above other content)
//! with [`SplitButton::paint_menu`]. Use [`choice`] to build menu items with
//! a check mark for the current choice.

use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::{PopupAlign, Rect, Size};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};

use crate::button::{Button, ButtonOutcome, ButtonState, ButtonStyles};
use crate::menu::{Menu, MenuItem, MenuOutcome, MenuPolicy, MenuState, MenuStyles};

/// Runtime split-button state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SplitButtonState {
    /// Primary action button.
    pub primary: ButtonState,
    /// Menu toggle button.
    pub toggle: ButtonState,
    /// Menu state while open.
    pub menu: MenuState,
    /// Whether the menu is open.
    pub open: bool,
}

impl SplitButtonState {
    /// Closed, enabled state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            primary: ButtonState::new(),
            toggle: ButtonState::new(),
            menu: MenuState::new(None),
            open: false,
        }
    }

    /// Enable or disable the whole control (closing the menu when disabled).
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.primary.set_disabled(disabled);
        self.toggle.set_disabled(disabled);
        if disabled {
            self.open = false;
        }
    }

    /// Open the menu with keyboard focus on `focused` (the current choice).
    pub const fn open_at(&mut self, focused: Option<usize>) {
        self.open = true;
        self.menu = MenuState::new(None);
        self.menu.set_focused(focused);
    }

    /// Close the menu.
    pub const fn close(&mut self) {
        self.open = false;
    }
}

/// Outcome from handling a split-button event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitButtonOutcome {
    /// The event was not for this control.
    Ignored,
    /// Handled without a visual change.
    Handled,
    /// Handled; repaint.
    Redraw,
    /// The primary action was activated.
    Primary,
    /// The menu opened.
    Opened,
    /// The menu closed without a choice (Escape, a click outside, or the
    /// toggle again).
    Closed,
    /// A menu item was chosen (the menu has closed).
    Selected {
        /// Item index.
        index: usize,
        /// Item id.
        id: String,
    },
}

impl SplitButtonOutcome {
    /// Whether the control consumed the event.
    #[must_use]
    pub const fn is_handled(&self) -> bool {
        !matches!(self, Self::Ignored)
    }
}

/// Styles for a split button and its popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitButtonStyles {
    /// Both buttons.
    pub button: ButtonStyles,
    /// The popup menu.
    pub menu: MenuStyles,
    /// The popup's border.
    pub border: Style,
}

impl Default for SplitButtonStyles {
    fn default() -> Self {
        Self {
            button: ButtonStyles::default(),
            menu: MenuStyles::default(),
            border: Style::new(),
        }
    }
}

/// A primary action with a menu of alternatives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitButton<'a> {
    label: &'a str,
    toggle_label: &'a str,
    items: &'a [MenuItem],
    padding: (u16, u16),
    styles: SplitButtonStyles,
    menu_policy: MenuPolicy,
}

/// A menu item for a set of mutually exclusive choices: `✓ label` when
/// `checked`, aligned `  label` otherwise.
#[must_use]
pub fn choice(id: impl Into<String>, label: &str, checked: bool) -> MenuItem {
    MenuItem::rich(
        id,
        Line::from_spans(vec![
            Span::raw(if checked { "✓ " } else { "  " }),
            Span::raw(label.to_string()),
        ]),
    )
}

impl<'a> SplitButton<'a> {
    /// A split button labelled `label` whose toggle opens `items`.
    #[must_use]
    pub fn new(label: &'a str, items: &'a [MenuItem]) -> Self {
        Self {
            label,
            toggle_label: "▾",
            items,
            padding: (1, 1),
            styles: SplitButtonStyles::default(),
            menu_policy: MenuPolicy::context_menu(),
        }
    }

    /// Set the toggle's label (default `▾`).
    #[must_use]
    pub const fn toggle_label(mut self, label: &'a str) -> Self {
        self.toggle_label = label;
        self
    }

    /// Set both buttons' left and right padding (default one cell each).
    #[must_use]
    pub const fn padding(mut self, left: u16, right: u16) -> Self {
        self.padding = (left, right);
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: SplitButtonStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Set the popup menu's behavior.
    #[must_use]
    pub const fn menu_policy(mut self, policy: MenuPolicy) -> Self {
        self.menu_policy = policy;
        self
    }

    const fn primary_button(&self) -> Button<'a> {
        Button::new(self.label)
            .filled_padding(self.padding.0, self.padding.1)
            .styles(self.styles.button)
    }

    const fn toggle_button(&self) -> Button<'a> {
        Button::new(self.toggle_label)
            .filled_padding(self.padding.0, self.padding.1)
            .styles(self.styles.button)
    }

    fn menu(&self) -> Menu<'a> {
        Menu::new(self.items)
            .policy(self.menu_policy)
            .styles(self.styles.menu)
    }

    /// Rendered width: both buttons and the one-cell separator between them.
    #[must_use]
    pub fn width(&self) -> u16 {
        self.primary_button()
            .width()
            .saturating_add(1)
            .saturating_add(self.toggle_button().width())
    }

    /// The primary and toggle buttons' rectangles within `area`.
    #[must_use]
    pub fn parts(&self, area: Rect) -> (Rect, Rect) {
        let primary = self.primary_button().width().min(area.width);
        let toggle_x = area.x.saturating_add(primary).saturating_add(1);
        let toggle = self
            .toggle_button()
            .width()
            .min(area.right().saturating_sub(toggle_x));
        (
            Rect::new(area.x, area.y, primary, 1),
            Rect::new(toggle_x, area.y, toggle, 1),
        )
    }

    /// Where the popup (including its border) appears: below the control,
    /// or above when there is no room below, right-aligned to the control
    /// and kept inside `bounds`.
    #[must_use]
    pub fn menu_area(&self, area: Rect, bounds: Rect) -> Rect {
        let (width, height) = self.menu().size();
        area.popup(
            Size::new(width.saturating_add(2), height.saturating_add(2)),
            bounds,
            PopupAlign::End,
        )
    }

    fn menu_list_area(&self, area: Rect, bounds: Rect) -> Rect {
        let outer = self.menu_area(area, bounds);
        Rect::new(
            outer.x.saturating_add(1),
            outer.y.saturating_add(1),
            outer.width.saturating_sub(2),
            outer.height.saturating_sub(2),
        )
    }

    /// Handle one event for the control at `area`, with its popup kept in
    /// `bounds`. Keys are only handled while the menu is open or while one
    /// of the buttons has keyboard focus.
    pub fn handle_event(
        &self,
        area: Rect,
        bounds: Rect,
        state: &mut SplitButtonState,
        event: &Event,
    ) -> SplitButtonOutcome {
        if state.open {
            return self.handle_open(area, bounds, state, event);
        }
        let focused = state.primary.focused() || state.toggle.focused();
        if let Event::Key(stroke) = event {
            if !focused {
                return SplitButtonOutcome::Ignored;
            }
            if stroke.key == KeyCode::Down && stroke.modifiers.alt {
                state.open_at(Some(0));
                return SplitButtonOutcome::Opened;
            }
        }
        let (primary, toggle) = self.parts(area);
        let first = self
            .primary_button()
            .handle_event(primary, &mut state.primary, event);
        if first == ButtonOutcome::Pressed {
            return SplitButtonOutcome::Primary;
        }
        if matches!(event, Event::Key(_)) && state.primary.focused() {
            return outcome(first);
        }
        let second = self
            .toggle_button()
            .handle_event(toggle, &mut state.toggle, event);
        if second == ButtonOutcome::Pressed {
            state.open_at(Some(0));
            return SplitButtonOutcome::Opened;
        }
        if first.needs_redraw() || second.needs_redraw() {
            SplitButtonOutcome::Redraw
        } else if first.is_handled() || second.is_handled() {
            SplitButtonOutcome::Handled
        } else {
            SplitButtonOutcome::Ignored
        }
    }

    fn handle_open(
        &self,
        area: Rect,
        bounds: Rect,
        state: &mut SplitButtonState,
        event: &Event,
    ) -> SplitButtonOutcome {
        let list = self.menu_list_area(area, bounds);
        if let Event::Mouse(mouse) = event
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && !self.menu_area(area, bounds).contains(mouse.position)
        {
            // A click outside (including on the toggle) closes the menu.
            state.close();
            state.toggle.interaction.hovered = false;
            state.toggle.interaction.pressed = false;
            return SplitButtonOutcome::Closed;
        }
        match self.menu().handle_event(list, &mut state.menu, event) {
            MenuOutcome::Activated { index, id } => {
                state.close();
                SplitButtonOutcome::Selected { index, id }
            }
            MenuOutcome::Cancelled => {
                state.close();
                SplitButtonOutcome::Closed
            }
            MenuOutcome::Ignored => {
                // Keys and pointer events stay with the open menu.
                if matches!(event, Event::Key(_) | Event::Mouse(_)) {
                    SplitButtonOutcome::Handled
                } else {
                    SplitButtonOutcome::Ignored
                }
            }
            MenuOutcome::Redraw | MenuOutcome::Focused(_) | MenuOutcome::Typeahead(_) => {
                SplitButtonOutcome::Redraw
            }
        }
    }

    /// Paint both buttons at `area`, registering `{id}.primary` and
    /// `{id}.toggle` hit regions.
    pub fn paint(&self, id: &str, area: Rect, state: &SplitButtonState, cx: &mut PaintCx<'_, '_>) {
        let (primary, toggle) = self.parts(area);
        let mut toggle_state = state.toggle;
        // The toggle reads as pressed while its menu is open.
        toggle_state.interaction.pressed |= state.open;
        for (suffix, rect, line, button_state) in [
            (
                "primary",
                primary,
                self.primary_button().line(state.primary),
                state.primary,
            ),
            (
                "toggle",
                toggle,
                self.toggle_button().line(toggle_state),
                toggle_state,
            ),
        ] {
            if rect.is_empty() {
                continue;
            }
            cx.write_line(LocalRect::terminal(rect), &line);
            cx.push_hit(
                HitRegion::new(format!("{id}.{suffix}"), rect)
                    .role(HitRole::Action)
                    .enabled(!button_state.interaction.disabled),
            );
        }
        let separator = Rect::new(primary.right(), area.y, 1, 1);
        if separator.right() <= area.right() {
            cx.write_line(
                LocalRect::terminal(separator),
                &Line::from_spans(vec![Span::styled("│", self.styles.border)]),
            );
        }
    }

    /// Paint the open popup (nothing when closed). Paint it after other
    /// content: it occludes what lies beneath, including hit regions.
    pub fn paint_menu(
        &self,
        id: &str,
        area: Rect,
        bounds: Rect,
        state: &SplitButtonState,
        cx: &mut PaintCx<'_, '_>,
    ) {
        if !state.open {
            return;
        }
        let outer = self.menu_area(area, bounds);
        if outer.width < 3 || outer.height < 3 {
            return;
        }
        cx.occlude(LocalRect::terminal(outer));
        let background = self.styles.menu.background;
        cx.fill(LocalRect::terminal(outer), " ", background);
        let border = background.patch(self.styles.border);
        let inner = usize::from(outer.width - 2);
        let rows = [
            (outer.y, format!("╭{}╮", "─".repeat(inner))),
            (outer.bottom() - 1, format!("╰{}╯", "─".repeat(inner))),
        ];
        for (y, text) in rows {
            cx.write_line(
                LocalRect::terminal(Rect::new(outer.x, y, outer.width, 1)),
                &Line::from_spans(vec![Span::styled(text, border)]),
            );
        }
        for y in outer.y + 1..outer.bottom() - 1 {
            for x in [outer.x, outer.right() - 1] {
                cx.set_cell(i32::from(x), i64::from(y), "│", border);
            }
        }
        let list = self.menu_list_area(area, bounds);
        cx.push_hit(HitRegion::new(format!("{id}.menu"), outer).role(HitRole::Background));
        self.menu().paint(list, &state.menu, background, cx);
        cx.push_hit(HitRegion::new(format!("{id}.menu.items"), list).role(HitRole::ListItem));
    }
}

const fn outcome(outcome: ButtonOutcome) -> SplitButtonOutcome {
    match outcome {
        ButtonOutcome::Ignored => SplitButtonOutcome::Ignored,
        ButtonOutcome::Handled => SplitButtonOutcome::Handled,
        ButtonOutcome::Redraw => SplitButtonOutcome::Redraw,
        ButtonOutcome::Pressed => SplitButtonOutcome::Primary,
    }
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyStroke, Modifiers};
    use bmux_tui::event::MouseEvent;
    use bmux_tui::geometry::Point;

    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke {
            key: code,
            modifiers: Modifiers::NONE,
        })
    }

    fn items() -> Vec<MenuItem> {
        vec![
            choice("csv", "CSV", true),
            choice("tsv", "TSV", false),
            choice("json", "JSON", false).disabled(true),
        ]
    }

    fn click(x: u16, y: u16) -> [Event; 2] {
        [
            Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(x, y),
            )),
            Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(x, y),
            )),
        ]
    }

    fn send(
        button: &SplitButton<'_>,
        area: Rect,
        state: &mut SplitButtonState,
        events: &[Event],
    ) -> SplitButtonOutcome {
        let bounds = Rect::new(0, 0, 40, 12);
        let mut last = SplitButtonOutcome::Ignored;
        for event in events {
            last = button.handle_event(area, bounds, state, event);
        }
        last
    }

    #[test]
    fn primary_and_toggle_split_the_control() {
        let items = items();
        let button = SplitButton::new("Copy", &items);
        assert_eq!(button.width(), 6 + 1 + 3);
        let area = Rect::new(10, 2, button.width(), 1);
        let (primary, toggle) = button.parts(area);
        assert_eq!(
            (primary.x, primary.width, toggle.x, toggle.width),
            (10, 6, 17, 3)
        );

        let mut state = SplitButtonState::new();
        assert_eq!(
            send(&button, area, &mut state, &click(11, 2)),
            SplitButtonOutcome::Primary
        );
        assert_eq!(
            send(&button, area, &mut state, &click(18, 2)),
            SplitButtonOutcome::Opened
        );
        assert!(state.open);
    }

    #[test]
    fn menu_opens_below_or_above_and_selects_or_closes() {
        let items = items();
        let button = SplitButton::new("Copy", &items);
        let bounds = Rect::new(0, 0, 40, 12);
        let low = Rect::new(10, 10, button.width(), 1);
        let menu = button.menu_area(low, bounds);
        assert!(menu.bottom() <= low.y, "no room below: opens above");
        assert_eq!(menu.right(), low.right(), "right-aligned to the control");
        let high = Rect::new(10, 1, button.width(), 1);
        assert_eq!(button.menu_area(high, bounds).y, 2, "opens below");

        let mut state = SplitButtonState::new();
        state.open_at(Some(0));
        let down = key(KeyCode::Down);
        let enter = key(KeyCode::Enter);
        assert_eq!(
            send(&button, high, &mut state, &[down, enter]),
            SplitButtonOutcome::Selected {
                index: 1,
                id: "tsv".to_string()
            }
        );
        assert!(!state.open);

        state.open_at(Some(0));
        let escape = key(KeyCode::Escape);
        assert_eq!(
            send(&button, high, &mut state, &[escape]),
            SplitButtonOutcome::Closed
        );
        state.open_at(Some(0));
        assert_eq!(
            send(&button, high, &mut state, &click(0, 11)[..1]),
            SplitButtonOutcome::Closed,
            "a click outside closes"
        );
    }

    #[test]
    fn keys_need_focus_and_alt_down_opens() {
        let items = items();
        let button = SplitButton::new("Copy", &items);
        let area = Rect::new(0, 0, button.width(), 1);
        let mut state = SplitButtonState::new();
        let alt_down = Event::Key(KeyStroke {
            key: KeyCode::Down,
            modifiers: Modifiers {
                alt: true,
                ..Modifiers::NONE
            },
        });
        assert_eq!(
            send(&button, area, &mut state, std::slice::from_ref(&alt_down)),
            SplitButtonOutcome::Ignored
        );
        state.primary.set_focused(true);
        assert_eq!(
            send(&button, area, &mut state, &[alt_down]),
            SplitButtonOutcome::Opened
        );
        state.close();
        assert_eq!(
            send(&button, area, &mut state, &[key(KeyCode::Enter)]),
            SplitButtonOutcome::Primary
        );
    }

    #[test]
    fn paints_buttons_and_the_open_popup() {
        use bmux_tui::buffer::Buffer;
        use bmux_tui::frame::Frame;

        let items = items();
        let button = SplitButton::new("Copy", &items);
        let area = Rect::new(20, 0, button.width(), 1);
        let bounds = Rect::new(0, 0, 40, 8);
        let mut state = SplitButtonState::new();
        state.open_at(Some(0));
        let mut buffer = Buffer::empty(bounds);
        let mut frame = Frame::new(&mut buffer);
        let mut cx = PaintCx::new(&mut frame);
        button.paint("copy", area, &state, &mut cx);
        button.paint_menu("copy", area, bounds, &state, &mut cx);
        let rows: Vec<String> = (0..5)
            .map(|y| buffer.row_symbols(y).unwrap_or_default())
            .collect();
        assert!(rows[0].contains(" Copy │ ▾ "), "{rows:#?}");
        assert!(rows[1].trim_end().ends_with('╮'), "{rows:#?}");
        assert!(rows[2].contains("✓ CSV"), "{rows:#?}");
        assert!(rows[3].contains("TSV"), "{rows:#?}");
    }

    #[test]
    fn choices_mark_the_current_item() {
        let item = choice("csv", "CSV", true);
        assert_eq!(item.lines[0].spans[0].content, "✓ ");
        assert_eq!(choice("tsv", "TSV", false).lines[0].spans[0].content, "  ");
    }
}
