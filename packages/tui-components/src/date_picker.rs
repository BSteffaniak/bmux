//! Calendar date, date-time, and time picker control.
//!
//! [`DatePicker`] is an immediate-mode control over caller-owned
//! [`DatePickerState`]. It renders a month header with `‹`/`›` navigation, a
//! weekday header, a six-week day grid, and (for time modes) an
//! `HH : MM : SS` row. The caller supplies the "today" reference date; the
//! control never reads the system clock while painting.
//!
//! ```text
//! ‹  November 2026   ›
//! Mo Tu We Th Fr Sa Su
//! 26 27 28 29 30 31  1
//!  2  3  4  5  6  7  8
//!  ...
//!
//!     14 : 30 : 00
//! ```
//!
//! Use [`DatePicker::preferred_size`] with
//! [`Rect::popup`](bmux_tui::geometry::Rect::popup) to anchor the picker as a
//! popup, or [`DatePickerComponent`] for the canonical component lifecycle.

use std::cell::Cell;
use std::hash::{Hash, Hasher};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_text_edit::pointer::{ClickPolicy, ClickTracker};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, FocusEvent, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::hit::{HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::Modifier;
use chrono::{Datelike, Days, Months, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::common::{ComponentMousePolicy, InteractionState};

/// Width of one day cell in terminal columns.
const DAY_WIDTH: u16 = 2;
/// Horizontal distance between the starts of adjacent day cells.
const DAY_STRIDE: u16 = 3;
/// Width of the calendar (seven day cells separated by single spaces).
const CALENDAR_WIDTH: u16 = DAY_STRIDE * 6 + DAY_WIDTH;
/// Number of week rows in the day grid.
const GRID_ROWS: u16 = 6;
/// Number of day cells in the day grid.
const GRID_CELLS: usize = 42;
/// Rows used by the calendar: header, weekday header, and the day grid.
const CALENDAR_HEIGHT: u16 = 2 + GRID_ROWS;
/// Column offsets of the hour, minute, and second fields in the time row.
const TIME_FIELD_OFFSETS: [u16; 3] = [0, 5, 10];
/// Separator painted between time fields.
const TIME_SEPARATOR: &str = " : ";

/// Default English month names, January first.
pub const ENGLISH_MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Default English two-letter weekday names, Monday first.
pub const ENGLISH_WEEKDAY_NAMES: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];

/// Which values a [`DatePicker`] edits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DatePickerMode {
    /// Calendar only.
    #[default]
    Date,
    /// Calendar plus a time row.
    DateTime,
    /// Time row only.
    Time,
}

impl DatePickerMode {
    /// Return whether this mode shows the calendar.
    #[must_use]
    pub const fn has_date(self) -> bool {
        matches!(self, Self::Date | Self::DateTime)
    }

    /// Return whether this mode shows the time row.
    #[must_use]
    pub const fn has_time(self) -> bool {
        matches!(self, Self::DateTime | Self::Time)
    }
}

/// First day of the week shown in the calendar's leftmost column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum WeekStart {
    /// Weeks start on Monday (ISO 8601).
    #[default]
    Monday,
    /// Weeks start on Sunday.
    Sunday,
}

impl WeekStart {
    /// Return the number of columns `weekday` sits after the week start.
    #[must_use]
    pub const fn column_of(self, weekday: Weekday) -> u32 {
        match self {
            Self::Monday => weekday.num_days_from_monday(),
            Self::Sunday => weekday.num_days_from_sunday(),
        }
    }
}

/// Part of the picker that receives keyboard input.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum DatePickerPart {
    /// The calendar day grid.
    #[default]
    Calendar,
    /// The hour field.
    Hour,
    /// The minute field.
    Minute,
    /// The second field.
    Second,
}

impl DatePickerPart {
    const fn time_index(self) -> Option<usize> {
        match self {
            Self::Calendar => None,
            Self::Hour => Some(0),
            Self::Minute => Some(1),
            Self::Second => Some(2),
        }
    }

    const fn max_value(self) -> u32 {
        match self {
            Self::Calendar => 0,
            Self::Hour => 23,
            Self::Minute | Self::Second => 59,
        }
    }
}

/// Visual styles for a date picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatePickerStyles {
    /// Day numbers in the viewed month and time values.
    pub normal: Style,
    /// Month and year title.
    pub header: Style,
    /// `‹` / `›` month navigation arrows.
    pub arrow: Style,
    /// Weekday header row.
    pub weekday: Style,
    /// Days outside the viewed month.
    pub outside: Style,
    /// Overlay marking the caller-supplied "today" date.
    pub today: Style,
    /// Overlay for the selected day while the calendar is not focused.
    pub selected: Style,
    /// Overlay for the selected day or time field that has keyboard focus.
    pub focused: Style,
    /// Time field separators.
    pub separator: Style,
    /// Every cell while the control is disabled.
    pub disabled: Style,
}

impl Default for DatePickerStyles {
    fn default() -> Self {
        Self {
            normal: Style::new(),
            header: Style::new().add_modifier(Modifier::BOLD),
            arrow: Style::new(),
            weekday: Style::new().add_modifier(Modifier::DIM),
            outside: Style::new().add_modifier(Modifier::DIM),
            today: Style::new().add_modifier(Modifier::UNDERLINE),
            selected: Style::new().add_modifier(Modifier::REVERSED),
            focused: Style::new()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
            separator: Style::new().add_modifier(Modifier::DIM),
            disabled: Style::new().add_modifier(Modifier::DIM),
        }
    }
}

impl DatePickerStyles {
    /// Derive styles from a semantic theme resolved for one surface depth.
    ///
    /// Popup pickers typically use
    /// [`ComponentSurfaceDepth::Overlay`](crate::theme::ComponentSurfaceDepth::Overlay).
    #[must_use]
    pub const fn from_theme(
        theme: crate::theme::ComponentTheme,
        depth: crate::theme::ComponentSurfaceDepth,
    ) -> Self {
        let theme = theme.for_surface(depth);
        Self {
            normal: theme.text,
            header: theme.text.patch(Style::new().add_modifier(Modifier::BOLD)),
            arrow: theme.info,
            weekday: theme.muted,
            outside: theme.muted,
            today: theme
                .info
                .patch(Style::new().add_modifier(Modifier::UNDERLINE)),
            selected: theme.selected,
            focused: theme.focused,
            separator: theme.muted,
            disabled: theme.disabled,
        }
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`DatePickerStyles`] on the
    /// normal surface.
    #[must_use]
    pub const fn date_picker_styles(self) -> DatePickerStyles {
        DatePickerStyles::from_theme(self, crate::theme::ComponentSurfaceDepth::Normal)
    }
}

impl From<crate::theme::ComponentTheme> for DatePickerStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        theme.date_picker_styles()
    }
}

/// Configurable date picker behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // Independent opt-in behavior switches.
pub struct DatePickerPolicy {
    /// Mouse behavior. `click` gates day/arrow/field clicks.
    pub mouse: ComponentMousePolicy,
    /// Whether the mouse wheel changes the viewed month (over the calendar)
    /// and the hovered time field.
    pub wheel: bool,
    /// Whether Enter submits the current value.
    pub enter_submits: bool,
    /// Whether Escape cancels.
    pub escape_cancels: bool,
    /// Whether double-clicking a day submits it.
    pub double_click_submits: bool,
    /// Whether Tab / Shift+Tab move focus between the calendar and time fields.
    pub tab_cycles: bool,
    /// Whether Tab past the last part wraps to the first (and Shift+Tab past
    /// the first wraps to the last). When `false` the key is ignored at the
    /// edges so the caller can move focus out of the picker.
    pub tab_wraps: bool,
}

impl DatePickerPolicy {
    /// Common interactive picker behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            mouse: ComponentMousePolicy::button(),
            wheel: true,
            enter_submits: true,
            escape_cancels: true,
            double_click_submits: true,
            tab_cycles: true,
            tab_wraps: true,
        }
    }
}

