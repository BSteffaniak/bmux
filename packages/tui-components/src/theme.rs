//! Neutral opt-in theme bundle for component style defaults.
//!
//! Component-specific style structs remain the source of truth for precise
//! caller control. This module only provides ergonomic conversions from a small
//! shared palette into common component style structs.

use bmux_tui::prelude::Style;
use bmux_tui::style::{Color, Modifier};

use crate::common::InteractionStyles;

/// Neutral component theme palette.
///
/// The palette is deliberately domain-neutral. Applications can construct a
/// palette from their own semantic theme and then derive consistent generic
/// component styles while retaining the ability to override any component's
/// precise style structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentTheme {
    /// Base text style.
    pub base: Style,
    /// Background fill style.
    pub background: Style,
    /// Focused interactive style.
    pub focused: Style,
    /// Selected interactive style.
    pub selected: Style,
    /// Disabled/muted unavailable style.
    pub disabled: Style,
    /// Muted metadata/separator style.
    pub muted: Style,
    /// Informational accent style.
    pub info: Style,
    /// Success accent style.
    pub success: Style,
    /// Warning accent style.
    pub warning: Style,
    /// Error accent style.
    pub error: Style,
    /// Border/separator style.
    pub border: Style,
}

impl ComponentTheme {
    /// Return the default neutral BMUX component theme.
    #[must_use]
    pub const fn bmux_default() -> Self {
        Self {
            base: Style::new().fg(Color::White),
            background: Style::new(),
            focused: Style::new().add_modifier(Modifier::REVERSED),
            selected: Style::new().fg(Color::Black).bg(Color::Cyan),
            disabled: Style::new()
                .fg(Color::BrightBlack)
                .add_modifier(Modifier::DIM),
            muted: Style::new().fg(Color::BrightBlack),
            info: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            border: Style::new().fg(Color::BrightBlack),
        }
    }

    /// Return a terminal-native neutral theme.
    ///
    /// Base text and backgrounds use the backend defaults. Semantic status
    /// colors remain ANSI colors so terminals retain control of their exact
    /// palette, and no component receives an explicit background fill unless
    /// the caller supplies one.
    #[must_use]
    pub const fn terminal_default() -> Self {
        Self {
            base: Style::new().fg(Color::Default),
            background: Style::new().bg(Color::Default),
            focused: Style::new().add_modifier(Modifier::REVERSED),
            selected: Style::new().add_modifier(Modifier::REVERSED),
            disabled: Style::new().add_modifier(Modifier::DIM),
            muted: Style::new().add_modifier(Modifier::DIM),
            info: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            border: Style::new().add_modifier(Modifier::DIM),
        }
    }

    /// Convert into interaction styles.
    #[must_use]
    pub fn interaction_styles(self) -> InteractionStyles {
        InteractionStyles::from(self)
    }
}

impl Default for ComponentTheme {
    fn default() -> Self {
        Self::bmux_default()
    }
}

impl From<ComponentTheme> for InteractionStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self::new(
            theme.base,
            theme.focused,
            theme.info,
            theme.selected,
            theme.disabled,
        )
    }
}

#[cfg(feature = "text-input")]
#[cfg(test)]
mod tests {
    #[cfg(feature = "all")]
    use bmux_tui::style::Style;
    use bmux_tui::style::{Color, Modifier};

    use super::ComponentTheme;

    #[test]
    fn default_theme_exposes_expected_semantic_styles() {
        let theme = ComponentTheme::default();

        assert_eq!(theme.base.fg, Some(Color::White));
        assert!(theme.focused.modifiers.contains(Modifier::REVERSED));
        assert_eq!(theme.error.fg, Some(Color::Red));
    }

    #[cfg(feature = "all")]
    #[test]
    fn terminal_default_theme_leaves_component_backgrounds_terminal_native() {
        let theme = ComponentTheme::terminal_default();

        assert_eq!(theme.base.fg, Some(Color::Default));
        assert_eq!(theme.background.bg, Some(Color::Default));
        assert_eq!(theme.picker_frame_styles().background, theme.background);
        assert_eq!(theme.modal_theme().background, theme.background);
        #[cfg(feature = "text-input")]
        assert_eq!(theme.text_input_box_styles().background, theme.background);
    }

