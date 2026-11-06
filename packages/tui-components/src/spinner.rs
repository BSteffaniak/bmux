//! Time-driven activity spinner.
//!
//! A [`Spinner`] maps elapsed time to a frame, so its speed does not depend on how often the
//! application redraws. Measure elapsed time from one shared epoch (for example application
//! start) to keep every spinner on screen in step; measure from an activity's own start to
//! begin each spinner at its first frame.
//!
//! ```
//! use std::time::Duration;
//! use bmux_tui_components::spinner::Spinner;
//!
//! let spinner = Spinner::braille();
//! assert_eq!(spinner.frame(Duration::ZERO), "⠋");
//! assert_eq!(spinner.frame(spinner.interval()), "⠙");
//! assert_eq!(Spinner::fixed("…").next_change(Duration::ZERO), None);
//! ```

use std::sync::Arc;
use std::time::Duration;

use bmux_tui::prelude::Span;
use bmux_tui::style::Style;
use bmux_tui::text_width::display_width;

/// Default time each frame is shown.
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(80);

/// Animation frames and their speed. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Spinner {
    frames: Arc<[Box<str>]>,
    interval: Duration,
    width: usize,
}

impl Spinner {
    /// A spinner showing `frames` in order, each for `interval`. `None` when there are no
    /// frames or `interval` is zero.
    #[must_use]
    pub fn new<I, S>(frames: I, interval: Duration) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let frames: Arc<[Box<str>]> = frames
            .into_iter()
            .map(|frame| frame.into().into_boxed_str())
            .collect();
        if frames.is_empty() || interval.is_zero() {
            return None;
        }
        let width = frames
            .iter()
            .map(|frame| display_width(frame))
            .max()
            .unwrap_or_default();
        Some(Self {
            frames,
            interval,
            width,
        })
    }

    fn preset(frames: &[&str]) -> Self {
        Self::new(frames.iter().copied(), DEFAULT_INTERVAL)
            .unwrap_or_else(|| unreachable!("presets have frames"))
    }

    /// Braille dots (`⠋⠙⠹…`).
    #[must_use]
    pub fn braille() -> Self {
        Self::preset(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
    }

    /// A growing and shrinking dot (`·•●•`).
    #[must_use]
    pub fn dot() -> Self {
        Self::preset(&["·", "•", "●", "•"])
    }

    /// A rotating line drawn with ASCII (`|/-\`), for terminals without Unicode fonts.
    #[must_use]
    pub fn ascii() -> Self {
        Self::preset(&["|", "/", "-", "\\"])
    }

    /// A single unchanging frame, for when motion is turned off.
    #[must_use]
    pub fn fixed(frame: impl Into<String>) -> Self {
        Self::new([frame], DEFAULT_INTERVAL).unwrap_or_else(|| unreachable!("one frame"))
    }

    /// A preset by name: `braille`, `dot` or `ascii`.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "braille" => Some(Self::braille()),
            "dot" => Some(Self::dot()),
            "ascii" => Some(Self::ascii()),
            _ => None,
        }
    }

    /// The same frames shown for `interval` each. Unchanged when `interval` is zero.
    #[must_use]
    pub const fn with_interval(mut self, interval: Duration) -> Self {
        if !interval.is_zero() {
            self.interval = interval;
        }
        self
    }

    /// Time each frame is shown.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// Display width of the widest frame, in terminal cells. Reserve this much so text after
    /// the spinner does not shift between frames.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Whether the spinner has more than one frame.
    #[must_use]
    pub fn animates(&self) -> bool {
        self.frames.len() > 1
    }

    fn index(&self, elapsed: Duration) -> usize {
        let step = elapsed.as_nanos() / self.interval.as_nanos();
        // The remainder is below `frames.len()`, which is a `usize`.
        usize::try_from(step % self.frames.len() as u128).unwrap_or_default()
    }

    /// The frame shown `elapsed` after the spinner's epoch.
    #[must_use]
    pub fn frame(&self, elapsed: Duration) -> &str {
        &self.frames[self.index(elapsed)]
    }

    /// The frame at `elapsed` as a styled span.
    #[must_use]
    pub fn span(&self, elapsed: Duration, style: Style) -> Span {
        Span::styled(self.frame(elapsed), style)
    }

    /// Time from `elapsed` until the frame changes; `None` when it never does. Use it to
    /// schedule the next redraw.
    #[must_use]
    pub fn next_change(&self, elapsed: Duration) -> Option<Duration> {
        if !self.animates() {
            return None;
        }
        let interval = self.interval.as_nanos();
        let remaining = interval - elapsed.as_nanos() % interval;
        Some(Duration::from_nanos(
            u64::try_from(remaining).unwrap_or(u64::MAX),
        ))
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::braille()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_follow_elapsed_time_not_redraws() {
        let spinner = Spinner::ascii().with_interval(Duration::from_millis(100));
        let at = |millis| spinner.frame(Duration::from_millis(millis));
        assert_eq!(at(0), "|");
        assert_eq!(at(99), "|");
        assert_eq!(at(100), "/");
        assert_eq!(at(350), "\\");
        assert_eq!(at(400), "|");
        assert_eq!(
            spinner.next_change(Duration::from_millis(130)),
            Some(Duration::from_millis(70))
        );
    }

    #[test]
    fn fixed_spinners_never_change() {
        let spinner = Spinner::fixed("…");
        assert!(!spinner.animates());
        assert_eq!(spinner.frame(Duration::from_secs(9)), "…");
        assert_eq!(spinner.next_change(Duration::from_secs(9)), None);
    }

    #[test]
    fn width_is_the_widest_frame_in_cells() {
        let spinner = Spinner::new(["a", "界", "🙂"], DEFAULT_INTERVAL).unwrap();
        assert_eq!(spinner.width(), 2);
        assert_eq!(Spinner::braille().width(), 1);
    }

    #[test]
    fn empty_frames_and_zero_intervals_are_rejected() {
        assert!(Spinner::new(Vec::<String>::new(), DEFAULT_INTERVAL).is_none());
        assert!(Spinner::new(["x"], Duration::ZERO).is_none());
        assert_eq!(
            Spinner::braille().with_interval(Duration::ZERO).interval(),
            DEFAULT_INTERVAL
        );
        assert_eq!(Spinner::named("dot"), Some(Spinner::dot()));
        assert_eq!(Spinner::named("nope"), None);
    }
}