impl Default for DatePickerPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Runtime date picker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatePickerState {
    /// Common interaction flags. Set `interaction.focused` while the picker
    /// owns keyboard focus to paint the focused part.
    pub interaction: InteractionState,
    date: NaiveDate,
    time: NaiveTime,
    view: NaiveDate,
    today: Option<NaiveDate>,
    part: DatePickerPart,
    pending_digit: Option<u8>,
    #[serde(skip)]
    clicks: ClickTracker,
}

impl DatePickerState {
    /// Create state for `value` with no "today" reference.
    #[must_use]
    pub fn new(value: NaiveDateTime) -> Self {
        Self {
            interaction: InteractionState::new(),
            date: value.date(),
            time: value.time(),
            view: first_of_month(value.date()),
            today: None,
            part: DatePickerPart::Calendar,
            pending_digit: None,
            clicks: ClickTracker::new(),
        }
    }

    /// Create state for `value` with a caller-supplied "today" reference used
    /// for the today marker and the `t` shortcut.
    #[must_use]
    pub fn with_today(value: NaiveDateTime, today: NaiveDate) -> Self {
        let mut state = Self::new(value);
        state.today = Some(today);
        state
    }

    /// Capture native caller-owned state for an in-memory control rebuild.
    /// This is not a stable wire or durable storage contract. Multi-click
    /// history is not captured.
    #[must_use]
    pub fn snapshot(&self) -> serde_json::Value {
        crate::common::state_snapshot(self)
    }

    /// Restore a bounded native rebuild snapshot. Malformed snapshots return an
    /// error without changing this state.
    ///
    /// # Errors
    /// Returns an error for malformed, oversized, or incompatible snapshots.
    pub fn restore_snapshot(&mut self, snapshot: &serde_json::Value) -> Result<(), String> {
        let restored = crate::common::decode_state_snapshot(snapshot)?;
        *self = restored;
        Ok(())
    }

    /// Return the selected date and time.
    #[must_use]
    pub const fn value(&self) -> NaiveDateTime {
        NaiveDateTime::new(self.date, self.time)
    }

    /// Return the selected date.
    #[must_use]
    pub const fn date(&self) -> NaiveDate {
        self.date
    }

    /// Return the selected time.
    #[must_use]
    pub const fn time(&self) -> NaiveTime {
        self.time
    }

    /// Set the selected date and time, moving the month view to the new date.
    pub fn set_value(&mut self, value: NaiveDateTime) {
        self.set_date(value.date());
        self.set_time(value.time());
    }

    /// Set the selected date, moving the month view to it.
    pub fn set_date(&mut self, date: NaiveDate) {
        self.date = date;
        self.view = first_of_month(date);
    }

    /// Set the selected time.
    pub const fn set_time(&mut self, time: NaiveTime) {
        self.time = time;
        self.pending_digit = None;
    }

    /// Return the viewed month as `(year, month)` with `month` in `1..=12`.
    #[must_use]
    pub fn month_view(&self) -> (i32, u32) {
        (self.view.year(), self.view.month())
    }

    /// Show `month` (`1..=12`) of `year` without changing the selection.
    ///
    /// Returns `false` and leaves the view unchanged when the month is out of
    /// range for [`NaiveDate`].
    pub fn set_month_view(&mut self, year: i32, month: u32) -> bool {
        NaiveDate::from_ymd_opt(year, month, 1).is_some_and(|view| {
            self.view = view;
            true
        })
    }

    /// Return the caller-supplied "today" reference.
    #[must_use]
    pub const fn today(&self) -> Option<NaiveDate> {
        self.today
    }

    /// Set or clear the "today" reference.
    pub const fn set_today(&mut self, today: Option<NaiveDate>) {
        self.today = today;
    }

    /// Return the part that receives keyboard input.
    ///
    /// The picker normalizes this to a part its mode shows before handling
    /// input or painting.
    #[must_use]
    pub const fn part(&self) -> DatePickerPart {
        self.part
    }

    /// Move keyboard input to `part`, discarding any partially typed value.
    pub const fn set_part(&mut self, part: DatePickerPart) {
        self.part = part;
        self.pending_digit = None;
    }

    /// Set disabled state for the whole control.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
        if disabled {
            self.pending_digit = None;
        }
    }

    fn set_time_field(&mut self, part: DatePickerPart, value: u32) -> bool {
        let time = match part {
            DatePickerPart::Calendar => None,
            DatePickerPart::Hour => self.time.with_hour(value),
            DatePickerPart::Minute => self.time.with_minute(value),
            DatePickerPart::Second => self.time.with_second(value),
        };
        match time {
            Some(time) if time != self.time => {
                self.time = time;
                true
            }
            Some(_) | None => false,
        }
    }

    fn time_field(&self, part: DatePickerPart) -> u32 {
        match part {
            DatePickerPart::Calendar => 0,
            DatePickerPart::Hour => self.time.hour(),
            DatePickerPart::Minute => self.time.minute(),
            DatePickerPart::Second => self.time.second(),
        }
    }
}

/// Outcome from date picker input handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatePickerOutcome {
    /// Event was not handled.
    Ignored,
    /// Visual state (view month, focused part, pending input) changed without
    /// changing the value.
    Redraw,
    /// The value changed to the contained date and time.
    Changed(NaiveDateTime),
    /// The user submitted the contained value (Enter or double-click).
    Submitted(NaiveDateTime),
    /// The user cancelled (Escape).
    Cancelled,
}

/// Configurable date / date-time / time picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatePicker<'a> {
    mode: DatePickerMode,
    show_seconds: bool,
    week_start: WeekStart,
    month_names: &'a [&'a str; 12],
    weekday_names: &'a [&'a str; 7],
    policy: DatePickerPolicy,
    styles: DatePickerStyles,
}

impl<'a> DatePicker<'a> {
    /// Create a picker for `mode` with seconds hidden, Monday week start, and
    /// English labels.
    #[must_use]
    pub fn new(mode: DatePickerMode) -> Self {
        Self {
            mode,
            show_seconds: false,
            week_start: WeekStart::Monday,
            month_names: &ENGLISH_MONTH_NAMES,
            weekday_names: &ENGLISH_WEEKDAY_NAMES,
            policy: DatePickerPolicy::interactive(),
            styles: DatePickerStyles::default(),
        }
    }

    /// Show and edit seconds in the time row.
    #[must_use]
    pub const fn show_seconds(mut self, show_seconds: bool) -> Self {
        self.show_seconds = show_seconds;
        self
    }

    /// Set the first day of the week.
    #[must_use]
    pub const fn week_start(mut self, week_start: WeekStart) -> Self {
        self.week_start = week_start;
        self
    }