    #[cfg(feature = "all")]
    #[test]
    fn theme_converts_to_generic_component_style_families() {
        let theme = ComponentTheme {
            base: Style::new().fg(Color::White),
            background: Style::new().bg(Color::Black),
            focused: Style::new().fg(Color::BrightCyan),
            selected: Style::new().fg(Color::Black).bg(Color::Cyan),
            disabled: Style::new().fg(Color::BrightBlack),
            muted: Style::new().fg(Color::BrightBlack),
            info: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            border: Style::new().fg(Color::Blue),
        };

        assert_eq!(theme.button_styles().disabled, theme.disabled);
        assert_eq!(theme.checkbox_styles().focused, theme.focused);
        assert_eq!(theme.selectable_list_styles().selected, theme.selected);
        assert_eq!(theme.form_field_styles().error, theme.error);
        assert_eq!(theme.table_styles().separator, theme.border);
        assert_eq!(theme.picker_frame_styles().background, theme.background);
        assert_eq!(theme.modal_theme().border, theme.focused);
        assert_eq!(theme.status_bar_styles().success, theme.success);
        assert_eq!(theme.toast_stack_styles().error.fg, theme.error.fg);
        assert_eq!(theme.stepper_styles().complete, theme.success);
        assert_eq!(theme.progress_bar_styles().indeterminate, theme.info);
        assert_eq!(theme.scrollbar_styles().track, theme.border);
        assert_eq!(theme.empty_state_styles().action, theme.info);
        assert_eq!(theme.text_view_styles().text, theme.base);
        assert_eq!(theme.chart_styles().dataset, theme.info);
        assert_eq!(theme.sparkline_styles().low, theme.error);
    }

    #[cfg(feature = "all")]
    #[test]
    fn component_state_matrix_uses_only_supplied_theme_styles() {
        use bmux_tui::buffer::Buffer;
        use bmux_tui::frame::Frame;
        use bmux_tui::geometry::{Point, Rect};

        use crate::button::{Button, ButtonState};
        use crate::common::InteractionState;

        let theme = ComponentTheme {
            base: Style::new().fg(Color::Rgb(1, 1, 1)),
            background: Style::new().bg(Color::Rgb(2, 2, 2)),
            focused: Style::new().fg(Color::Rgb(3, 3, 3)),
            selected: Style::new().fg(Color::Rgb(4, 4, 4)),
            disabled: Style::new().fg(Color::Rgb(5, 5, 5)),
            muted: Style::new().fg(Color::Rgb(6, 6, 6)),
            info: Style::new().fg(Color::Rgb(7, 7, 7)),
            success: Style::new().fg(Color::Rgb(8, 8, 8)),
            warning: Style::new().fg(Color::Rgb(9, 9, 9)),
            error: Style::new().fg(Color::Rgb(10, 10, 10)),
            border: Style::new().fg(Color::Rgb(11, 11, 11)),
        };
        let states = [
            ("normal", InteractionState::new(), theme.base),
            (
                "hovered",
                InteractionState {
                    hovered: true,
                    ..InteractionState::new()
                },
                theme.info,
            ),
            (
                "focused",
                InteractionState {
                    focused: true,
                    ..InteractionState::new()
                },
                theme.focused,
            ),
            (
                "pressed",
                InteractionState {
                    pressed: true,
                    ..InteractionState::new()
                },
                theme.selected,
            ),
            (
                "disabled",
                InteractionState {
                    disabled: true,
                    ..InteractionState::new()
                },
                theme.disabled,
            ),
            (
                "disabled-precedence",
                InteractionState {
                    focused: true,
                    hovered: true,
                    pressed: true,
                    disabled: true,
                },
                theme.disabled,
            ),
        ];

        for (label, interaction, expected) in states {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
            let mut frame = Frame::new(&mut buffer);
            Button::new("Action").styles(theme.button_styles()).render(
                Rect::new(0, 0, 12, 1),
                &ButtonState { interaction },
                &mut frame,
            );
            assert_eq!(
                frame
                    .buffer()
                    .get(Point::new(0, 0))
                    .expect("button cell")
                    .style,
                expected,
                "button {label}"
            );
        }

        let checkbox = theme.checkbox_styles();
        assert_eq!(checkbox.normal, theme.base);
        assert_eq!(checkbox.hovered, theme.info);
        assert_eq!(checkbox.focused, theme.focused);
        assert_eq!(checkbox.pressed, theme.selected);
        assert_eq!(checkbox.disabled, theme.disabled);

        let badge = theme.badge_styles();
        assert_eq!(badge.info.fg, theme.info.fg);
        assert_eq!(badge.success.fg, theme.success.fg);
        assert_eq!(badge.warning.fg, theme.warning.fg);
        assert_eq!(badge.error.fg, theme.error.fg);
        assert_eq!(theme.empty_state_styles().body, theme.muted);
        assert_eq!(theme.progress_bar_styles().indeterminate, theme.info);
    }
}
