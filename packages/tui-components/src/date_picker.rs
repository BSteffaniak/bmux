//! Calendar date, date-time, and time picker control.
//!
//! [`DatePicker`] is an immediate-mode control over caller-owned
//! [`DatePickerState`]. The compact layout ([`DatePickerPolicy::interactive`])
//! renders a month header with `‹`/`›` navigation, a weekday header, a six-week
//! day grid, and (for time modes) an `HH : MM : SS` row:
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
//! The full layout ([`DatePickerPolicy::full`]) adds a text field that parses
//! as you type (ISO dates and times, optional UTC offsets, and `now`, `today`,
//! `yesterday` and `tomorrow`) and keeps the calendar and time in step with it,
//! a readable rendering of the value, year arrows, a month grid (click the
//! title or press `m`), ▲ / ▼ buttons on each time field, and Now, Today,
//! Cancel and OK buttons:
//!
//! ```text
//!  2026-11-04 14:30:05
//! We 4 Nov 2026, 14:30:05
//!
//! «  ‹     November 2026      ›  »
//!       Mo Tu We Th Fr Sa Su
//!       ...
//!
//!       ▲    ▲    ▲
//! Time  14 : 30 : 05  [ Now ]
//!       ▼    ▼    ▼
//!
//! [ Today ]        [ Cancel ] [ OK ]
//! ```
//!
//! The caller supplies "today" and "now" ([`DatePickerState::set_now`]); the
//! control never reads the system clock, so the host decides which time zone
//! relative words belong to.
//!
//! Use [`DatePicker::preferred_size`] with
//! [`Rect::popup`](bmux_tui::geometry::Rect::popup) to anchor the picker as a
//! popup, or [`DatePickerComponent`] for the canonical component lifecycle.

#[path = "date_picker/parse.rs"]
mod parse;

use std::cell::RefCell;
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
use chrono::{
    Datelike, Days, FixedOffset, Months, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday,
};
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::common::{ComponentMousePolicy, InteractionState};

use parse::{OffsetChange, ParseContext, ParseError};

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
/// Width of the full layout.
const FULL_WIDTH: u16 = 32;
/// Rows of the month grid (three months per row).
const MONTH_ROWS: u16 = 4;
/// Horizontal distance between the starts of adjacent month cells.
const MONTH_STRIDE: u16 = 7;
/// Width of one month cell.
const MONTH_WIDTH: u16 = 6;
/// Column of the hour field in the full layout's time row (after the
/// `Time` label).
const FULL_TIME_X: u16 = 6;
/// Columns between the calendar and the time beside it.
const SIDE_GAP: u16 = 6;
/// Width of the time block beside the calendar (`[ Now ] [ Today ]`).
const SIDE_BLOCK: u16 = 17;
/// Width from which the full date-time picker puts the time beside the
/// calendar.
pub const SIDE_BY_SIDE_WIDTH: u16 = CALENDAR_WIDTH + SIDE_GAP + SIDE_BLOCK;
/// Column offsets of the time fields in the full layout's `HH:MM:SS`.
const TIGHT_TIME_OFFSETS: [u16; 3] = [0, 3, 6];
/// Label before the time fields in the full layout.
const TIME_LABEL: &str = "Time";
const NOW_LABEL: &str = "[ Now ]";
const TODAY_LABEL: &str = "[ Today ]";
const CANCEL_LABEL: &str = "[ Cancel ]";
const OK_LABEL: &str = "[ OK ]";

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
    /// The text field ([`DatePickerPolicy::text_input`]).
    Text,
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
            Self::Text | Self::Calendar => None,
            Self::Hour => Some(0),
            Self::Minute => Some(1),
            Self::Second => Some(2),
        }
    }

    const fn max_value(self) -> u32 {
        match self {
            Self::Text | Self::Calendar => 0,
            Self::Hour => 23,
            Self::Minute | Self::Second => 59,
        }
    }

    const fn is_time(self) -> bool {
        self.time_index().is_some()
    }
}

/// Visual styles for a date picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatePickerStyles {
    /// Day numbers in the viewed month and time values.
    pub normal: Style,
    /// Month and year title.
    pub header: Style,
    /// `‹` / `›` month navigation arrows (and `«` / `»`, ▲ / ▼).
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
    /// Overlay for the day, month or button under the pointer.
    pub hovered: Style,
    /// The text field.
    pub field: Style,
    /// The readable value and hints under the text field, and the time label.
    pub hint: Style,
    /// Why the typed text is not a value.
    pub error: Style,
    /// Now, Today, Cancel and OK buttons.
    pub button: Style,
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
            hovered: Style::new().add_modifier(Modifier::UNDERLINE),
            field: Style::new().add_modifier(Modifier::UNDERLINE),
            hint: Style::new().add_modifier(Modifier::DIM),
            error: Style::new().add_modifier(Modifier::BOLD),
            button: Style::new().add_modifier(Modifier::BOLD),
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
            arrow: theme.accent,
            weekday: theme.muted,
            outside: theme.muted,
            today: theme
                .info
                .patch(Style::new().add_modifier(Modifier::UNDERLINE)),
            selected: theme.selected,
            focused: theme.focused,
            separator: theme.muted,
            disabled: theme.disabled,
            hovered: theme.selected,
            field: theme
                .text
                .patch(Style::new().add_modifier(Modifier::UNDERLINE)),
            hint: theme.muted,
            error: theme.error,
            button: theme
                .accent
                .patch(Style::new().add_modifier(Modifier::BOLD)),
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
    /// Mouse behavior. `click` gates day/arrow/field/button clicks and
    /// `hover` the pointer highlight.
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
    /// Whether Tab / Shift+Tab move focus between the picker's parts.
    pub tab_cycles: bool,
    /// Whether Tab past the last part wraps to the first (and Shift+Tab past
    /// the first wraps to the last). When `false` the key is ignored at the
    /// edges so the caller can move focus out of the picker.
    pub tab_wraps: bool,
    /// Show a text field above the calendar that parses as you type and keeps
    /// the calendar and time in step with it.
    pub text_input: bool,
    /// Accept a UTC offset after the time (`+02:00`, `-0530`, `Z`, `UTC`) in
    /// the text field; see [`DatePickerState::offset`].
    pub offsets: bool,
    /// Show `«` / `»` year arrows beside the month arrows.
    pub year_arrows: bool,
    /// Clicking the month title (or `m` in the calendar) shows a month grid.
    pub month_grid: bool,
    /// Show ▲ / ▼ buttons on the time fields and a Now button.
    pub time_buttons: bool,
    /// Show a Today button under the calendar.
    pub today_button: bool,
    /// Show Cancel and OK buttons (reported as
    /// [`DatePickerOutcome::Cancelled`] / [`DatePickerOutcome::Submitted`]).
    pub footer: bool,
}

impl DatePickerPolicy {
    /// Compact interactive picker: the calendar and time row only.
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
            text_input: false,
            offsets: false,
            year_arrows: false,
            month_grid: false,
            time_buttons: false,
            today_button: false,
            footer: false,
        }
    }

    /// Full picker: text entry, year arrows, month grid, time buttons, and
    /// Today, Now, Cancel and OK buttons. Offsets stay off; enable
    /// [`Self::offsets`] for zone-aware values.
    #[must_use]
    pub const fn full() -> Self {
        Self {
            text_input: true,
            year_arrows: true,
            month_grid: true,
            time_buttons: true,
            today_button: true,
            footer: true,
            ..Self::interactive()
        }
    }

    /// Whether any part of the full layout is enabled (it needs the wider
    /// layout).
    const fn wide(&self) -> bool {
        self.text_input || self.year_arrows || self.time_buttons || self.today_button || self.footer
    }
}

impl Default for DatePickerPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// A clickable element of the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Target {
    PrevYear,
    PrevMonth,
    Title,
    NextMonth,
    NextYear,
    Day(u8),
    Month(u8),
    Up(DatePickerPart),
    Down(DatePickerPart),
    Field(DatePickerPart),
    Now,
    Today,
    Cancel,
    Ok,
    Text,
}