    /// Set month names, January first.
    #[must_use]
    pub const fn month_names(mut self, names: &'a [&'a str; 12]) -> Self {
        self.month_names = names;
        self
    }

    /// Set weekday names, Monday first. Each name is clipped/padded to two
    /// columns; the picker reorders them for [`WeekStart`].
    #[must_use]
    pub const fn weekday_names(mut self, names: &'a [&'a str; 7]) -> Self {
        self.weekday_names = names;
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: DatePickerPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: DatePickerStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return the picker mode.
    #[must_use]
    pub const fn mode(&self) -> DatePickerMode {
        self.mode
    }

    /// Return the keyboard-focusable parts in Tab order.
    #[must_use]
    pub const fn parts(&self) -> &'static [DatePickerPart] {
        use DatePickerPart::{Calendar, Hour, Minute, Second};
        match (self.mode, self.show_seconds) {
            (DatePickerMode::Date, _) => &[Calendar],
            (DatePickerMode::DateTime, false) => &[Calendar, Hour, Minute],
            (DatePickerMode::DateTime, true) => &[Calendar, Hour, Minute, Second],
            (DatePickerMode::Time, false) => &[Hour, Minute],
            (DatePickerMode::Time, true) => &[Hour, Minute, Second],
        }
    }

    /// Return the natural size of the picker, suitable for anchoring it as a
    /// popup with [`Rect::popup`].
    #[must_use]
    pub const fn preferred_size(&self) -> Size {
        let time_width = self.time_width();
        match self.mode {
            DatePickerMode::Date => Size::new(CALENDAR_WIDTH, CALENDAR_HEIGHT),
            // One blank row separates the calendar from the time row.
            DatePickerMode::DateTime => Size::new(CALENDAR_WIDTH, CALENDAR_HEIGHT + 2),
            DatePickerMode::Time => Size::new(time_width, 1),
        }
    }

    /// Return the part the picker treats as focused for `state`.
    #[must_use]
    pub fn effective_part(&self, state: &DatePickerState) -> DatePickerPart {
        let parts = self.parts();
        if parts.contains(&state.part) {
            state.part
        } else {
            parts[0]
        }
    }

    /// Paint the picker through a scoped local-coordinate context whose origin
    /// is this control's top-left corner.
    pub fn paint(
        &self,
        area: Rect,
        state: &DatePickerState,
        fallback: Style,
        cx: &mut PaintCx<'_, '_>,
    ) {
        if area.is_empty() {
            return;
        }
        cx.fill(LocalRect::terminal(area), " ", fallback);
        let geometry = self.geometry(area);
        if self.mode.has_date() {
            let row = Rect::new(area.x, area.y, geometry.content_width, 1);
            cx.write_line_with_fallback_style(
                LocalRect::terminal(row),
                &self.header_line(state, geometry.content_width),
                fallback,
            );
            let row = Rect::new(area.x, area.y.saturating_add(1), geometry.content_width, 1);
            cx.write_line_with_fallback_style(
                LocalRect::terminal(row),
                &self.weekday_line(state),
                fallback,
            );
            let dates = self.grid_dates(state.view);
            for week in 0..GRID_ROWS {
                let row = Rect::new(
                    area.x,
                    area.y.saturating_add(2).saturating_add(week),
                    geometry.content_width,
                    1,
                );
                cx.write_line_with_fallback_style(
                    LocalRect::terminal(row),
                    &self.week_line(state, &dates, usize::from(week)),
                    fallback,
                );
            }
        }
        if let Some(row) = geometry.time_row() {
            cx.write_line_with_fallback_style(
                LocalRect::terminal(row),
                &self.time_line(state, geometry),
                fallback,
            );
        }
    }

    /// Handle one input event using the current time for double-click
    /// detection.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut DatePickerState,
        event: &Event,
    ) -> DatePickerOutcome {
        self.handle_event_at(area, state, event, Instant::now())
    }

    /// Handle one input event, using `now` for double-click detection.
    pub fn handle_event_at(
        &self,
        area: Rect,
        state: &mut DatePickerState,
        event: &Event,
        now: Instant,
    ) -> DatePickerOutcome {
        let part = self.effective_part(state);
        if part != state.part {
            state.set_part(part);
        }
        if state.interaction.disabled {
            return DatePickerOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) => self.handle_key(state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse, now),
            Event::Focus(FocusEvent::Lost) => {
                state.pending_digit = None;
                state.clicks.reset();
                DatePickerOutcome::Ignored
            }
            Event::Resize(_) | Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                DatePickerOutcome::Ignored
            }
        }
    }

    // ---- keyboard -------------------------------------------------------

    fn handle_key(&self, state: &mut DatePickerState, stroke: KeyStroke) -> DatePickerOutcome {
        let pending = state.pending_digit.take();
        let plain = stroke.modifiers.is_empty();
        let shift_only = stroke.modifiers
            == Modifiers {
                shift: true,
                ..Modifiers::NONE
            };
        match stroke.key {
            KeyCode::Tab if self.policy.tab_cycles && plain => self.cycle_part(state, 1),
            KeyCode::Tab if self.policy.tab_cycles && shift_only => self.cycle_part(state, -1),
            KeyCode::Enter if self.policy.enter_submits && plain => {
                DatePickerOutcome::Submitted(state.value())
            }
            KeyCode::Escape if self.policy.escape_cancels && plain => DatePickerOutcome::Cancelled,
            _ if state.part == DatePickerPart::Calendar => {
                Self::handle_calendar_key(state, stroke, plain, shift_only)
            }
            _ if plain => self.handle_time_key(state, stroke.key, pending),
            _ => DatePickerOutcome::Ignored,
        }
    }

    fn handle_calendar_key(
        state: &mut DatePickerState,
        stroke: KeyStroke,
        plain: bool,
        shift_only: bool,
    ) -> DatePickerOutcome {
        let date = state.date;
        let target = match stroke.key {
            KeyCode::Left if plain => add_days(date, -1),
            KeyCode::Right if plain => add_days(date, 1),
            KeyCode::Up if plain => add_days(date, -7),
            KeyCode::Down if plain => add_days(date, 7),
            KeyCode::PageUp if plain => add_months(date, -1),
            KeyCode::PageDown if plain => add_months(date, 1),
            KeyCode::PageUp if shift_only => add_months(date, -12),
            KeyCode::PageDown if shift_only => add_months(date, 12),
            KeyCode::Home if plain => Some(first_of_month(date)),
            KeyCode::End if plain => Some(last_of_month(date)),
            KeyCode::Char('t') if plain => state.today,
            _ => return DatePickerOutcome::Ignored,
        };
        Self::commit_date(state, target)
    }

    fn handle_time_key(
        &self,
        state: &mut DatePickerState,
        key: KeyCode,
        pending: Option<u8>,
    ) -> DatePickerOutcome {
        let part = state.part;
        match key {
            KeyCode::Up => Self::step_time(state, part, 1),
            KeyCode::Down => Self::step_time(state, part, -1),
            KeyCode::Left => self.move_time_part(state, -1),
            KeyCode::Right => self.move_time_part(state, 1),
            KeyCode::Char(ch) => ch.to_digit(10).map_or(DatePickerOutcome::Ignored, |digit| {
                self.type_digit(state, part, digit, pending)
            }),
            _ => {
                if pending.is_some() {
                    DatePickerOutcome::Redraw
                } else {
                    DatePickerOutcome::Ignored
                }
            }
        }
    }

    fn type_digit(
        &self,
        state: &mut DatePickerState,
        part: DatePickerPart,
        digit: u32,
        pending: Option<u8>,
    ) -> DatePickerOutcome {
        let max = part.max_value();
        let (value, complete) = match pending {
            Some(first) if u32::from(first) * 10 + digit <= max => {
                (u32::from(first) * 10 + digit, true)
            }
            Some(_) | None => (digit, digit * 10 > max),
        };
        let changed = state.set_time_field(part, value);
        if complete {
            let _ = self.move_time_part(state, 1);
        } else {
            state.pending_digit = u8::try_from(digit).ok();
        }
        if changed {
            DatePickerOutcome::Changed(state.value())
        } else {
            DatePickerOutcome::Redraw
        }
    }

    fn step_time(
        state: &mut DatePickerState,
        part: DatePickerPart,
        delta: i32,
    ) -> DatePickerOutcome {
        let modulus = part.max_value() + 1;
        let current = state.time_field(part);
        let next = if delta >= 0 {
            (current + 1) % modulus
        } else {
            (current + modulus - 1) % modulus
        };
        if state.set_time_field(part, next) {
            DatePickerOutcome::Changed(state.value())
        } else {
            DatePickerOutcome::Ignored
        }
    }

    fn move_time_part(&self, state: &mut DatePickerState, delta: isize) -> DatePickerOutcome {
        let parts = self.parts();
        let Some(index) = parts.iter().position(|part| *part == state.part) else {
            return DatePickerOutcome::Ignored;
        };
        match index
            .checked_add_signed(delta)
            .and_then(|next| parts.get(next))
        {
            Some(next) if *next != DatePickerPart::Calendar => {
                state.set_part(*next);
                DatePickerOutcome::Redraw
            }
            Some(_) | None => DatePickerOutcome::Ignored,
        }
    }

    fn cycle_part(&self, state: &mut DatePickerState, delta: isize) -> DatePickerOutcome {
        let parts = self.parts();
        if parts.len() < 2 {
            return DatePickerOutcome::Ignored;
        }
        let index = parts
            .iter()
            .position(|part| *part == state.part)
            .unwrap_or(0);
        let next = match index.checked_add_signed(delta) {
            Some(next) if next < parts.len() => next,
            _ if !self.policy.tab_wraps => return DatePickerOutcome::Ignored,
            _ if delta < 0 => parts.len() - 1,
            _ => 0,
        };
        state.set_part(parts[next]);
        DatePickerOutcome::Redraw
    }

    fn commit_date(state: &mut DatePickerState, target: Option<NaiveDate>) -> DatePickerOutcome {
        let Some(target) = target else {
            return DatePickerOutcome::Ignored;
        };
        if target == state.date {
            let view = first_of_month(target);
            if state.view == view {
                DatePickerOutcome::Ignored
            } else {
                state.view = view;
                DatePickerOutcome::Redraw
            }
        } else {
            state.set_date(target);
            DatePickerOutcome::Changed(state.value())
        }
    }

    // ---- mouse ----------------------------------------------------------

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut DatePickerState,
        mouse: MouseEvent,
        now: Instant,
    ) -> DatePickerOutcome {
        if !self.policy.mouse.enabled {
            return DatePickerOutcome::Ignored;
        }
        let geometry = self.geometry(area);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if self.policy.mouse.click && area.contains(mouse.position) =>
            {
                self.handle_click(geometry, state, mouse.position, now)
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if self.policy.wheel => {
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    1
                } else {
                    -1
                };
                if let Some(part) = geometry.time_field_at(mouse.position) {
                    state.pending_digit = None;
                    Self::step_time(state, part, delta)
                } else if geometry.calendar().contains(mouse.position) {
                    Self::shift_view(state, -delta)
                } else {
                    DatePickerOutcome::Ignored
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => DatePickerOutcome::Ignored,
        }
    }

    fn handle_click(
        &self,
        geometry: Geometry,
        state: &mut DatePickerState,
        position: Point,
        now: Instant,
    ) -> DatePickerOutcome {
        let gained_focus = !state.interaction.focused || state.pending_digit.is_some();
        state.interaction.focused = true;
        state.pending_digit = None;
        let fallback = if gained_focus {
            DatePickerOutcome::Redraw
        } else {
            DatePickerOutcome::Ignored
        };
        if geometry.prev_arrow().contains(position) {
            state.clicks.reset();
            return or(Self::shift_view(state, -1), fallback);
        }
        if geometry.next_arrow().contains(position) {
            state.clicks.reset();
            return or(Self::shift_view(state, 1), fallback);
        }
        if let Some(index) = geometry.day_at(position) {
            let Some(date) = self.grid_dates(state.view)[index] else {
                return fallback;
            };
            let count =
                state
                    .clicks
                    .register(position.x, position.y, now, &ClickPolicy::with_max_count(2));
            let part_changed = state.part != DatePickerPart::Calendar;
            state.set_part(DatePickerPart::Calendar);
            if date == state.date {
                if count >= 2 && self.policy.double_click_submits {
                    state.clicks.reset();
                    return DatePickerOutcome::Submitted(state.value());
                }
                let view = first_of_month(date);
                if state.view != view || part_changed {
                    state.view = view;
                    return DatePickerOutcome::Redraw;
                }
                return fallback;
            }
            state.set_date(date);
            return DatePickerOutcome::Changed(state.value());
        }
        state.clicks.reset();
        if let Some(part) = geometry.time_field_at(position)
            && part != state.part
        {
            state.set_part(part);
            return DatePickerOutcome::Redraw;
        }
        fallback
    }

    const fn shift_view(state: &mut DatePickerState, delta: i32) -> DatePickerOutcome {
        match add_months(state.view, delta) {
            Some(view) => {
                state.view = view;
                DatePickerOutcome::Redraw
            }
            None => DatePickerOutcome::Ignored,
        }
    }

    // ---- rendering helpers ---------------------------------------------

    const fn time_width(&self) -> u16 {
        if self.show_seconds {
            TIME_FIELD_OFFSETS[2] + DAY_WIDTH
        } else {
            TIME_FIELD_OFFSETS[1] + DAY_WIDTH
        }
    }

    const fn geometry(&self, area: Rect) -> Geometry {
        let preferred = self.preferred_size();
        let content_width = if preferred.width < area.width {
            preferred.width
        } else {
            area.width
        };
        Geometry {
            area,
            content_width,
            mode: self.mode,
            time_width: self.time_width(),
            show_seconds: self.show_seconds,
        }
    }

    fn grid_dates(&self, view: NaiveDate) -> [Option<NaiveDate>; GRID_CELLS] {
        let offset = i64::from(self.week_start.column_of(view.weekday()));
        let mut dates = [None; GRID_CELLS];
        for (index, slot) in (0_i64..).zip(dates.iter_mut()) {
            *slot = add_days(view, index - offset);
        }
        dates
    }

    fn header_line(&self, state: &DatePickerState, width: u16) -> Line {
        let disabled = state.interaction.disabled;
        let arrow = if disabled {
            self.styles.disabled
        } else {
            self.styles.arrow
        };
        let header = if disabled {
            self.styles.disabled
        } else {
            self.styles.header
        };
        let month_index = usize::try_from(state.view.month0()).unwrap_or(0);
        let title = format!(
            "{} {}",
            self.month_names.get(month_index).copied().unwrap_or(""),
            state.view.year()
        );
        let available = usize::from(width.saturating_sub(4));
        let title = Line::from_spans(vec![Span::styled(title, header)]).truncate(available);
        let padding = available.saturating_sub(title.width());
        let left = padding / 2;
        let mut line = Line::from_spans(vec![
            Span::styled("‹", arrow),
            Span::raw(" ".repeat(left + 1)),
        ]);
        for span in title.spans {
            line.push_span(span);
        }
        line.push_span(Span::raw(" ".repeat(padding - left + 1)));
        line.push_span(Span::styled("›", arrow));
        line
    }

    fn weekday_line(&self, state: &DatePickerState) -> Line {
        let style = if state.interaction.disabled {
            self.styles.disabled
        } else {
            self.styles.weekday
        };
        let offset = match self.week_start {
            WeekStart::Monday => 0,
            WeekStart::Sunday => 6,
        };
        let mut line = Line::new();
        for column in 0..7 {
            if column > 0 {
                line.push_span(Span::raw(" "));
            }
            let name = self.weekday_names[(column + offset) % 7];
            let name = Line::from(name).truncate(usize::from(DAY_WIDTH));
            let pad = usize::from(DAY_WIDTH).saturating_sub(name.width());
            line.push_span(Span::styled(
                format!("{}{}", name.plain_text(), " ".repeat(pad)),
                style,
            ));
        }
        line
    }

    fn week_line(
        &self,
        state: &DatePickerState,
        dates: &[Option<NaiveDate>; GRID_CELLS],
        week: usize,
    ) -> Line {
        let calendar_focused =
            state.interaction.focused && self.effective_part(state) == DatePickerPart::Calendar;
        let mut line = Line::new();
        for column in 0..7 {
            if column > 0 {
                line.push_span(Span::raw(" "));
            }
            let Some(date) = dates[week * 7 + column] else {
                line.push_span(Span::raw("  "));
                continue;
            };
            let style = if state.interaction.disabled {
                self.styles.disabled
            } else {
                let mut style = if date.month() == state.view.month() {
                    self.styles.normal
                } else {
                    self.styles.outside
                };
                if state.today == Some(date) {
                    style = style.patch(self.styles.today);
                }
                if date == state.date {
                    style = style.patch(if calendar_focused {
                        self.styles.focused
                    } else {
                        self.styles.selected
                    });
                }
                style
            };
            line.push_span(Span::styled(format!("{:>2}", date.day()), style));
        }
        line
    }

    fn time_line(&self, state: &DatePickerState, geometry: Geometry) -> Line {
        let disabled = state.interaction.disabled;
        let focused_part = state
            .interaction
            .focused
            .then(|| self.effective_part(state));
        let separator = if disabled {
            self.styles.disabled
        } else {
            self.styles.separator
        };
        let mut line = Line::from_spans(vec![Span::raw(
            " ".repeat(usize::from(geometry.time_offset())),
        )]);
        let parts: &[DatePickerPart] = if self.show_seconds {
            &[
                DatePickerPart::Hour,
                DatePickerPart::Minute,
                DatePickerPart::Second,
            ]
        } else {
            &[DatePickerPart::Hour, DatePickerPart::Minute]
        };
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                line.push_span(Span::styled(TIME_SEPARATOR, separator));
            }
            let style = if disabled {
                self.styles.disabled
            } else if focused_part == Some(*part) {
                self.styles.normal.patch(self.styles.focused)
            } else {
                self.styles.normal
            };
            line.push_span(Span::styled(
                format!("{:02}", state.time_field(*part)),
                style,
            ));
        }
        line
    }
}

