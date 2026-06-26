//! Neutral opt-in theme bundle for component style defaults.
//!
//! Component-specific style structs remain the source of truth for precise
//! caller control. This module only provides ergonomic conversions from a small
//! shared palette into common component style structs.

use bmux_tui::prelude::Style;
use bmux_tui::style::{Color, Modifier};

use crate::button::ButtonStyles;
use crate::checkbox::CheckboxStyles;
use crate::form_field::FormFieldStyles;
use crate::selectable_list::SelectableListStyles;
use crate::table::TableStyles;

/// Neutral component theme palette.
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

    /// Convert into button styles.
    #[must_use]
    pub fn button_styles(self) -> ButtonStyles {
        ButtonStyles::from(self)
    }

    /// Convert into checkbox styles.
    #[must_use]
    pub fn checkbox_styles(self) -> CheckboxStyles {
        CheckboxStyles::from(self)
    }

    /// Convert into selectable-list styles.
    #[must_use]
    pub fn selectable_list_styles(self) -> SelectableListStyles {
        SelectableListStyles::from(self)
    }

    /// Convert into form-field styles.
    #[must_use]
    pub fn form_field_styles(self) -> FormFieldStyles {
        FormFieldStyles::from(self)
    }

    /// Convert into table styles.
    #[must_use]
    pub fn table_styles(self) -> TableStyles {
        TableStyles::from(self)
    }
}

impl Default for ComponentTheme {
    fn default() -> Self {
        Self::bmux_default()
    }
}

impl From<ComponentTheme> for ButtonStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            normal: theme.base,
            focused: theme.focused,
            hovered: theme.info,
            pressed: theme.selected,
            disabled: theme.disabled,
        }
    }
}

impl From<ComponentTheme> for CheckboxStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            normal: theme.base,
            focused: theme.focused,
            hovered: theme.info,
            pressed: theme.selected,
            disabled: theme.disabled,
        }
    }
}

impl From<ComponentTheme> for SelectableListStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            normal: theme.base,
            focused: theme.focused,
            selected: theme.selected,
            hovered: theme.info,
            pressed: theme.selected.add_modifier(Modifier::BOLD),
            disabled: theme.disabled,
        }
    }
}

impl From<ComponentTheme> for FormFieldStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            label: theme.base.add_modifier(Modifier::BOLD),
            required_marker: theme.error.add_modifier(Modifier::BOLD),
            help: theme.muted,
            error: theme.error,
        }
    }
}

impl From<ComponentTheme> for TableStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            header: theme.info.add_modifier(Modifier::BOLD),
            row: theme.base,
            selected: theme.selected,
            selected_column: theme.focused,
            selected_cell: theme.warning,
            hovered: theme.info,
            disabled: theme.disabled,
            separator: theme.border,
            empty: theme.muted,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::style::{Color, Modifier};

    use super::ComponentTheme;

    #[test]
    fn default_theme_exposes_expected_semantic_styles() {
        let theme = ComponentTheme::default();

        assert_eq!(theme.base.fg, Some(Color::White));
        assert!(theme.focused.modifiers.contains(Modifier::REVERSED));
        assert_eq!(theme.error.fg, Some(Color::Red));
    }

    #[test]
    fn theme_converts_to_common_component_style_structs() {
        let theme = ComponentTheme::default();

        assert_eq!(theme.button_styles().disabled, theme.disabled);
        assert_eq!(theme.checkbox_styles().focused, theme.focused);
        assert_eq!(theme.selectable_list_styles().selected, theme.selected);
        assert_eq!(theme.form_field_styles().error, theme.error);
        assert_eq!(theme.table_styles().separator, theme.border);
    }
}