/// Runtime date picker state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Text typed into the field; `None` shows the value's canonical text.
    text: Option<String>,
    /// Cursor position in `text`, in characters.
    cursor: usize,
    /// The next typed character replaces the whole text (after focusing it).
    replace_on_type: bool,
    /// Why `text` is not a value, when it cannot become one.
    text_error: Option<String>,
    /// `text` could still become a value by typing more.
    text_incomplete: bool,
    /// UTC offset of the value, in seconds east of UTC.
    offset: Option<i32>,
    now: Option<NaiveDateTime>,
    now_offset: Option<i32>,
    /// The month grid is showing in place of the day grid.
    months: bool,
    hover: Option<Target>,
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
            text: None,
            cursor: 0,
            replace_on_type: false,
            text_error: None,
            text_incomplete: false,
            offset: None,
            now: None,
            now_offset: None,
            months: false,
            hover: None,
        }
    }

    /// Create state for `value` with a caller-supplied "today" reference used
    /// for the today marker, the `t` shortcut, the Today button and date words.
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
    /// Typed text is replaced by the value's text.
    pub fn set_value(&mut self, value: NaiveDateTime) {
        self.set_date(value.date());
        self.set_time(value.time());
    }

    /// Set the selected date, moving the month view to it. Typed text is
    /// replaced by the value's text.
    pub fn set_date(&mut self, date: NaiveDate) {
        self.date = date;
        self.view = first_of_month(date);
        self.months = false;
        self.sync_text();
    }

    /// Set the selected time. Typed text is replaced by the value's text.
    pub fn set_time(&mut self, time: NaiveTime) {
        self.time = time;
        self.pending_digit = None;
        self.sync_text();
    }

    /// Return the UTC offset of the value, when one was typed, given by
    /// [`Self::set_offset`], or taken from `now` (with
    /// [`DatePickerPolicy::offsets`]).
    #[must_use]
    pub fn offset(&self) -> Option<FixedOffset> {
        self.offset.and_then(FixedOffset::east_opt)
    }

    /// Set the value's UTC offset. Typed text is replaced by the value's text.
    pub fn set_offset(&mut self, offset: Option<FixedOffset>) {
        self.offset = offset.map(|offset| offset.local_minus_utc());
        self.sync_text();
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

    /// Return the caller-supplied current time, if set.
    #[must_use]
    pub const fn now(&self) -> Option<NaiveDateTime> {
        self.now
    }

    /// Set the current wall-clock time and the UTC offset it is in. `now`
    /// and the Now button use them, and `today` becomes `now`'s date. Hosts
    /// choose the zone (a database session's, the user's), so relative words
    /// agree with the values being edited. Update it as time passes; the
    /// picker never reads the system clock.
    pub fn set_now(&mut self, now: NaiveDateTime, offset: Option<FixedOffset>) {
        self.now = Some(now);
        self.now_offset = offset.map(|offset| offset.local_minus_utc());
        self.today = Some(now.date());
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

    /// Return the text typed into the field, if it differs from the value's
    /// own text (the field shows [`DatePicker::text`]).
    #[must_use]
    pub fn typed_text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Return why the typed text is not a value, when it cannot become one by
    /// typing more.
    #[must_use]
    pub fn text_error(&self) -> Option<&str> {
        self.text_error.as_deref()
    }

    /// Return whether typed text is not (yet) a value. While it is, the value
    /// is the last one the text or the calendar produced, and the picker does
    /// not submit.
    #[must_use]
    pub const fn text_pending(&self) -> bool {
        self.text.is_some() && (self.text_error.is_some() || self.text_incomplete)
    }

    /// Return whether the month grid is showing in place of the day grid.
    #[must_use]
    pub const fn showing_months(&self) -> bool {
        self.months
    }

    /// Let the field show the value's text again.
    fn sync_text(&mut self) {
        self.text = None;
        self.text_error = None;
        self.text_incomplete = false;
        self.replace_on_type = false;
    }

    fn set_time_field(&mut self, part: DatePickerPart, value: u32) -> bool {
        let time = match part {
            DatePickerPart::Text | DatePickerPart::Calendar => None,
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
            DatePickerPart::Text | DatePickerPart::Calendar => 0,
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
    /// Visual state (view month, focused part, typed text, hover) changed
    /// without changing the value.
    Redraw,
    /// The value changed to the contained date and time.
    Changed(NaiveDateTime),
    /// The user submitted the contained value (Enter, double-click or OK).
    Submitted(NaiveDateTime),
    /// The user cancelled (Escape or Cancel).
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
        use DatePickerPart::{Calendar, Hour, Minute, Second, Text};
        match (self.mode, self.show_seconds, self.policy.text_input) {
            (DatePickerMode::Date, _, false) => &[Calendar],
            (DatePickerMode::Date, _, true) => &[Text, Calendar],
            (DatePickerMode::DateTime, false, false) => &[Calendar, Hour, Minute],
            (DatePickerMode::DateTime, true, false) => &[Calendar, Hour, Minute, Second],
            (DatePickerMode::DateTime, false, true) => &[Text, Calendar, Hour, Minute],
            (DatePickerMode::DateTime, true, true) => &[Text, Calendar, Hour, Minute, Second],
            (DatePickerMode::Time, false, false) => &[Hour, Minute],
            (DatePickerMode::Time, true, false) => &[Hour, Minute, Second],
            (DatePickerMode::Time, false, true) => &[Text, Hour, Minute],
            (DatePickerMode::Time, true, true) => &[Text, Hour, Minute, Second],
        }
    }

    /// Return the natural size of the picker, suitable for anchoring it as a
    /// popup with [`Rect::popup`].
    #[must_use]
    pub const fn preferred_size(&self) -> Size {
        let layout = self.layout_for(FULL_WIDTH);
        Size::new(layout.width, layout.height)
    }

    /// Return the size the picker takes in `width` columns: the full layout
    /// fills the width (its text field spans it) and, from
    /// [`SIDE_BY_SIDE_WIDTH`], puts the time beside the calendar. The compact
    /// layout keeps its natural size.
    #[must_use]
    pub const fn size_for_width(&self, width: u16) -> Size {
        let layout = self.layout_for(width);
        Size::new(layout.width, layout.height)
    }

    /// Return the part the picker treats as focused for `state`.
    #[must_use]
    pub fn effective_part(&self, state: &DatePickerState) -> DatePickerPart {
        let parts = self.parts();
        if parts.contains(&state.part) {
            state.part
        } else if parts.contains(&DatePickerPart::Calendar) {
            DatePickerPart::Calendar
        } else {
            parts
                .iter()
                .copied()
                .find(|part| part.is_time())
                .unwrap_or(parts[0])
        }
    }

    /// Return the text the field shows: the typed text, or the value's own
    /// text (`2026-11-04 14:30:05`, with the offset when there is one).
    #[must_use]
    pub fn text(&self, state: &DatePickerState) -> String {
        state
            .text
            .clone()
            .unwrap_or_else(|| self.canonical_text(state))
    }

    /// Put `text` in the field as if typed, and follow the value it
    /// describes. Hosts use it to start from text the calendar cannot show
    /// (the field then explains why and [`DatePickerState::text_pending`] is
    /// true).
    pub fn set_text(&self, state: &mut DatePickerState, text: &str) -> DatePickerOutcome {
        state.text = Some(text.to_string());
        state.cursor = text.chars().count();
        state.replace_on_type = false;
        self.apply_text(state)
    }

    /// Return the value's own text, as the field shows it when nothing is
    /// typed.
    #[must_use]
    pub fn canonical_text(&self, state: &DatePickerState) -> String {
        parse::format(state.value(), state.offset(), self.mode, self.show_seconds)
    }

    /// Key hints for the focused part, as `(keys, action)` pairs.
    #[must_use]
    pub fn hints(&self, state: &DatePickerState) -> Vec<(&'static str, &'static str)> {
        let mut hints = Vec::new();
        match self.effective_part(state) {
            DatePickerPart::Text => {
                hints.push(("type", "a date, time, now, today"));
            }
            DatePickerPart::Calendar if state.months => {
                hints.push(("←→↑↓", "month"));
                hints.push(("Enter", "choose"));
            }
            DatePickerPart::Calendar => {
                hints.push(("←→↑↓", "day"));
                hints.push(("PgUp/PgDn", "month"));
                if self.policy.month_grid {
                    hints.push(("m", "months"));
                }
                if state.today.is_some() {
                    hints.push(("t", "today"));
                }
            }
            DatePickerPart::Hour | DatePickerPart::Minute | DatePickerPart::Second => {
                hints.push(("↑↓", "change"));
                hints.push(("←→", "field"));
                if state.now.is_some() {
                    hints.push(("n", "now"));
                }
            }
        }
        hints
    }

    // ---- painting -------------------------------------------------------

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
        let mut write = |rect: Rect, line: &Line| {
            let rect = rect.intersection(area);
            if !rect.is_empty() {
                cx.write_line_with_fallback_style(LocalRect::terminal(rect), line, fallback);
            }
        };
        if let Some(row) = geometry.text_row() {
            write(row, &self.text_line(state, row.width));
            // Under the field's text, which starts one column in.
            let status = row.shifted(1);
            write(
                Rect::new(status.x + 1, status.y, status.width.saturating_sub(1), 1),
                &self.status_line(state, status.width.saturating_sub(1)),
            );
        }
        if self.mode.has_date() {
            let header = geometry.header_row();
            write(header, &self.header_line(state, geometry));
            if state.months {
                for row in 0..MONTH_ROWS {
                    write(geometry.grid().nth_row(row), &self.month_line(state, row));
                }
            } else {
                write(header.shifted(1), &self.weekday_line(state, geometry));
                let dates = self.grid_dates(state.view);
                for week in 0..GRID_ROWS {
                    write(
                        header.shifted(2).nth_row(week),
                        &self.week_line(state, &dates, usize::from(week), geometry),
                    );
                }
            }
        }
        if let Some(row) = geometry.time_row() {
            // From the time's own column: beside the calendar it shares rows.
            let block = geometry.time_block(row);
            write(block, &self.time_line(state, geometry));
            if geometry.layout.time_buttons {
                write(block.shifted(-1), &self.spin_line(state, geometry, true));
                write(block.shifted(1), &self.spin_line(state, geometry, false));
            }
            if let Some(buttons) = geometry.layout.now_row {
                let y = geometry.area.y.saturating_add(buttons);
                write(
                    Rect::new(block.x, y, block.width, 1),
                    &self.buttons_line(state, geometry),
                );
            }
            if let Some(label) = geometry.time_label() {
                write(
                    label,
                    &Line::from_spans(vec![Span::styled(
                        TIME_LABEL,
                        self.enabled(state, self.styles.hint),
                    )]),
                );
            }
        }
        if let Some(row) = geometry.footer_row() {
            write(row, &self.footer_line(state, geometry));
        }
    }

    // ---- events ---------------------------------------------------------

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
            Event::Paste(text) if state.part == DatePickerPart::Text => {
                let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
                self.edit_text(state, |buffer, cursor| {
                    insert(buffer, cursor, &text);
                })
            }
            Event::Focus(FocusEvent::Lost) => {
                state.pending_digit = None;
                state.clicks.reset();
                if state.hover.take().is_some() {
                    DatePickerOutcome::Redraw
                } else {
                    DatePickerOutcome::Ignored
                }
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
            KeyCode::Enter | KeyCode::Escape
                if plain && state.months && state.part == DatePickerPart::Calendar =>
            {
                state.months = false;
                DatePickerOutcome::Redraw
            }
            KeyCode::Enter if self.policy.enter_submits && plain => self.submit(state),
            KeyCode::Escape if self.policy.escape_cancels && plain => DatePickerOutcome::Cancelled,
            _ if state.part == DatePickerPart::Text => self.handle_text_key(state, stroke),
            _ if state.part == DatePickerPart::Calendar && state.months => {
                Self::handle_month_key(state, stroke, plain, shift_only)
            }
            _ if state.part == DatePickerPart::Calendar => {
                self.handle_calendar_key(state, stroke, plain, shift_only)
            }
            _ if plain => self.handle_time_key(state, stroke.key, pending),
            _ => DatePickerOutcome::Ignored,
        }
    }

    /// Submit unless typed text is not a value yet.
    const fn submit(&self, state: &mut DatePickerState) -> DatePickerOutcome {
        if state.text_pending() {
            // Show the field so the reason is visible.
            if self.policy.text_input {
                state.set_part(DatePickerPart::Text);
            }
            return DatePickerOutcome::Redraw;
        }
        DatePickerOutcome::Submitted(state.value())
    }

    fn handle_calendar_key(
        &self,
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
            KeyCode::Char('m') if plain && self.policy.month_grid => {
                state.months = true;
                return DatePickerOutcome::Redraw;
            }
            // Typing a date starts a new entry in the text field.
            KeyCode::Char(ch)
                if (plain || shift_only) && self.policy.text_input && is_entry_char(ch) =>
            {
                state.set_part(DatePickerPart::Text);
                state.replace_on_type = true;
                return self.handle_text_key(state, stroke);
            }
            _ => return DatePickerOutcome::Ignored,
        };
        Self::commit_date(state, target)
    }

    /// Keys while the month grid shows: arrows move the highlighted month
    /// (the viewed month), page keys change the year.
    const fn handle_month_key(
        state: &mut DatePickerState,
        stroke: KeyStroke,
        plain: bool,
        shift_only: bool,
    ) -> DatePickerOutcome {
        let delta = match stroke.key {
            KeyCode::Left if plain => -1,
            KeyCode::Right if plain => 1,
            KeyCode::Up if plain => -3,
            KeyCode::Down if plain => 3,
            KeyCode::PageUp if plain || shift_only => -12,
            KeyCode::PageDown if plain || shift_only => 12,
            KeyCode::Char('m') if plain => {
                state.months = false;
                return DatePickerOutcome::Redraw;
            }
            _ => return DatePickerOutcome::Ignored,
        };
        Self::shift_view(state, delta)
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
            KeyCode::Char('n') => self.now(state),
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

    /// Text field editing. Returns `Ignored` for keys it does not use.
    fn handle_text_key(&self, state: &mut DatePickerState, stroke: KeyStroke) -> DatePickerOutcome {
        let modifiers = stroke.modifiers;
        let ctrl = modifiers
            == Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            };
        let typing = !modifiers.ctrl && !modifiers.alt && !modifiers.super_key;
        match stroke.key {
            KeyCode::Char(ch) if typing => {
                let mut text = [0; 4];
                let text = ch.encode_utf8(&mut text);
                self.edit_text(state, |buffer, cursor| insert(buffer, cursor, text))
            }
            KeyCode::Char('u') if ctrl => self.edit_text(state, |buffer, cursor| {
                buffer.clear();
                *cursor = 0;
            }),
            KeyCode::Char('k') if ctrl => self.edit_text(state, |buffer, cursor| {
                let end = byte_index(buffer, *cursor);
                buffer.truncate(end);
            }),
            KeyCode::Char('w') if ctrl => self.edit_text(state, |buffer, cursor| {
                let chars: Vec<char> = buffer.chars().collect();
                let mut start = *cursor;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && chars[start - 1] != ' ' {
                    start -= 1;
                }
                let range = byte_index(buffer, start)..byte_index(buffer, *cursor);
                buffer.replace_range(range, "");
                *cursor = start;
            }),
            KeyCode::Backspace if !modifiers.ctrl && !modifiers.alt => {
                self.edit_text(state, |buffer, cursor| {
                    if *cursor > 0 {
                        let start = byte_index(buffer, *cursor - 1);
                        let end = byte_index(buffer, *cursor);
                        buffer.replace_range(start..end, "");
                        *cursor -= 1;
                    }
                })
            }
            KeyCode::Delete if modifiers.is_empty() => self.edit_text(state, |buffer, cursor| {
                if *cursor < buffer.chars().count() {
                    let start = byte_index(buffer, *cursor);
                    let end = byte_index(buffer, *cursor + 1);
                    buffer.replace_range(start..end, "");
                }
            }),
            KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End
                if modifiers.is_empty() =>
            {
                self.move_cursor(state, stroke.key)
            }
            KeyCode::Char('a') if ctrl => self.move_cursor(state, KeyCode::Home),
            KeyCode::Char('e') if ctrl => self.move_cursor(state, KeyCode::End),
            _ => DatePickerOutcome::Ignored,
        }
    }

    fn move_cursor(&self, state: &mut DatePickerState, key: KeyCode) -> DatePickerOutcome {
        let text = self.text(state);
        let len = text.chars().count();
        if state.text.is_none() {
            // Editing starts from the value's own text.
            state.cursor = len;
            state.text = Some(text);
        }
        state.replace_on_type = false;
        state.cursor = match key {
            KeyCode::Left => state.cursor.saturating_sub(1),
            KeyCode::Right => (state.cursor + 1).min(len),
            KeyCode::Home => 0,
            _ => len,
        };
        DatePickerOutcome::Redraw
    }

    /// Apply `edit` to the field's text (taking over the value's text first),
    /// then parse it and follow the value it describes.
    fn edit_text(
        &self,
        state: &mut DatePickerState,
        edit: impl FnOnce(&mut String, &mut usize),
    ) -> DatePickerOutcome {
        let mut buffer = if state.replace_on_type {
            String::new()
        } else {
            self.text(state)
        };
        let mut cursor = if state.replace_on_type {
            0
        } else if state.text.is_none() {
            buffer.chars().count()
        } else {
            state.cursor.min(buffer.chars().count())
        };
        state.replace_on_type = false;
        edit(&mut buffer, &mut cursor);
        state.cursor = cursor;
        state.text = Some(buffer);
        self.apply_text(state)
    }

    /// Parse the typed text and follow the value it describes.
    fn apply_text(&self, state: &mut DatePickerState) -> DatePickerOutcome {
        let text = state.text.clone().unwrap_or_default();
        let context = ParseContext {
            mode: self.mode,
            now: state.now,
            today: state.today,
            now_offset: state.now_offset.and_then(FixedOffset::east_opt),
            offsets: self.policy.offsets,
        };
        match parse::parse(&text, &context) {
            Ok(parsed) => {
                state.text_error = None;
                state.text_incomplete = false;
                let before = (state.value(), state.offset);
                if let Some(date) = parsed.date {
                    state.date = date;
                    state.view = first_of_month(date);
                    state.months = false;
                }
                if let Some(time) = parsed.time {
                    state.time = time;
                }
                if let OffsetChange::Set(offset) = parsed.offset {
                    state.offset = offset.map(|offset| offset.local_minus_utc());
                }
                if (state.value(), state.offset) == before {
                    DatePickerOutcome::Redraw
                } else {
                    DatePickerOutcome::Changed(state.value())
                }
            }
            Err(ParseError::Incomplete) => {
                state.text_error = None;
                state.text_incomplete = true;
                DatePickerOutcome::Redraw
            }
            Err(ParseError::Invalid(message)) => {
                state.text_error = Some(message);
                state.text_incomplete = false;
                DatePickerOutcome::Redraw
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
            state.sync_text();
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
            state.sync_text();
            DatePickerOutcome::Changed(state.value())
        } else {
            DatePickerOutcome::Ignored
        }
    }

    /// Set the value to the caller-supplied current time (and its offset,
    /// with [`DatePickerPolicy::offsets`]).
    fn now(&self, state: &mut DatePickerState) -> DatePickerOutcome {
        let Some(now) = state.now else {
            return DatePickerOutcome::Ignored;
        };
        let before = (state.value(), state.offset);
        if self.mode.has_date() {
            state.date = now.date();
            state.view = first_of_month(now.date());
            state.months = false;
        }
        if self.mode.has_time() {
            state.time = now.time();
        }
        if self.policy.offsets {
            state.offset = state.now_offset;
        }
        state.sync_text();
        if (state.value(), state.offset) == before {
            DatePickerOutcome::Redraw
        } else {
            DatePickerOutcome::Changed(state.value())
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
            Some(next) if next.is_time() => {
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
        self.focus(state, parts[next]);
        DatePickerOutcome::Redraw
    }

    /// Move keyboard input to `part` as Tab does: leaving the field shows
    /// the value's own text when the typed text was a value, and entering it
    /// selects its text so typing replaces it. Parts the picker does not show
    /// are ignored.
    pub fn focus_part(&self, state: &mut DatePickerState, part: DatePickerPart) {
        if self.parts().contains(&part) {
            self.focus(state, part);
        }
    }

    /// Move keyboard input to `part`. Leaving the field shows the value's own
    /// text when the typed text was a value; entering it selects its text so
    /// typing replaces it.
    fn focus(&self, state: &mut DatePickerState, part: DatePickerPart) {
        if state.part == DatePickerPart::Text
            && part != DatePickerPart::Text
            && !state.text_pending()
        {
            state.sync_text();
        }
        state.set_part(part);
        state.months = state.months && part == DatePickerPart::Calendar;
        if part == DatePickerPart::Text {
            state.cursor = self.text(state).chars().count();
            state.replace_on_type = true;
        }
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
            MouseEventKind::Move | MouseEventKind::Drag(_) if self.policy.mouse.hover => {
                let hover = self.target_at(geometry, state, mouse.position);
                if hover == state.hover {
                    DatePickerOutcome::Ignored
                } else {
                    state.hover = hover;
                    DatePickerOutcome::Redraw
                }
            }
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
                    Self::shift_view(state, if state.months { -delta * 12 } else { -delta })
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

    #[allow(clippy::too_many_lines)] // One arm per clickable element.
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
        let target = self.target_at(geometry, state, position);
        if !matches!(target, Some(Target::Day(_))) {
            state.clicks.reset();
        }
        // In the month grid the month arrows step years.
        let step = if state.months { 12 } else { 1 };
        let outcome = match target {
            None => return fallback,
            Some(Target::PrevMonth) => Self::shift_view(state, -step),
            Some(Target::NextMonth) => Self::shift_view(state, step),
            Some(Target::PrevYear) => Self::shift_view(state, -12),
            Some(Target::NextYear) => Self::shift_view(state, 12),
            Some(Target::Title) => {
                state.months = !state.months;
                self.focus(state, DatePickerPart::Calendar);
                DatePickerOutcome::Redraw
            }
            Some(Target::Month(month)) => {
                let year = state.view.year();
                state.set_month_view(year, u32::from(month) + 1);
                state.months = false;
                self.focus(state, DatePickerPart::Calendar);
                DatePickerOutcome::Redraw
            }
            Some(Target::Day(index)) => {
                return self.click_day(state, usize::from(index), position, now, fallback);
            }
            Some(Target::Field(part)) => {
                if part == state.part {
                    return fallback;
                }
                self.focus(state, part);
                DatePickerOutcome::Redraw
            }
            Some(Target::Up(part)) => {
                self.focus(state, part);
                or(Self::step_time(state, part, 1), DatePickerOutcome::Redraw)
            }
            Some(Target::Down(part)) => {
                self.focus(state, part);
                or(Self::step_time(state, part, -1), DatePickerOutcome::Redraw)
            }
            Some(Target::Now) => self.now(state),
            Some(Target::Today) => Self::commit_date(state, state.today),
            Some(Target::Cancel) => DatePickerOutcome::Cancelled,
            Some(Target::Ok) => self.submit(state),
            Some(Target::Text) => {
                if state.part != DatePickerPart::Text {
                    self.focus(state, DatePickerPart::Text);
                }
                let text = self.text(state);
                let len = text.chars().count();
                let start = geometry.text_scroll(state.cursor, len);
                // Editing starts from the value's own text.
                state.text = Some(text);
                let column = usize::from(position.x.saturating_sub(geometry.text_x()));
                state.cursor = (start + column).min(len);
                state.replace_on_type = false;
                DatePickerOutcome::Redraw
            }
        };
        or(outcome, fallback)
    }

    fn click_day(
        &self,
        state: &mut DatePickerState,
        index: usize,
        position: Point,
        now: Instant,
        fallback: DatePickerOutcome,
    ) -> DatePickerOutcome {
        let Some(date) = self.grid_dates(state.view)[index] else {
            return fallback;
        };
        let count =
            state
                .clicks
                .register(position.x, position.y, now, &ClickPolicy::with_max_count(2));
        let part_changed = state.part != DatePickerPart::Calendar;
        self.focus(state, DatePickerPart::Calendar);
        if date == state.date {
            if count >= 2 && self.policy.double_click_submits {
                state.clicks.reset();
                return self.submit(state);
            }
            let view = first_of_month(date);
            if state.view != view || part_changed {
                state.view = view;
                return DatePickerOutcome::Redraw;
            }
            return fallback;
        }
        state.set_date(date);
        DatePickerOutcome::Changed(state.value())
    }

    /// The clickable element at `position`.
    fn target_at(
        &self,
        geometry: Geometry,
        state: &DatePickerState,
        position: Point,
    ) -> Option<Target> {
        if let Some(index) = (self.mode.has_date() && !state.months)
            .then(|| geometry.day_at(position))
            .flatten()
        {
            return u8::try_from(index).ok().map(Target::Day);
        }
        if let Some(part) = geometry.time_field_at(position) {
            return Some(Target::Field(part));
        }
        self.targets(geometry, state)
            .into_iter()
            .find(|(_, rect)| rect.contains(position))
            .map(|(target, _)| target)
    }

    /// Every clickable element besides days and time fields, with its
    /// rectangle.
    fn targets(&self, geometry: Geometry, state: &DatePickerState) -> Vec<(Target, Rect)> {
        let mut targets = Vec::new();
        if self.mode.has_date() {
            targets.push((Target::PrevMonth, geometry.prev_month()));
            targets.push((Target::NextMonth, geometry.next_month()));
            if geometry.layout.year_arrows {
                targets.push((Target::PrevYear, geometry.prev_year()));
                targets.push((Target::NextYear, geometry.next_year()));
            }
            if self.policy.month_grid {
                targets.push((Target::Title, geometry.title()));
            }
            if state.months {
                for month in 0..12 {
                    targets.push((
                        Target::Month(month),
                        geometry.month_cell(usize::from(month)),
                    ));
                }
            }
        }
        if geometry.layout.time_buttons {
            for part in self.parts().iter().filter(|part| part.is_time()) {
                if let Some(field) = geometry.time_field_rect(*part) {
                    targets.push((Target::Up(*part), field.shifted(-1)));
                    targets.push((Target::Down(*part), field.shifted(1)));
                }
            }
            if let Some(rect) = geometry.now_button() {
                targets.push((Target::Now, rect));
            }
        }
        if let Some(row) = geometry.text_row() {
            targets.push((Target::Text, row));
        }
        if let Some(rect) = geometry.today_button() {
            targets.push((Target::Today, rect));
        }
        if let Some((cancel, ok)) = geometry.footer_buttons() {
            targets.push((Target::Cancel, cancel));
            targets.push((Target::Ok, ok));
        }
        targets
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

    const fn time_width(&self, tight: bool) -> u16 {
        let offsets = if tight {
            TIGHT_TIME_OFFSETS
        } else {
            TIME_FIELD_OFFSETS
        };
        if self.show_seconds {
            offsets[2] + DAY_WIDTH
        } else {
            offsets[1] + DAY_WIDTH
        }
    }

    /// Row and column offsets of each section in `width` columns, from the
    /// policy and mode.
    #[allow(clippy::too_many_lines)] // One branch per arrangement.
    const fn layout_for(&self, width: u16) -> Layout {
        let has_date = self.mode.has_date();
        let has_time = self.mode.has_time();
        if !self.policy.wide() {
            return Layout {
                width: if has_date {
                    CALENDAR_WIDTH
                } else {
                    self.time_width(false)
                },
                height: match self.mode {
                    DatePickerMode::Date => CALENDAR_HEIGHT,
                    // One blank row separates the calendar from the time row.
                    DatePickerMode::DateTime => CALENDAR_HEIGHT + 2,
                    DatePickerMode::Time => 1,
                },
                text: None,
                calendar: if has_date { Some(0) } else { None },
                calendar_x: 0,
                time: match self.mode {
                    DatePickerMode::Date => None,
                    DatePickerMode::DateTime => Some(CALENDAR_HEIGHT + 1),
                    DatePickerMode::Time => Some(0),
                },
                time_x: None,
                time_label: None,
                tight_time: false,
                time_buttons: false,
                year_arrows: false,
                now_row: None,
                footer: None,
                today: TodayPlace::None,
                buttons: false,
            };
        }
        let width = if width > FULL_WIDTH {
            width
        } else {
            FULL_WIDTH
        };
        let side = has_date && has_time && width >= SIDE_BY_SIDE_WIDTH;
        let spin = self.policy.time_buttons;
        let mut y = 0;
        let text = if self.policy.text_input {
            // Field, readable value, blank.
            y += 3;
            Some(0)
        } else {
            None
        };
        // Left-aligned with the field once there is room; centered under it in
        // the narrow layout.
        let calendar_x = if width >= SIDE_BY_SIDE_WIDTH {
            0
        } else {
            (width - CALENDAR_WIDTH) / 2
        };
        let calendar = if has_date { Some(y) } else { None };
        // The time and its buttons: beside the calendar, or under it.
        let (time, time_x, time_label, now_row) = if !has_time {
            (None, None, None, None)
        } else if side {
            let x = CALENDAR_WIDTH + SIDE_GAP;
            let value = if spin { y + 2 } else { y + 1 };
            let buttons = if spin { Some(y + 5) } else { None };
            (Some(value), Some(x), Some((x, y)), buttons)
        } else {
            if has_date {
                y += CALENDAR_HEIGHT + 1;
            }
            let x = if calendar_x > FULL_TIME_X {
                calendar_x
            } else {
                FULL_TIME_X
            };
            let value = if spin { y + 1 } else { y };
            let buttons = if spin { Some(y + 3) } else { None };
            y += if spin { 4 } else { 1 };
            (
                Some(value),
                Some(x),
                Some((x - FULL_TIME_X, value)),
                buttons,
            )
        };
        if side || (has_date && !has_time) {
            y += CALENDAR_HEIGHT;
        }
        let today = if !(self.policy.today_button && has_date) {
            TodayPlace::None
        } else if now_row.is_some() {
            TodayPlace::Buttons
        } else {
            TodayPlace::Footer
        };
        let footer = if self.policy.footer || matches!(today, TodayPlace::Footer) {
            y += 2;
            Some(y - 1)
        } else {
            None
        };
        Layout {
            width,
            height: if y == 0 { 1 } else { y },
            text,
            calendar,
            calendar_x,
            time,
            time_x,
            time_label,
            tight_time: true,
            time_buttons: spin,
            year_arrows: self.policy.year_arrows,
            now_row,
            footer,
            today,
            buttons: self.policy.footer,
        }
    }

    const fn geometry(&self, area: Rect) -> Geometry {
        let layout = self.layout_for(area.width);
        let content_width = if layout.width < area.width {
            layout.width
        } else {
            area.width
        };
        Geometry {
            area,
            layout,
            content_width,
            time_width: self.time_width(layout.tight_time),
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

    /// `style`, with the hover overlay when `target` is under the pointer.
    fn hovered(&self, state: &DatePickerState, target: Target, style: Style) -> Style {
        if state.hover == Some(target) && !state.interaction.disabled {
            style.patch(self.styles.hovered)
        } else {
            style
        }
    }

    const fn enabled(&self, state: &DatePickerState, style: Style) -> Style {
        if state.interaction.disabled {
            self.styles.disabled
        } else {
            style
        }
    }

    fn text_line(&self, state: &DatePickerState, width: u16) -> Line {
        let focused =
            state.interaction.focused && self.effective_part(state) == DatePickerPart::Text;
        let field = self.enabled(state, self.styles.field);
        let text = self.text(state);
        let chars: Vec<char> = text.chars().collect();
        let inner = usize::from(width.saturating_sub(2));
        let mut line = Line::from_spans(vec![Span::styled(" ", field)]);
        if chars.is_empty() && !focused {
            let placeholder = match self.mode {
                DatePickerMode::Date => "YYYY-MM-DD, today",
                DatePickerMode::DateTime => "YYYY-MM-DD HH:MM, now",
                DatePickerMode::Time => "HH:MM, now",
            };
            let placeholder = Line::from(placeholder).truncate(inner);
            let pad = inner.saturating_sub(placeholder.width());
            line.push_span(Span::styled(
                format!("{}{}", placeholder.plain_text(), " ".repeat(pad)),
                field.patch(self.styles.hint),
            ));
        } else {
            let start = text_scroll(state.cursor, chars.len(), inner, state.text.is_some());
            let cursor = if state.text.is_none() {
                chars.len()
            } else {
                state.cursor.min(chars.len())
            };
            for index in start..start + inner {
                let ch = chars.get(index).copied().unwrap_or(' ');
                let selected = focused && state.replace_on_type && index < chars.len();
                let style = if (focused && index == cursor && !state.replace_on_type) || selected {
                    field.patch(self.styles.focused)
                } else {
                    field
                };
                line.push_span(Span::styled(ch.to_string(), style));
            }
        }
        line.push_span(Span::styled(" ", field));
        line
    }

    fn status_line(&self, state: &DatePickerState, width: u16) -> Line {
        let pending = |message: String| (message, self.styles.hint);
        let (text, style) = match (&state.text, &state.text_error) {
            (Some(_), Some(error)) => (format!("\u{2717} {error}"), self.styles.error),
            (Some(_), None) if state.text_incomplete => pending(
                match self.mode {
                    DatePickerMode::Date => "keep typing: YYYY-MM-DD",
                    DatePickerMode::DateTime => "keep typing: YYYY-MM-DD HH:MM",
                    DatePickerMode::Time => "keep typing: HH:MM",
                }
                .to_string(),
            ),
            _ => pending(parse::describe(
                state.value(),
                state.offset(),
                self.mode,
                self.show_seconds,
                self.month_names,
                self.weekday_names,
            )),
        };
        Line::from_spans(vec![Span::styled(text, self.enabled(state, style))])
            .truncate(usize::from(width))
    }

    fn header_line(&self, state: &DatePickerState, geometry: Geometry) -> Line {
        let arrow = self.enabled(state, self.styles.arrow);
        let header = self.enabled(state, self.styles.header);
        let month_index = usize::try_from(state.view.month0()).unwrap_or(0);
        let title = if state.months {
            state.view.year().to_string()
        } else {
            format!(
                "{} {}",
                self.month_names.get(month_index).copied().unwrap_or(""),
                state.view.year()
            )
        };
        let title_rect = geometry.title();
        let available = usize::from(title_rect.width.saturating_sub(2));
        let title = Line::from_spans(vec![Span::styled(
            title,
            self.hovered(state, Target::Title, header),
        )])
        .truncate(available);
        let padding = available.saturating_sub(title.width());
        let left = padding / 2;
        let mut line = if geometry.layout.year_arrows {
            // `«‹` and `›»` at the grid's edges.
            Line::from_spans(vec![
                Span::raw(" ".repeat(usize::from(geometry.layout.calendar_x))),
                Span::styled("«", self.hovered(state, Target::PrevYear, arrow)),
            ])
        } else {
            Line::from_spans(vec![Span::raw(" ".repeat(usize::from(
                geometry.prev_month().x.saturating_sub(geometry.area.x),
            )))])
        };
        line.push_span(Span::styled(
            "‹",
            self.hovered(state, Target::PrevMonth, arrow),
        ));
        line.push_span(Span::raw(" ".repeat(left + 1)));
        for span in title.spans {
            line.push_span(span);
        }
        line.push_span(Span::raw(" ".repeat(padding - left + 1)));
        line.push_span(Span::styled(
            "›",
            self.hovered(state, Target::NextMonth, arrow),
        ));
        if geometry.layout.year_arrows {
            line.push_span(Span::styled(
                "»",
                self.hovered(state, Target::NextYear, arrow),
            ));
        }
        line
    }

    fn weekday_line(&self, state: &DatePickerState, geometry: Geometry) -> Line {
        let style = self.enabled(state, self.styles.weekday);
        let offset = match self.week_start {
            WeekStart::Monday => 0,
            WeekStart::Sunday => 6,
        };
        let mut line = Line::from_spans(vec![Span::raw(
            " ".repeat(usize::from(geometry.layout.calendar_x)),
        )]);
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
        geometry: Geometry,
    ) -> Line {
        let calendar_focused =
            state.interaction.focused && self.effective_part(state) == DatePickerPart::Calendar;
        let mut line = Line::from_spans(vec![Span::raw(
            " ".repeat(usize::from(geometry.layout.calendar_x)),
        )]);
        for column in 0..7 {
            if column > 0 {
                line.push_span(Span::raw(" "));
            }
            let index = week * 7 + column;
            let Some(date) = dates[index] else {
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
                style = self.hovered(state, Target::Day(u8::try_from(index).unwrap_or(0)), style);
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

    /// Row `row` of the month grid: three months.
    fn month_line(&self, state: &DatePickerState, row: u16) -> Line {
        let calendar_focused =
            state.interaction.focused && self.effective_part(state) == DatePickerPart::Calendar;
        let mut line = Line::new();
        for column in 0..3_u16 {
            if column > 0 {
                line.push_span(Span::raw(
                    " ".repeat(usize::from(MONTH_STRIDE - MONTH_WIDTH)),
                ));
            }
            let month = row * 3 + column;
            let name: String = self.month_names[usize::from(month)]
                .chars()
                .take(3)
                .collect();
            let mut style = self.enabled(state, self.styles.normal);
            style = self.hovered(
                state,
                Target::Month(u8::try_from(month).unwrap_or(0)),
                style,
            );
            if u32::from(month) == state.view.month0() && !state.interaction.disabled {
                style = style.patch(if calendar_focused {
                    self.styles.focused
                } else {
                    self.styles.selected
                });
            }
            line.push_span(Span::styled(
                format!("{name:^width$}", width = usize::from(MONTH_WIDTH)),
                style,
            ));
        }
        line
    }

    fn time_line(&self, state: &DatePickerState, geometry: Geometry) -> Line {
        let disabled = state.interaction.disabled;
        let focused_part = state
            .interaction
            .focused
            .then(|| self.effective_part(state));
        let separator = self.enabled(state, self.styles.separator);
        let mut line = Line::new();
        for (index, part) in self.time_parts().iter().enumerate() {
            if index > 0 {
                line.push_span(Span::styled(geometry.time_separator(), separator));
            }
            let style = if disabled {
                self.styles.disabled
            } else if focused_part == Some(*part) {
                self.styles.normal.patch(self.styles.focused)
            } else {
                self.hovered(state, Target::Field(*part), self.styles.normal)
            };
            line.push_span(Span::styled(
                format!("{:02}", state.time_field(*part)),
                style,
            ));
        }
        line
    }

    /// The Now (and Today) buttons under the time.
    fn buttons_line(&self, state: &DatePickerState, geometry: Geometry) -> Line {
        let button = self.enabled(state, self.styles.button);
        let mut line = Line::new();
        let mut x = geometry.area.x.saturating_add(geometry.time_x());
        for (rect, label, target) in [
            (geometry.now_button(), NOW_LABEL, Target::Now),
            (
                geometry
                    .today_button()
                    .filter(|_| matches!(geometry.layout.today, TodayPlace::Buttons)),
                TODAY_LABEL,
                Target::Today,
            ),
        ] {
            let Some(rect) = rect else { continue };
            line.push_span(Span::raw(" ".repeat(usize::from(rect.x.saturating_sub(x)))));
            line.push_span(Span::styled(label, self.hovered(state, target, button)));
            x = rect.right();
        }
        line
    }

    fn spin_line(&self, state: &DatePickerState, geometry: Geometry, up: bool) -> Line {
        let arrow = self.enabled(state, self.styles.arrow);
        let mut line = Line::new();
        let mut x = geometry.area.x.saturating_add(geometry.time_x());
        for part in self.time_parts() {
            let Some(field) = geometry.time_field_rect(*part) else {
                continue;
            };
            let target = if up {
                Target::Up(*part)
            } else {
                Target::Down(*part)
            };
            line.push_span(Span::raw(
                " ".repeat(usize::from(field.x.saturating_sub(x))),
            ));
            line.push_span(Span::styled(
                if up { "▲ " } else { "▼ " },
                self.hovered(state, target, arrow),
            ));
            x = field.right();
        }
        line
    }

    fn footer_line(&self, state: &DatePickerState, geometry: Geometry) -> Line {
        let button = self.enabled(state, self.styles.button);
        let mut line = Line::new();
        let mut x = geometry.area.x;
        let mut push = |line: &mut Line, rect: Rect, label: &'static str, target: Target| {
            line.push_span(Span::raw(" ".repeat(usize::from(rect.x.saturating_sub(x)))));
            line.push_span(Span::styled(label, self.hovered(state, target, button)));
            x = rect.right();
        };
        if let Some(rect) = geometry
            .today_button()
            .filter(|_| matches!(geometry.layout.today, TodayPlace::Footer))
        {
            push(&mut line, rect, TODAY_LABEL, Target::Today);
        }
        if let Some((cancel, ok)) = geometry.footer_buttons() {
            push(&mut line, cancel, CANCEL_LABEL, Target::Cancel);
            push(&mut line, ok, OK_LABEL, Target::Ok);
        }
        line
    }

    const fn time_parts(&self) -> &'static [DatePickerPart] {
        if self.show_seconds {
            &[
                DatePickerPart::Hour,
                DatePickerPart::Minute,
                DatePickerPart::Second,
            ]
        } else {
            &[DatePickerPart::Hour, DatePickerPart::Minute]
        }
    }
}

/// Whether `ch` begins a typed value from the calendar. Letters stay
/// calendar shortcuts (`t`, `m`); words are typed in the field itself.
const fn is_entry_char(ch: char) -> bool {
    ch.is_ascii_digit()
}

/// Insert `text` at character `cursor` of `buffer`, moving the cursor past it.
fn insert(buffer: &mut String, cursor: &mut usize, text: &str) {
    let index = byte_index(buffer, *cursor);
    buffer.insert_str(index, text);
    *cursor += text.chars().count();
}

/// Byte index of character `chars` in `text` (its length past the end).
fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index)
}

/// First visible character of a field `inner` columns wide, keeping the
/// cursor in view (the end, while the field shows the value's own text).
const fn text_scroll(cursor: usize, len: usize, inner: usize, typed: bool) -> usize {
    let cursor = if typed && cursor < len { cursor } else { len };
    if inner == 0 || cursor < inner {
        0
    } else {
        cursor + 1 - inner
    }
}

/// Row and column offsets of the picker's sections, relative to its
/// top-left corner.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // Independent sections of one layout.
struct Layout {
    width: u16,
    height: u16,
    /// Row of the text field (the readable value is the next row).
    text: Option<u16>,
    /// Row of the month header.
    calendar: Option<u16>,
    /// Column of the calendar's first day cell (and of its header).
    calendar_x: u16,
    /// Row of the time values.
    time: Option<u16>,
    /// Column of the hour field; `None` centers the fields.
    time_x: Option<u16>,
    /// Where the `Time` label goes, as `(column, row)`.
    time_label: Option<(u16, u16)>,
    /// `HH:MM:SS` rather than `HH : MM : SS`.
    tight_time: bool,
    /// ▲ / ▼ rows around the time row.
    time_buttons: bool,
    year_arrows: bool,
    /// Row of the Now (and Today) buttons, under the time.
    now_row: Option<u16>,
    /// Row of the Today / Cancel / OK buttons.
    footer: Option<u16>,
    today: TodayPlace,
    buttons: bool,
}

/// Where the Today button goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TodayPlace {
    None,
    /// Beside Now, under the time.
    Buttons,
    /// On the footer row (no time buttons to share).
    Footer,
}

/// Resolved picker geometry for one area.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    area: Rect,
    layout: Layout,
    content_width: u16,
    time_width: u16,
    show_seconds: bool,
}

/// A one-row rectangle at `(x, y)`.
const fn cell(x: u16, y: u16, width: u16) -> Rect {
    Rect::new(x, y, width, 1)
}

#[allow(clippy::cast_possible_truncation)] // Short ASCII labels.
const fn label_width(label: &str) -> u16 {
    label.len() as u16
}

trait RectRows {
    fn nth_row(self, row: u16) -> Self;
    fn shifted(self, delta: i16) -> Self;
}

impl RectRows for Rect {
    /// Row `row` of this rectangle, one row tall.
    fn nth_row(self, row: u16) -> Self {
        Self::new(self.x, self.y.saturating_add(row), self.width, 1)
    }

    /// This rectangle moved `delta` rows down.
    fn shifted(self, delta: i16) -> Self {
        Self::new(
            self.x,
            self.y.saturating_add_signed(delta),
            self.width,
            self.height,
        )
    }
}

impl Geometry {
    const fn header_y(self) -> u16 {
        match self.layout.calendar {
            Some(row) => self.area.y.saturating_add(row),
            None => self.area.y,
        }
    }

    const fn calendar_x(self) -> u16 {
        self.area.x.saturating_add(self.layout.calendar_x)
    }

    const fn header_row(self) -> Rect {
        cell(self.area.x, self.header_y(), self.content_width)
    }

    const fn prev_year(self) -> Rect {
        if self.layout.calendar.is_some() && self.layout.year_arrows {
            cell(self.calendar_x(), self.header_y(), 1)
        } else {
            Rect::new(0, 0, 0, 0)
        }
    }

    const fn next_year(self) -> Rect {
        if self.layout.calendar.is_some() && self.layout.year_arrows {
            cell(
                self.calendar_x().saturating_add(CALENDAR_WIDTH - 1),
                self.header_y(),
                1,
            )
        } else {
            Rect::new(0, 0, 0, 0)
        }
    }

    const fn prev_month(self) -> Rect {
        if self.layout.calendar.is_none() {
            return Rect::new(0, 0, 0, 0);
        }
        if self.layout.year_arrows {
            // `«‹` at the grid's left edge.
            cell(self.calendar_x().saturating_add(1), self.header_y(), 1)
        } else {
            cell(self.area.x, self.header_y(), 2)
        }
    }

    const fn next_month(self) -> Rect {
        if self.layout.calendar.is_none() {
            return Rect::new(0, 0, 0, 0);
        }
        if self.layout.year_arrows {
            // `›»` at the grid's right edge.
            cell(
                self.calendar_x().saturating_add(CALENDAR_WIDTH - 2),
                self.header_y(),
                1,
            )
        } else {
            cell(
                self.area
                    .x
                    .saturating_add(self.content_width.saturating_sub(2)),
                self.header_y(),
                2,
            )
        }
    }

    /// The month title between the month arrows, with a column of padding on
    /// each side.
    const fn title(self) -> Rect {
        let start = self.prev_month().x.saturating_add(1);
        let end = if self.layout.year_arrows {
            self.next_month().x
        } else {
            // The compact arrows are two columns wide, glyph on the outside.
            self.next_month().x.saturating_add(1)
        };
        cell(start, self.header_y(), end.saturating_sub(start))
    }

    const fn calendar(self) -> Rect {
        if self.layout.calendar.is_some() {
            Rect::new(
                self.area.x,
                self.header_y(),
                self.content_width,
                CALENDAR_HEIGHT,
            )
        } else {
            Rect::new(0, 0, 0, 0)
        }
    }

    const fn grid(self) -> Rect {
        if self.layout.calendar.is_some() {
            Rect::new(
                self.calendar_x(),
                self.header_y().saturating_add(2),
                CALENDAR_WIDTH,
                GRID_ROWS,
            )
        } else {
            Rect::new(0, 0, 0, 0)
        }
    }

    /// Return the day cell rect for grid `index`.
    const fn day_cell(self, index: usize) -> Rect {
        #[allow(clippy::cast_possible_truncation)] // index < 42
        let (row, column) = ((index / 7) as u16, (index % 7) as u16);
        cell(
            self.calendar_x().saturating_add(column * DAY_STRIDE),
            self.header_y().saturating_add(2).saturating_add(row),
            DAY_WIDTH,
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

    /// Return the month cell rect for month `index` (`0` = January).
    const fn month_cell(self, index: usize) -> Rect {
        #[allow(clippy::cast_possible_truncation)] // index < 12
        let (row, column) = ((index / 3) as u16, (index % 3) as u16);
        cell(
            self.calendar_x().saturating_add(column * MONTH_STRIDE),
            self.header_y().saturating_add(2).saturating_add(row),
            MONTH_WIDTH,
        )
    }

    const fn time_x(self) -> u16 {
        match self.layout.time_x {
            Some(x) => x,
            None => self.content_width.saturating_sub(self.time_width) / 2,
        }
    }

    const fn time_row(self) -> Option<Rect> {
        match self.layout.time {
            Some(row) => Some(cell(
                self.area.x,
                self.area.y.saturating_add(row),
                self.content_width,
            )),
            None => None,
        }
    }

    fn time_field_rect(self, part: DatePickerPart) -> Option<Rect> {
        let row = self.time_row()?;
        let index = part.time_index()?;
        if index == 2 && !self.show_seconds {
            return None;
        }
        Some(cell(
            row.x
                .saturating_add(self.time_x())
                .saturating_add(self.time_offsets()[index]),
            row.y,
            DAY_WIDTH,
        ))
    }

    const fn time_offsets(self) -> [u16; 3] {
        if self.layout.tight_time {
            TIGHT_TIME_OFFSETS
        } else {
            TIME_FIELD_OFFSETS
        }
    }

    const fn time_separator(self) -> &'static str {
        if self.layout.tight_time {
            ":"
        } else {
            TIME_SEPARATOR
        }
    }

    /// The time block's part of `row`: from the hour field to the right edge.
    const fn time_block(self, row: Rect) -> Rect {
        let x = row.x.saturating_add(self.time_x());
        Rect::new(x, row.y, row.right().saturating_sub(x), 1)
    }

    /// The `Time` label, if the layout shows one.
    fn time_label(self) -> Option<Rect> {
        let (x, y) = self.layout.time_label?;
        Some(cell(
            self.area.x.saturating_add(x),
            self.area.y.saturating_add(y),
            label_width(TIME_LABEL),
        ))
    }

    /// Return the time field nearest `position` on the time row; separators
    /// belong to the field on their left.
    fn time_field_at(self, position: Point) -> Option<DatePickerPart> {
        let row = self.time_row()?;
        let start = row.x.saturating_add(self.time_x());
        if !row.contains(position)
            || position.x < start
            || position.x >= start.saturating_add(self.time_width)
        {
            return None;
        }
        let dx = position.x - start;
        let offsets = self.time_offsets();
        Some(if dx < offsets[1] {
            DatePickerPart::Hour
        } else if !self.show_seconds || dx < offsets[2] {
            DatePickerPart::Minute
        } else {
            DatePickerPart::Second
        })
    }

    fn now_button(self) -> Option<Rect> {
        let row = self.layout.now_row?;
        Some(cell(
            self.area.x.saturating_add(self.time_x()),
            self.area.y.saturating_add(row),
            label_width(NOW_LABEL),
        ))
    }

    fn text_row(self) -> Option<Rect> {
        self.layout.text.map(|row| {
            cell(
                self.area.x,
                self.area.y.saturating_add(row),
                self.content_width,
            )
        })
    }

    /// Column of the field's first character.
    const fn text_x(self) -> u16 {
        self.area.x.saturating_add(1)
    }

    /// First visible character of the field for `cursor` in `len`.
    fn text_scroll(self, cursor: usize, len: usize) -> usize {
        text_scroll(
            cursor,
            len,
            usize::from(self.content_width.saturating_sub(2)),
            true,
        )
    }

    fn footer_row(self) -> Option<Rect> {
        self.layout.footer.map(|row| {
            cell(
                self.area.x,
                self.area.y.saturating_add(row),
                self.content_width,
            )
        })
    }

    fn today_button(self) -> Option<Rect> {
        match self.layout.today {
            TodayPlace::None => None,
            TodayPlace::Buttons => {
                let now = self.now_button()?;
                Some(cell(
                    now.right().saturating_add(1),
                    now.y,
                    label_width(TODAY_LABEL),
                ))
            }
            TodayPlace::Footer => {
                let row = self.footer_row()?;
                Some(cell(row.x, row.y, label_width(TODAY_LABEL)))
            }
        }
    }

    /// Cancel and OK, right-aligned.
    fn footer_buttons(self) -> Option<(Rect, Rect)> {
        let row = self.footer_row().filter(|_| self.layout.buttons)?;
        let ok_x = row.right().saturating_sub(label_width(OK_LABEL));
        let cancel_x = ok_x.saturating_sub(1 + label_width(CANCEL_LABEL));
        Some((
            cell(cancel_x, row.y, label_width(CANCEL_LABEL)),
            cell(ok_x, row.y, label_width(OK_LABEL)),
        ))
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
/// non-focusable regions for each clickable element (arrows, days, months,
/// time fields and buttons, the text field), and routes events through the
/// same resolved layout. State remains caller-owned through a `RefCell`.
pub struct DatePickerComponent<'a, 'state> {
    pointer_intent: bmux_tui::pointer::PointerIntent,
    id: LayoutId,
    picker: DatePicker<'a>,
    state: &'state RefCell<DatePickerState>,
    fallback: Style,
}

impl<'a, 'state> DatePickerComponent<'a, 'state> {
    /// Create a picker with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        picker: DatePicker<'a>,
        state: &'state RefCell<DatePickerState>,
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
        format!("{:?}", self.picker.policy).hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.pointer_intent).hash(&mut paint);
        format!("{:?}", self.picker).hash(&mut paint);
        format!("{:?}", self.fallback).hash(&mut paint);
        format!("{:?}", self.state.borrow()).hash(&mut paint);
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
        let state = self.state.borrow();
        let enabled = !state.interaction.disabled;
        let id = self.id.as_str();
        cx.push_hit(
            self.region(id.to_string(), area, HitRole::ListItem, enabled)
                .focusable(true),
        );
        let geometry = self.picker.geometry(area);
        let push = |cx: &mut PaintCx<'_, '_>, child: String, rect: Rect, role, semantic| {
            let rect = rect.intersection(area);
            if !rect.is_empty() {
                cx.push_hit(self.region(child.clone(), rect, role, enabled));
                cx.push_semantic(SemanticRegion::new(child, rect, semantic));
            }
        };
        for (target, rect) in self.picker.targets(geometry, &state) {
            let (suffix, role, semantic) = match target {
                Target::PrevMonth => ("prev".to_string(), HitRole::Action, "button"),
                Target::NextMonth => ("next".to_string(), HitRole::Action, "button"),
                Target::PrevYear => ("prev-year".to_string(), HitRole::Action, "button"),
                Target::NextYear => ("next-year".to_string(), HitRole::Action, "button"),
                Target::Title => ("title".to_string(), HitRole::Action, "button"),
                Target::Month(month) => (
                    format!("month.{}", month + 1),
                    HitRole::ListItem,
                    "gridcell",
                ),
                Target::Up(part) => (format!("{}.up", part_name(part)), HitRole::Action, "button"),
                Target::Down(part) => (
                    format!("{}.down", part_name(part)),
                    HitRole::Action,
                    "button",
                ),
                Target::Now => ("now".to_string(), HitRole::Action, "button"),
                Target::Today => ("today".to_string(), HitRole::Action, "button"),
                Target::Cancel => ("cancel".to_string(), HitRole::Action, "button"),
                Target::Ok => ("ok".to_string(), HitRole::Action, "button"),
                Target::Text => ("text".to_string(), HitRole::Action, "textbox"),
                Target::Day(_) | Target::Field(_) => continue,
            };
            push(cx, format!("{id}.{suffix}"), rect, role, semantic);
        }
        if self.picker.mode.has_date() && !state.months {
            for (index, date) in self.picker.grid_dates(state.view).iter().enumerate() {
                let Some(date) = date else { continue };
                push(
                    cx,
                    format!("{id}.day.{date}"),
                    geometry.day_cell(index),
                    HitRole::ListItem,
                    "gridcell",
                );
            }
        }
        for part in self.picker.parts() {
            let Some(rect) = geometry.time_field_rect(*part) else {
                continue;
            };
            push(
                cx,
                format!("{id}.{}", part_name(*part)),
                rect,
                HitRole::Action,
                "spinbutton",
            );
        }
        self.picker.paint(area, &state, self.fallback, cx);
        cx.push_semantic(SemanticRegion::new(id, area, "date-picker"));
        cx.push_damage(LocalRect::new(0, 0, area.width, area.height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let outcome = self
            .picker
            .handle_event(area, &mut self.state.borrow_mut(), event);
        match outcome {
            DatePickerOutcome::Ignored => EventOutcome::Ignored,
            DatePickerOutcome::Redraw
            | DatePickerOutcome::Changed(_)
            | DatePickerOutcome::Submitted(_)
            | DatePickerOutcome::Cancelled => EventOutcome::Redraw,
        }
    }
}

const fn part_name(part: DatePickerPart) -> &'static str {
    match part {
        DatePickerPart::Text => "text",
        DatePickerPart::Calendar => "calendar",
        DatePickerPart::Hour => "hour",
        DatePickerPart::Minute => "minute",
        DatePickerPart::Second => "second",
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

    fn render(picker: DatePicker<'_>, state: &DatePickerState) -> Vec<String> {
        let size = picker.preferred_size();
        let area = Rect::new(0, 0, size.width, size.height);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        let cell = RefCell::new(state.clone());
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
        let _ = render(picker, &state);
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
            render(DatePicker::new(DatePickerMode::Date), &state),
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
            render(sunday, &state),
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
            render(DatePicker::new(DatePickerMode::Time), &state),
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
        let state = RefCell::new(DatePickerState::new(at(2026, 11, 4, 9, 0, 0)));
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
        assert_eq!(state.borrow().date(), date(2026, 11, 5));
        let revision = component.revision();
        state.borrow_mut().set_part(DatePickerPart::Hour);
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

#[cfg(test)]
mod full_tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;

    use super::*;

    fn at(year: i32, month: u32, day: u32, h: u32, m: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(h, m, s)
            .unwrap()
    }

    fn picker(mode: DatePickerMode) -> DatePicker<'static> {
        DatePicker::new(mode)
            .show_seconds(true)
            .policy(DatePickerPolicy::full())
    }

    /// A focused picker on 2026-11-04 14:30:05, "now" 2026-11-04 09:00 at -05:00.
    fn state() -> DatePickerState {
        let mut state = DatePickerState::new(at(2026, 11, 4, 14, 30, 5));
        state.set_now(at(2026, 11, 4, 9, 0, 0), FixedOffset::east_opt(-5 * 3600));
        state.interaction.focused = true;
        state
    }

    fn area(picker: &DatePicker<'_>) -> Rect {
        let size = picker.preferred_size();
        Rect::new(0, 0, size.width, size.height)
    }

    fn send(
        picker: &DatePicker<'_>,
        state: &mut DatePickerState,
        event: &Event,
    ) -> DatePickerOutcome {
        picker.handle_event(area(picker), state, event)
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(code))
    }

    fn typed(
        picker: &DatePicker<'_>,
        state: &mut DatePickerState,
        text: &str,
    ) -> DatePickerOutcome {
        let mut outcome = DatePickerOutcome::Ignored;
        for ch in text.chars() {
            outcome = send(picker, state, &key(KeyCode::Char(ch)));
        }
        outcome
    }

    fn click(
        picker: &DatePicker<'_>,
        state: &mut DatePickerState,
        x: u16,
        y: u16,
    ) -> DatePickerOutcome {
        send(
            picker,
            state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(x, y),
            )),
        )
    }

    fn render(picker: &DatePicker<'_>, state: &DatePickerState) -> Vec<String> {
        let area = area(picker);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(0, 0, LocalRect::terminal(area), |cx| {
            picker.paint(area, state, Style::new(), cx);
        });
        (0..area.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect()
    }

    #[test]
    fn full_layout_shows_field_value_buttons_and_labels() {
        let picker = picker(DatePickerMode::DateTime);
        let rows = render(&picker, &state());
        assert_eq!(rows[0].trim(), "2026-11-04 14:30:05", "{rows:#?}");
        assert_eq!(rows[1].trim(), "We 4 Nov 2026, 14:30:05", "{rows:#?}");
        // The arrows hug the grid's edges.
        assert_eq!(rows[3].trim(), "«‹ November 2026  ›»", "{rows:#?}");
        assert_eq!(&rows[4][6..26], "Mo Tu We Th Fr Sa Su", "{rows:#?}");
        assert_eq!(rows[12].trim(), "▲  ▲  ▲", "{rows:#?}");
        assert_eq!(rows[13].trim_end(), "Time  14:30:05", "{rows:#?}");
        assert_eq!(rows[14].trim(), "▼  ▼  ▼", "{rows:#?}");
        // Today shares a row with Now; no row holds a lone button.
        assert_eq!(rows[15].trim(), "[ Now ] [ Today ]", "{rows:#?}");
        assert!(rows[17].ends_with("[ Cancel ] [ OK ]"), "{rows:#?}");
        assert_eq!(rows.len(), 18);
        // The compact preset keeps its size.
        assert_eq!(
            DatePicker::new(DatePickerMode::DateTime).preferred_size(),
            Size::new(CALENDAR_WIDTH, CALENDAR_HEIGHT + 2)
        );
        assert_eq!(
            DatePicker::new(DatePickerMode::DateTime).size_for_width(80),
            Size::new(CALENDAR_WIDTH, CALENDAR_HEIGHT + 2)
        );
    }

    fn render_at(picker: &DatePicker<'_>, state: &DatePickerState, width: u16) -> Vec<String> {
        let size = picker.size_for_width(width);
        let area = Rect::new(0, 0, size.width, size.height);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(0, 0, LocalRect::terminal(area), |cx| {
            picker.paint(area, state, Style::new(), cx);
        });
        (0..area.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect()
    }

    #[test]
    fn wide_layouts_put_the_time_beside_the_calendar_and_fill_the_width() {
        let no_footer = DatePicker::new(DatePickerMode::DateTime)
            .show_seconds(true)
            .policy(DatePickerPolicy {
                footer: false,
                ..DatePickerPolicy::full()
            });
        assert_eq!(
            no_footer.size_for_width(31),
            Size::new(FULL_WIDTH, 16),
            "never narrower"
        );
        assert_eq!(no_footer.size_for_width(60), Size::new(60, 11));
        let rows = render_at(&no_footer, &state(), 60);
        assert_eq!(
            rows[3].trim_end(),
            "«‹ November 2026  ›»      Time",
            "{rows:#?}"
        );
        assert_eq!(
            rows[4].trim_end(),
            "Mo Tu We Th Fr Sa Su      ▲  ▲  ▲",
            "{rows:#?}"
        );
        assert_eq!(
            rows[5].trim_end(),
            "26 27 28 29 30 31  1      14:30:05",
            "{rows:#?}"
        );
        assert_eq!(
            rows[6].trim_end(),
            " 2  3  4  5  6  7  8      ▼  ▼  ▼",
            "{rows:#?}"
        );
        assert_eq!(
            rows[8].trim_end(),
            "16 17 18 19 20 21 22      [ Now ] [ Today ]",
            "{rows:#?}"
        );
        // Every element is where it is drawn.
        let mut state = state();
        let area = Rect::new(0, 0, 60, 11);
        let click = |state: &mut DatePickerState, x, y| {
            no_footer.handle_event(
                area,
                state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(x, y),
                )),
            )
        };
        assert_eq!(
            click(&mut state, 26, 4),
            DatePickerOutcome::Changed(at(2026, 11, 4, 15, 30, 5))
        );
        assert_eq!(
            click(&mut state, 29, 6),
            DatePickerOutcome::Changed(at(2026, 11, 4, 15, 29, 5))
        );
        assert_eq!(
            click(&mut state, 30, 8),
            DatePickerOutcome::Changed(at(2026, 11, 4, 9, 0, 0)),
            "Now"
        );
        click(&mut state, 19, 3);
        assert_eq!(state.month_view(), (2027, 11), "»");
        assert_eq!(
            click(&mut state, 36, 8),
            DatePickerOutcome::Redraw,
            "already today: the view comes back"
        );
        // The date and time pickers fill the width too, left-aligned.
        let date = DatePicker::new(DatePickerMode::Date).policy(DatePickerPolicy::full());
        let rows = render_at(&date, &state, 60);
        assert_eq!(rows[3].trim_end(), "«‹ November 2026  ›»", "{rows:#?}");
        assert!(rows[rows.len() - 1].starts_with("[ Today ]"), "{rows:#?}");
    }

    #[test]
    fn typing_follows_the_value_live() {
        let picker = picker(DatePickerMode::DateTime);
        let mut state = state();
        // Tab into the field: typing replaces its text.
        send(
            &picker,
            &mut state,
            &Event::Key(KeyStroke::with_modifiers(
                KeyCode::Tab,
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            )),
        );
        assert_eq!(state.part(), DatePickerPart::Text);
        assert_eq!(
            typed(&picker, &mut state, "2026-1"),
            DatePickerOutcome::Redraw
        );
        assert!(state.text_pending() && state.text_error().is_none());
        assert_eq!(
            state.date(),
            at(2026, 11, 4, 0, 0, 0).date(),
            "nothing changes while incomplete"
        );
        assert!(render(&picker, &state)[1].contains("keep typing"));
        assert_eq!(
            typed(&picker, &mut state, "2-25"),
            DatePickerOutcome::Changed(at(2026, 12, 25, 14, 30, 5))
        );
        assert_eq!(state.month_view(), (2026, 12), "the calendar follows");
        assert_eq!(
            typed(&picker, &mut state, " 08:15"),
            DatePickerOutcome::Changed(at(2026, 12, 25, 8, 15, 0))
        );
        assert_eq!(state.typed_text(), Some("2026-12-25 08:15"));
        // A mistake says why and holds the last value; Enter does not submit.
        typed(&picker, &mut state, ":75");
        assert!(state.text_error().unwrap().contains("75"));
        assert_eq!(state.value(), at(2026, 12, 25, 8, 15, 0));
        assert_eq!(
            send(&picker, &mut state, &key(KeyCode::Enter)),
            DatePickerOutcome::Redraw
        );
        assert!(render(&picker, &state)[1].trim_start().starts_with('✗'));
        send(&picker, &mut state, &key(KeyCode::Backspace));
        send(&picker, &mut state, &key(KeyCode::Backspace));
        typed(&picker, &mut state, "45");
        assert_eq!(state.value(), at(2026, 12, 25, 8, 15, 45));
        // Leaving the field shows the value's own text again.
        send(&picker, &mut state, &key(KeyCode::Tab));
        assert_eq!(state.typed_text(), None);
        assert_eq!(picker.text(&state), "2026-12-25 08:15:45");
        assert_eq!(
            send(&picker, &mut state, &key(KeyCode::Enter)),
            DatePickerOutcome::Submitted(at(2026, 12, 25, 8, 15, 45))
        );
    }

    #[test]
    fn calendar_changes_rewrite_the_field() {
        let picker = picker(DatePickerMode::DateTime);
        let mut state = state();
        state.set_part(DatePickerPart::Text);
        typed(&picker, &mut state, "x");
        assert!(state.text_error().is_some());
        // Picking a day replaces bad text with the value's text.
        state.set_part(DatePickerPart::Calendar);
        send(&picker, &mut state, &key(KeyCode::Right));
        assert_eq!(state.typed_text(), None);
        assert_eq!(picker.text(&state), "2026-11-05 14:30:05");
        // A digit in the calendar starts a new entry.
        typed(&picker, &mut state, "2027-01-01");
        assert_eq!(state.part(), DatePickerPart::Text);
        assert_eq!(state.date(), NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
    }

    #[test]
    fn relative_words_and_buttons_use_the_supplied_clock() {
        let picker = picker(DatePickerMode::DateTime);
        let mut state = state();
        state.set_part(DatePickerPart::Text);
        state.replace_on_type = true;
        typed(&picker, &mut state, "tomorrow");
        assert_eq!(
            state.value(),
            at(2026, 11, 5, 14, 30, 5),
            "date words keep the time"
        );
        send(&picker, &mut state, &key(KeyCode::Tab));
        state.set_part(DatePickerPart::Text);
        state.replace_on_type = true;
        typed(&picker, &mut state, "now");
        assert_eq!(state.value(), at(2026, 11, 4, 9, 0, 0));
        assert_eq!(state.offset(), None, "offsets are off by default");

        let zoned = DatePicker::new(DatePickerMode::DateTime)
            .show_seconds(true)
            .policy(DatePickerPolicy {
                offsets: true,
                ..DatePickerPolicy::full()
            });
        let mut state = super::full_tests::state();
        // The Now button (row 15, under the fields) takes the clock's offset.
        assert_eq!(
            click(&zoned, &mut state, 8, 15),
            DatePickerOutcome::Changed(at(2026, 11, 4, 9, 0, 0))
        );
        assert_eq!(state.offset(), FixedOffset::east_opt(-5 * 3600));
        assert_eq!(zoned.text(&state), "2026-11-04 09:00:00-05:00");
        // Typed offsets are kept; Today keeps the time and offset.
        state.set_part(DatePickerPart::Text);
        state.replace_on_type = true;
        typed(&zoned, &mut state, "2026-10-01 12:00+02:00");
        assert_eq!(state.offset(), FixedOffset::east_opt(2 * 3600));
        assert_eq!(
            click(&zoned, &mut state, 16, 15),
            DatePickerOutcome::Changed(at(2026, 11, 4, 12, 0, 0))
        );
        assert_eq!(state.offset(), FixedOffset::east_opt(2 * 3600));
    }

    #[test]
    fn every_action_has_a_mouse_target() {
        let picker = picker(DatePickerMode::DateTime);
        let mut state = state();
        click(&picker, &mut state, 25, 3);
        assert_eq!(state.month_view(), (2027, 11), "» is next year");
        click(&picker, &mut state, 6, 3);
        assert_eq!(state.month_view(), (2026, 11), "« is the previous year");
        click(&picker, &mut state, 24, 3);
        assert_eq!(state.month_view(), (2026, 12), "› is next month");
        click(&picker, &mut state, 7, 3);
        assert_eq!(state.month_view(), (2026, 11), "‹ is the previous month");
        // Day 4 sits at grid index 9.
        assert_eq!(
            click(&picker, &mut state, 15, 6),
            DatePickerOutcome::Changed(at(2026, 11, 5, 14, 30, 5))
        );
        // ▲ / ▼ step their field.
        assert_eq!(
            click(&picker, &mut state, 6, 12),
            DatePickerOutcome::Changed(at(2026, 11, 5, 15, 30, 5))
        );
        assert_eq!(
            click(&picker, &mut state, 9, 14),
            DatePickerOutcome::Changed(at(2026, 11, 5, 15, 29, 5))
        );
        assert_eq!(state.part(), DatePickerPart::Minute);
        assert_eq!(
            click(&picker, &mut state, 27, 17),
            DatePickerOutcome::Submitted(at(2026, 11, 5, 15, 29, 5))
        );
        assert_eq!(
            click(&picker, &mut state, 17, 17),
            DatePickerOutcome::Cancelled
        );
        // Clicking the field places the cursor.
        click(&picker, &mut state, 6, 0);
        assert_eq!(state.part(), DatePickerPart::Text);
        assert_eq!(state.cursor, 5);
        typed(&picker, &mut state, "x");
        assert_eq!(state.typed_text(), Some("2026-x11-05 15:29:05"));
    }

    #[test]
    fn the_month_grid_picks_months_by_mouse_and_keyboard() {
        let picker = picker(DatePickerMode::Date);
        let mut state = state();
        click(&picker, &mut state, 15, 3);
        assert!(state.showing_months());
        let rows = render(&picker, &state);
        assert!(
            rows[3].contains("2026") && !rows[3].contains("November"),
            "{rows:#?}"
        );
        assert!(
            rows[5].contains("Jan") && rows[8].contains("Dec"),
            "{rows:#?}"
        );
        // March: row 0, column 2.
        click(&picker, &mut state, 6 + 2 * 7 + 1, 5);
        assert!(!state.showing_months());
        assert_eq!(state.month_view(), (2026, 3));
        assert_eq!(
            state.date(),
            NaiveDate::from_ymd_opt(2026, 11, 4).unwrap(),
            "only the view moves"
        );
        // m opens it; arrows move by month and row; Enter chooses.
        send(&picker, &mut state, &key(KeyCode::Char('m')));
        send(&picker, &mut state, &key(KeyCode::Down));
        send(&picker, &mut state, &key(KeyCode::Right));
        send(&picker, &mut state, &key(KeyCode::PageDown));
        send(&picker, &mut state, &key(KeyCode::Enter));
        assert!(!state.showing_months());
        assert_eq!(state.month_view(), (2027, 7));
    }

    #[test]
    fn hover_highlights_the_element_under_the_pointer() {
        let picker = picker(DatePickerMode::DateTime);
        let mut state = state();
        let moved = |x, y| Event::Mouse(MouseEvent::new(MouseEventKind::Move, Point::new(x, y)));
        assert_eq!(
            send(&picker, &mut state, &moved(15, 15)),
            DatePickerOutcome::Redraw
        );
        assert_eq!(state.hover, Some(Target::Today));
        assert_eq!(
            send(&picker, &mut state, &moved(16, 15)),
            DatePickerOutcome::Ignored
        );
        send(&picker, &mut state, &moved(12, 6));
        assert_eq!(state.hover, Some(Target::Day(9)));
        send(&picker, &mut state, &moved(31, 11));
        assert_eq!(state.hover, None);
    }

    #[test]
    fn component_registers_every_button() {
        let picker = picker(DatePickerMode::DateTime);
        let state = RefCell::new(state());
        let component = DatePickerComponent::new("when", picker, &state);
        let size = picker.preferred_size();
        let mut buffer = Buffer::empty(Rect::new(0, 0, size.width, size.height));
        let mut frame = Frame::new(&mut buffer);
        let layout = component.layout(Constraints::tight(size), &mut LayoutCx::new());
        PaintCx::new(&mut frame).with_child(
            0,
            0,
            LocalRect::new(0, 0, size.width, size.height),
            |cx| {
                component.paint(&layout, cx);
            },
        );
        let ids: Vec<&str> = frame
            .hits()
            .regions()
            .iter()
            .map(|region| region.id.as_str())
            .collect();
        for id in [
            "when.text",
            "when.prev-year",
            "when.prev",
            "when.title",
            "when.next",
            "when.next-year",
            "when.day.2026-11-04",
            "when.hour.up",
            "when.second.down",
            "when.now",
            "when.today",
            "when.cancel",
            "when.ok",
        ] {
            assert!(ids.contains(&id), "{id} missing from {ids:?}");
        }
    }

    #[test]
    fn hints_follow_the_focused_part() {
        let picker = picker(DatePickerMode::DateTime);
        let mut state = state();
        assert!(picker.hints(&state).iter().any(|(keys, _)| *keys == "m"));
        state.set_part(DatePickerPart::Minute);
        assert!(
            picker
                .hints(&state)
                .iter()
                .any(|(keys, action)| *keys == "n" && *action == "now")
        );
        state.set_part(DatePickerPart::Text);
        assert!(picker.hints(&state)[0].1.contains("today"));
    }
}