/// Resolved picker geometry for one area.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    area: Rect,
    content_width: u16,
    mode: DatePickerMode,
    time_width: u16,
    show_seconds: bool,
}

impl Geometry {
    const fn prev_arrow(self) -> Rect {
        if self.mode.has_date() {
            Rect::new(self.area.x, self.area.y, 2, 1)
        } else {
            Rect::new(0, 0, 0, 0)
        }
    }

    const fn next_arrow(self) -> Rect {
        if self.mode.has_date() {
            Rect::new(
                self.area
                    .x
                    .saturating_add(self.content_width.saturating_sub(2)),
                self.area.y,
                2,
                1,
            )
        } else {
            Rect::new(0, 0, 0, 0)
        }
    }

    const fn calendar(self) -> Rect {
        if self.mode.has_date() {
            Rect::new(
                self.area.x,
                self.area.y,
                self.content_width,
                CALENDAR_HEIGHT,
            )
        } else {
            Rect::new(0, 0, 0, 0)
        }
    }

    const fn grid(self) -> Rect {
        if self.mode.has_date() {
            Rect::new(
                self.area.x,
                self.area.y.saturating_add(2),
                self.content_width,
                GRID_ROWS,
            )
        } else {
            Rect::new(0, 0, 0, 0)
        }
    }

    /// Return the day cell rect for grid `index` (relative to the area).
    const fn day_cell(self, index: usize) -> Rect {
        #[allow(clippy::cast_possible_truncation)] // index < 42
        let (row, column) = ((index / 7) as u16, (index % 7) as u16);
        Rect::new(
            self.area.x.saturating_add(column * DAY_STRIDE),
            self.area.y.saturating_add(2).saturating_add(row),
            DAY_WIDTH,
            1,
        )
    }

    fn day_at(self, position: Point) -> Option<usize> {
        let grid = self.grid();
        if !grid.contains(position) {
            return None;
        }
        let column = ((position.x - grid.x) / DAY_STRIDE).min(6);
        let row = position.y - grid.y;
        Some(usize::from(row) * 7 + usize::from(column))
    }

    const fn time_offset(self) -> u16 {
        self.content_width.saturating_sub(self.time_width) / 2
    }

    const fn time_row(self) -> Option<Rect> {
        let y = match self.mode {
            DatePickerMode::Date => return None,
            DatePickerMode::DateTime => self.area.y.saturating_add(CALENDAR_HEIGHT + 1),
            DatePickerMode::Time => self.area.y,
        };
        Some(Rect::new(self.area.x, y, self.content_width, 1))
    }

    fn time_field_rect(self, part: DatePickerPart) -> Option<Rect> {
        let row = self.time_row()?;
        let index = part.time_index()?;
        if index == 2 && !self.show_seconds {
            return None;
        }
        Some(Rect::new(
            row.x
                .saturating_add(self.time_offset())
                .saturating_add(TIME_FIELD_OFFSETS[index]),
            row.y,
            DAY_WIDTH,
            1,
        ))
    }

    /// Return the time field nearest `position` on the time row; separators
    /// belong to the field on their left.
    fn time_field_at(self, position: Point) -> Option<DatePickerPart> {
        let row = self.time_row()?;
        let start = row.x.saturating_add(self.time_offset());
        if !row.contains(position)
            || position.x < start
            || position.x >= start.saturating_add(self.time_width)
        {
            return None;
        }
        let dx = position.x - start;
        Some(if dx < TIME_FIELD_OFFSETS[1] {
            DatePickerPart::Hour
        } else if !self.show_seconds || dx < TIME_FIELD_OFFSETS[2] {
            DatePickerPart::Minute
        } else {
            DatePickerPart::Second
        })
    }
}

const fn or(outcome: DatePickerOutcome, fallback: DatePickerOutcome) -> DatePickerOutcome {
    match outcome {
        DatePickerOutcome::Ignored => fallback,
        other => other,
    }
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    // Day 1 exists in every month representable by `NaiveDate`.
    date.with_day(1).unwrap_or(date)
}

fn last_of_month(date: NaiveDate) -> NaiveDate {
    (28..=31)
        .rev()
        .find_map(|day| date.with_day(day))
        .unwrap_or(date)
}

const fn add_days(date: NaiveDate, delta: i64) -> Option<NaiveDate> {
    if delta >= 0 {
        date.checked_add_days(Days::new(delta.unsigned_abs()))
    } else {
        date.checked_sub_days(Days::new(delta.unsigned_abs()))
    }
}

/// Add `delta` months, clamping the day to the target month's length.
const fn add_months(date: NaiveDate, delta: i32) -> Option<NaiveDate> {
    if delta >= 0 {
        date.checked_add_months(Months::new(delta.unsigned_abs()))
    } else {
        date.checked_sub_months(Months::new(delta.unsigned_abs()))
    }
}

/// Canonical component-lifecycle date picker.
///
/// The component measures [`DatePicker::preferred_size`], paints through the
/// scoped paint context, registers the picker as one focusable region plus
/// non-focusable regions for the month arrows, each day, and each time field,
/// and routes events through the same resolved layout. State remains
/// caller-owned through an interior-mutable `Cell`.
pub struct DatePickerComponent<'a, 'state> {
    pointer_intent: bmux_tui::pointer::PointerIntent,
    id: LayoutId,
    picker: DatePicker<'a>,
    state: &'state Cell<DatePickerState>,
    fallback: Style,
}

impl<'a, 'state> DatePickerComponent<'a, 'state> {
    /// Create a picker with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        picker: DatePicker<'a>,
        state: &'state Cell<DatePickerState>,
    ) -> Self {
        Self {
            pointer_intent: bmux_tui::pointer::PointerIntent::Auto,
            id: id.into(),
            picker,
            state,
            fallback: Style::new(),
        }
    }

    /// Override pointer presentation without changing input routing or mouse policy.
    #[must_use]
    pub const fn pointer_intent(mut self, intent: bmux_tui::pointer::PointerIntent) -> Self {
        self.pointer_intent = intent;
        self
    }

    /// Set the fill style applied beneath the picker.
    #[must_use]
    pub const fn fallback_style(mut self, fallback: Style) -> Self {
        self.fallback = fallback;
        self
    }

    fn region(&self, id: String, rect: Rect, role: HitRole, enabled: bool) -> SceneRegion {
        let mouse = self.picker.policy.mouse;
        SceneRegion::new(id, rect)
            .role(role)
            .pointer_intent(self.pointer_intent)
            .pointer_events(mouse.enabled)
            .hoverable(mouse.enabled && mouse.hover)
            .focusable(false)
            .enabled(enabled)
    }
}

impl Component for DatePickerComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.picker.mode.hash(&mut layout);
        self.picker.show_seconds.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.pointer_intent).hash(&mut paint);
        format!("{:?}", self.picker).hash(&mut paint);
        format!("{:?}", self.fallback).hash(&mut paint);
        format!("{:?}", self.state.get()).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let size = self.picker.preferred_size();
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(size.width.into(), size.height.into())),
        )
        .with_metadata(LayoutMetadata::new().semantic("date-picker"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let area = Rect::new(
            0,
            0,
            layout.size.width.try_into().unwrap_or(u16::MAX),
            layout.size.height.try_into().unwrap_or(u16::MAX),
        );
        if area.is_empty() {
            return;
        }
        let state = self.state.get();
        let enabled = !state.interaction.disabled;
        let id = self.id.as_str();
        cx.push_hit(
            self.region(id.to_string(), area, HitRole::ListItem, enabled)
                .focusable(true),
        );
        let geometry = self.picker.geometry(area);
        if self.picker.mode.has_date() {
            for (suffix, rect) in [
                ("prev", geometry.prev_arrow()),
                ("next", geometry.next_arrow()),
            ] {
                let rect = rect.intersection(area);
                if !rect.is_empty() {
                    let child = format!("{id}.{suffix}");
                    cx.push_hit(self.region(child.clone(), rect, HitRole::Action, enabled));
                    cx.push_semantic(SemanticRegion::new(child, rect, "button"));
                }
            }
            for (index, date) in self.picker.grid_dates(state.view).iter().enumerate() {
                let rect = geometry.day_cell(index).intersection(area);
                let Some(date) = date else { continue };
                if rect.is_empty() {
                    continue;
                }
                let child = format!("{id}.day.{date}");
                cx.push_hit(self.region(child.clone(), rect, HitRole::ListItem, enabled));
                cx.push_semantic(SemanticRegion::new(child, rect, "gridcell"));
            }
        }
        for part in self.picker.parts() {
            let Some(rect) = geometry.time_field_rect(*part) else {
                continue;
            };
            let rect = rect.intersection(area);
            if rect.is_empty() {
                continue;
            }
            let suffix = match part {
                DatePickerPart::Calendar => continue,
                DatePickerPart::Hour => "hour",
                DatePickerPart::Minute => "minute",
                DatePickerPart::Second => "second",
            };
            let child = format!("{id}.{suffix}");
            cx.push_hit(self.region(child.clone(), rect, HitRole::Action, enabled));
            cx.push_semantic(SemanticRegion::new(child, rect, "spinbutton"));
        }
        self.picker.paint(area, &state, self.fallback, cx);
        cx.push_semantic(SemanticRegion::new(id, area, "date-picker"));
        cx.push_damage(LocalRect::new(0, 0, area.width, area.height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let mut state = self.state.get();
        let outcome = self.picker.handle_event(area, &mut state, event);
        self.state.set(state);
        match outcome {
            DatePickerOutcome::Ignored => EventOutcome::Ignored,
            DatePickerOutcome::Redraw
            | DatePickerOutcome::Changed(_)
            | DatePickerOutcome::Submitted(_)
            | DatePickerOutcome::Cancelled => EventOutcome::Redraw,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;

    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn at(year: i32, month: u32, day: u32, h: u32, m: u32, s: u32) -> NaiveDateTime {
        date(year, month, day).and_hms_opt(h, m, s).unwrap()
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(code))
    }

    fn shift(code: KeyCode) -> Event {
        Event::Key(KeyStroke::with_modifiers(
            code,
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        ))
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
    }

    fn down(x: u16, y: u16) -> Event {
        mouse(MouseEventKind::Down(MouseButton::Left), x, y)
    }

    fn press(
        picker: &DatePicker<'_>,
        state: &mut DatePickerState,
        event: &Event,
    ) -> DatePickerOutcome {
        picker.handle_event(Rect::new(0, 0, 30, 12), state, event)
    }

    fn render(picker: DatePicker<'_>, state: DatePickerState) -> Vec<String> {
        let size = picker.preferred_size();
        let area = Rect::new(0, 0, size.width, size.height);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        let cell = Cell::new(state);
        let component = DatePickerComponent::new("picker", picker, &cell);
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        PaintCx::new(&mut frame).with_child(0, 0, LocalRect::terminal(area), |cx| {
            component.paint(&layout, cx);
        });
        (0..size.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect()
    }

    #[test]
    fn arrows_move_by_day_and_week_across_month_boundaries() {
        let picker = DatePicker::new(DatePickerMode::Date);
        let mut state = DatePickerState::new(at(2026, 1, 31, 0, 0, 0));
        assert_eq!(
            press(&picker, &mut state, &key(KeyCode::Right)),
            DatePickerOutcome::Changed(at(2026, 2, 1, 0, 0, 0))
        );
        assert_eq!(state.month_view(), (2026, 2));
        press(&picker, &mut state, &key(KeyCode::Left));
        assert_eq!(state.date(), date(2026, 1, 31));
        press(&picker, &mut state, &key(KeyCode::Down));
        assert_eq!(state.date(), date(2026, 2, 7));
        press(&picker, &mut state, &key(KeyCode::Up));
        press(&picker, &mut state, &key(KeyCode::Up));
        assert_eq!(state.date(), date(2026, 1, 24));
    }

    #[test]
    fn page_keys_move_months_and_years_clamping_leap_days() {
        let picker = DatePicker::new(DatePickerMode::Date);
        let mut state = DatePickerState::new(at(2024, 1, 31, 0, 0, 0));
        press(&picker, &mut state, &key(KeyCode::PageDown));
        assert_eq!(state.date(), date(2024, 2, 29));
        press(&picker, &mut state, &key(KeyCode::PageUp));
        assert_eq!(state.date(), date(2024, 1, 29));

        state.set_date(date(2024, 2, 29));
        press(&picker, &mut state, &shift(KeyCode::PageDown));
        assert_eq!(state.date(), date(2025, 2, 28));
        press(&picker, &mut state, &shift(KeyCode::PageUp));
        assert_eq!(state.date(), date(2024, 2, 28));
        assert_eq!(state.month_view(), (2024, 2));
    }

    #[test]
    fn home_end_and_today_shortcuts() {
        let picker = DatePicker::new(DatePickerMode::Date);
        let mut state = DatePickerState::with_today(at(2024, 2, 10, 0, 0, 0), date(2026, 11, 4));
        press(&picker, &mut state, &key(KeyCode::End));
        assert_eq!(state.date(), date(2024, 2, 29));
        press(&picker, &mut state, &key(KeyCode::Home));
        assert_eq!(state.date(), date(2024, 2, 1));
        assert_eq!(
            press(&picker, &mut state, &key(KeyCode::Home)),
            DatePickerOutcome::Ignored
        );
        press(&picker, &mut state, &key(KeyCode::Char('t')));
        assert_eq!(state.date(), date(2026, 11, 4));

        let mut no_today = DatePickerState::new(at(2024, 2, 10, 0, 0, 0));
        assert_eq!(
            press(&picker, &mut no_today, &key(KeyCode::Char('t'))),
            DatePickerOutcome::Ignored
        );
    }

    #[test]
    fn navigation_stops_at_representable_bounds() {
        let picker = DatePicker::new(DatePickerMode::Date);
        let mut state = DatePickerState::new(NaiveDate::MAX.and_hms_opt(0, 0, 0).unwrap());
        assert_eq!(
            press(&picker, &mut state, &key(KeyCode::Right)),
            DatePickerOutcome::Ignored
        );
        assert_eq!(state.date(), NaiveDate::MAX);
        // The partial grid still renders without panicking.
        let _ = render(picker, state);
    }

    #[test]
    fn enter_submits_and_escape_cancels() {
        let picker = DatePicker::new(DatePickerMode::DateTime);
        let mut state = DatePickerState::new(at(2026, 11, 4, 9, 30, 0));
        assert_eq!(
            press(&picker, &mut state, &key(KeyCode::Enter)),
            DatePickerOutcome::Submitted(at(2026, 11, 4, 9, 30, 0))
        );
        assert_eq!(
            press(&picker, &mut state, &key(KeyCode::Escape)),
            DatePickerOutcome::Cancelled
        );
        let quiet = picker.policy(DatePickerPolicy {
            enter_submits: false,
            escape_cancels: false,
            ..DatePickerPolicy::interactive()
        });
        assert_eq!(
            press(&quiet, &mut state, &key(KeyCode::Enter)),
            DatePickerOutcome::Ignored
        );
        assert_eq!(
            press(&quiet, &mut state, &key(KeyCode::Escape)),
            DatePickerOutcome::Ignored
        );
    }

    #[test]
    fn tab_cycles_parts_by_mode() {
        let picker = DatePicker::new(DatePickerMode::DateTime).show_seconds(true);
        let mut state = DatePickerState::new(at(2026, 11, 4, 9, 30, 0));
        for expected in [
            DatePickerPart::Hour,
            DatePickerPart::Minute,
            DatePickerPart::Second,
            DatePickerPart::Calendar,
        ] {
            assert_eq!(
                press(&picker, &mut state, &key(KeyCode::Tab)),
                DatePickerOutcome::Redraw
            );
            assert_eq!(state.part(), expected);
        }
        press(&picker, &mut state, &shift(KeyCode::Tab));
        assert_eq!(state.part(), DatePickerPart::Second);

        let date_only = DatePicker::new(DatePickerMode::Date);
        assert_eq!(
            press(&date_only, &mut state, &key(KeyCode::Tab)),
            DatePickerOutcome::Ignored
        );
        assert_eq!(state.part(), DatePickerPart::Calendar);

        let no_wrap = DatePicker::new(DatePickerMode::Time).policy(DatePickerPolicy {
            tab_wraps: false,
            ..DatePickerPolicy::interactive()
        });
        let mut state = DatePickerState::new(at(2026, 11, 4, 9, 30, 0));
        assert_eq!(
            press(&no_wrap, &mut state, &shift(KeyCode::Tab)),
            DatePickerOutcome::Ignored
        );
        assert_eq!(state.part(), DatePickerPart::Hour);
        press(&no_wrap, &mut state, &key(KeyCode::Tab));
        assert_eq!(
            press(&no_wrap, &mut state, &key(KeyCode::Tab)),
            DatePickerOutcome::Ignored
        );
        assert_eq!(state.part(), DatePickerPart::Minute);
    }

    #[test]
    fn time_fields_wrap_without_changing_date() {
        let picker = DatePicker::new(DatePickerMode::DateTime).show_seconds(true);
        let mut state = DatePickerState::new(at(2026, 12, 31, 23, 59, 59));
        state.set_part(DatePickerPart::Hour);
        assert_eq!(
            press(&picker, &mut state, &key(KeyCode::Up)),
            DatePickerOutcome::Changed(at(2026, 12, 31, 0, 59, 59))
        );
        press(&picker, &mut state, &key(KeyCode::Down));
        assert_eq!(state.time().hour(), 23);
        state.set_part(DatePickerPart::Minute);
        press(&picker, &mut state, &key(KeyCode::Up));
        assert_eq!(state.value(), at(2026, 12, 31, 23, 0, 59));
        state.set_part(DatePickerPart::Second);
        press(&picker, &mut state, &key(KeyCode::Up));
        assert_eq!(state.value(), at(2026, 12, 31, 23, 0, 0));
        press(&picker, &mut state, &key(KeyCode::Down));
        assert_eq!(state.value(), at(2026, 12, 31, 23, 0, 59));
    }

    #[test]
    fn digits_type_two_digit_values_and_advance() {
        let picker = DatePicker::new(DatePickerMode::Time).show_seconds(true);
        let mut state = DatePickerState::new(at(2026, 1, 1, 0, 0, 0));
        press(&picker, &mut state, &key(KeyCode::Char('1')));
        assert_eq!(state.time().hour(), 1);
        assert_eq!(state.part(), DatePickerPart::Hour);
        press(&picker, &mut state, &key(KeyCode::Char('7')));
        assert_eq!(state.time().hour(), 17);
        assert_eq!(state.part(), DatePickerPart::Minute);
        // A leading digit that cannot start a two-digit value completes at once.
        press(&picker, &mut state, &key(KeyCode::Char('7')));
        assert_eq!(state.time().minute(), 7);
        assert_eq!(state.part(), DatePickerPart::Second);
        // An out-of-range second digit restarts entry.
        press(&picker, &mut state, &key(KeyCode::Char('5')));
        press(&picker, &mut state, &key(KeyCode::Char('9')));
        assert_eq!(state.time().second(), 59);
        // Hour: "2" then "5" is out of range, so "5" becomes a new value.
        state.set_part(DatePickerPart::Hour);
        press(&picker, &mut state, &key(KeyCode::Char('2')));
        press(&picker, &mut state, &key(KeyCode::Char('5')));
        assert_eq!(state.time().hour(), 5);
        assert_eq!(state.part(), DatePickerPart::Minute);
        // A non-digit key discards a pending first digit.
        press(&picker, &mut state, &key(KeyCode::Char('1')));
        press(&picker, &mut state, &key(KeyCode::Right));
        press(&picker, &mut state, &key(KeyCode::Left));
        press(&picker, &mut state, &key(KeyCode::Char('2')));
        assert_eq!(state.time().minute(), 2);
    }

    #[test]
    fn mode_normalizes_part() {
        let picker = DatePicker::new(DatePickerMode::Time);
        let mut state = DatePickerState::new(at(2026, 1, 1, 10, 0, 0));
        assert_eq!(picker.effective_part(&state), DatePickerPart::Hour);
        press(&picker, &mut state, &key(KeyCode::Up));
        assert_eq!(state.time().hour(), 11);
        state.set_part(DatePickerPart::Second);
        press(&picker, &mut state, &key(KeyCode::Up));
        assert_eq!(state.part(), DatePickerPart::Hour);
        assert_eq!(state.time().hour(), 12);
    }

    #[test]
    fn renders_monday_and_sunday_week_start() {
        let state = DatePickerState::with_today(at(2026, 11, 4, 14, 30, 5), date(2026, 11, 4));
        assert_eq!(
            render(DatePicker::new(DatePickerMode::Date), state),
            [
                "‹  November 2026   ›",
                "Mo Tu We Th Fr Sa Su",
                "26 27 28 29 30 31  1",
                " 2  3  4  5  6  7  8",
                " 9 10 11 12 13 14 15",
                "16 17 18 19 20 21 22",
                "23 24 25 26 27 28 29",
                "30  1  2  3  4  5  6",
            ]
        );
        let sunday = DatePicker::new(DatePickerMode::DateTime)
            .week_start(WeekStart::Sunday)
            .show_seconds(true);
        assert_eq!(
            render(sunday, state),
            [
                "‹  November 2026   ›",
                "Su Mo Tu We Th Fr Sa",
                " 1  2  3  4  5  6  7",
                " 8  9 10 11 12 13 14",
                "15 16 17 18 19 20 21",
                "22 23 24 25 26 27 28",
                "29 30  1  2  3  4  5",
                " 6  7  8  9 10 11 12",
                "                    ",
                "    14 : 30 : 05    ",
            ]
        );
        assert_eq!(
            render(DatePicker::new(DatePickerMode::Time), state),
            ["14 : 30"]
        );
    }

    #[test]
    fn styles_mark_selected_today_outside_and_focused_field() {
        let mut state = DatePickerState::with_today(at(2026, 11, 4, 14, 30, 5), date(2026, 11, 5));
        state.interaction.focused = true;
        state.set_part(DatePickerPart::Minute);
        let picker = DatePicker::new(DatePickerMode::DateTime);
        let size = picker.preferred_size();
        let area = Rect::new(0, 0, size.width, size.height);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        picker.paint(area, &state, Style::new(), &mut PaintCx::new(&mut frame));
        let styles = DatePickerStyles::default();
        let style_at = |x, y| buffer.get(Point::new(x, y)).unwrap().style;
        // Nov 4 at row 3 col 2; selected (calendar not focused).
        assert_eq!(style_at(7, 3), styles.normal.patch(styles.selected));
        // Nov 5 is today.
        assert_eq!(style_at(10, 3), styles.normal.patch(styles.today));
        // Oct 26 is outside the month.
        assert_eq!(style_at(0, 2), styles.outside);
        // Minute field focused: time row "   14 : 30   " offset 6, minute at 11.
        assert_eq!(style_at(11, 9), styles.normal.patch(styles.focused));
        assert_eq!(style_at(6, 9), styles.normal);
    }

    #[test]
    fn mouse_selects_days_changes_month_and_double_click_submits() {
        let picker = DatePicker::new(DatePickerMode::DateTime);
        let mut state = DatePickerState::new(at(2026, 11, 4, 9, 0, 0));
        let area = Rect::new(10, 5, 20, 10);
        let now = Instant::now();
        // Nov 12 at grid row 2 (y = 5 + 4), column 3 (x = 10 + 9).
        assert_eq!(
            picker.handle_event_at(area, &mut state, &down(19, 9), now),
            DatePickerOutcome::Changed(at(2026, 11, 12, 9, 0, 0))
        );
        assert!(state.interaction.focused);
        assert_eq!(
            picker.handle_event_at(
                area,
                &mut state,
                &down(19, 9),
                now + Duration::from_millis(100)
            ),
            DatePickerOutcome::Submitted(at(2026, 11, 12, 9, 0, 0))
        );
        // A slow second click does not submit.
        picker.handle_event_at(area, &mut state, &down(19, 9), now + Duration::from_secs(5));
        assert_eq!(
            picker.handle_event_at(
                area,
                &mut state,
                &down(19, 9),
                now + Duration::from_secs(10)
            ),
            DatePickerOutcome::Ignored
        );
        // Outside-month day in the first row selects October 26.
        picker.handle_event_at(area, &mut state, &down(10, 7), now);
        assert_eq!(state.date(), date(2026, 10, 26));
        assert_eq!(state.month_view(), (2026, 10));
        // Month arrows change only the view.
        assert_eq!(
            picker.handle_event_at(area, &mut state, &down(29, 5), now),
            DatePickerOutcome::Redraw
        );
        assert_eq!(state.month_view(), (2026, 11));
        picker.handle_event_at(area, &mut state, &down(10, 5), now);
        picker.handle_event_at(area, &mut state, &down(10, 5), now);
        assert_eq!(state.month_view(), (2026, 9));
        assert_eq!(state.date(), date(2026, 10, 26));
        // Clicking the minute field focuses it.
        picker.handle_event_at(area, &mut state, &down(22, 14), now);
        assert_eq!(state.part(), DatePickerPart::Minute);
    }

    #[test]
    fn wheel_changes_month_over_calendar_and_time_over_fields() {
        let picker = DatePicker::new(DatePickerMode::DateTime);
        let mut state = DatePickerState::new(at(2026, 11, 4, 9, 0, 0));
        let area = Rect::new(0, 0, 20, 10);
        assert_eq!(
            picker.handle_event(area, &mut state, &mouse(MouseEventKind::ScrollDown, 5, 4)),
            DatePickerOutcome::Redraw
        );
        assert_eq!(state.month_view(), (2026, 12));
        picker.handle_event(area, &mut state, &mouse(MouseEventKind::ScrollUp, 5, 0));
        picker.handle_event(area, &mut state, &mouse(MouseEventKind::ScrollUp, 5, 0));
        assert_eq!(state.month_view(), (2026, 10));
        // Time row "   09 : 00" starts at x = 6; hour at 6..8, minute at 11..13.
        assert_eq!(
            picker.handle_event(area, &mut state, &mouse(MouseEventKind::ScrollDown, 6, 9)),
            DatePickerOutcome::Changed(at(2026, 11, 4, 8, 0, 0))
        );
        picker.handle_event(area, &mut state, &mouse(MouseEventKind::ScrollUp, 12, 9));
        assert_eq!(state.time().minute(), 1);
        assert_eq!(
            picker.handle_event(area, &mut state, &mouse(MouseEventKind::ScrollUp, 0, 8)),
            DatePickerOutcome::Ignored
        );
    }

    #[test]
    fn disabled_picker_ignores_input_and_dims() {
        let picker = DatePicker::new(DatePickerMode::Date);
        let mut state = DatePickerState::new(at(2026, 11, 4, 0, 0, 0));
        state.set_disabled(true);
        assert_eq!(
            press(&picker, &mut state, &key(KeyCode::Right)),
            DatePickerOutcome::Ignored
        );
        assert_eq!(
            press(&picker, &mut state, &down(3, 3)),
            DatePickerOutcome::Ignored
        );
        assert_eq!(state.date(), date(2026, 11, 4));
    }

    #[test]
    fn set_month_view_validates_range() {
        let mut state = DatePickerState::new(at(2026, 11, 4, 0, 0, 0));
        assert!(state.set_month_view(2027, 2));
        assert_eq!(state.month_view(), (2027, 2));
        assert!(!state.set_month_view(2027, 13));
        assert_eq!(state.month_view(), (2027, 2));
        assert_eq!(state.date(), date(2026, 11, 4));
    }

    #[test]
    fn snapshot_round_trips() {
        let mut state = DatePickerState::with_today(at(2026, 11, 4, 1, 2, 3), date(2026, 11, 5));
        state.set_part(DatePickerPart::Minute);
        let snapshot = state.snapshot();
        let mut restored = DatePickerState::new(at(2000, 1, 1, 0, 0, 0));
        restored.restore_snapshot(&snapshot).unwrap();
        assert_eq!(restored, state);
    }

    #[test]
    fn component_registers_regions_and_routes_events() {
        let state = Cell::new(DatePickerState::new(at(2026, 11, 4, 9, 0, 0)));
        let component =
            DatePickerComponent::new("when", DatePicker::new(DatePickerMode::DateTime), &state);
        let area = Rect::new(3, 2, 20, 10);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 14));
        let mut frame = Frame::new(&mut buffer);
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(20, 10));
        PaintCx::new(&mut frame).with_child(3, 2, LocalRect::new(0, 0, 20, 10), |cx| {
            component.paint(&layout, cx);
        });
        let regions = frame.hits().regions();
        assert_eq!(regions[0].id.as_str(), "when");
        assert_eq!(regions[0].area, area);
        assert_eq!(frame.hits().focus_targets(None).len(), 1);
        let day = regions
            .iter()
            .find(|region| region.id.as_str() == "when.day.2026-11-04")
            .unwrap();
        assert_eq!(day.area, Rect::new(9, 5, 2, 1));
        assert!(
            regions
                .iter()
                .any(|region| region.id.as_str() == "when.next")
        );
        assert!(
            regions
                .iter()
                .any(|region| region.id.as_str() == "when.minute")
        );

        assert_eq!(
            component.event(&key(KeyCode::Right), &layout, &mut EventCx::new(&layout)),
            EventOutcome::Redraw
        );
        assert_eq!(state.get().date(), date(2026, 11, 5));
        let revision = component.revision();
        state.set({
            let mut next = state.get();
            next.set_part(DatePickerPart::Hour);
            next
        });
        assert_eq!(revision.layout, component.revision().layout);
        assert_ne!(revision.paint, component.revision().paint);
    }

    #[test]
    fn theme_styles_resolve_surface() {
        let theme = crate::theme::ComponentTheme::opaque_dark();
        let styles = theme.date_picker_styles();
        assert_eq!(
            styles.focused,
            theme
                .for_surface(crate::theme::ComponentSurfaceDepth::Normal)
                .focused
        );
        let overlay =
            DatePickerStyles::from_theme(theme, crate::theme::ComponentSurfaceDepth::Overlay);
        assert_eq!(
            overlay.selected,
            theme
                .for_surface(crate::theme::ComponentSurfaceDepth::Overlay)
                .selected
        );
    }
}
