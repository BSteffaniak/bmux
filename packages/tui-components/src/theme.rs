//! Neutral opt-in theme bundle for component style defaults.
//!
//! Component-specific style structs remain the source of truth for precise
//! caller control. This module only provides ergonomic conversions from a small
//! shared palette into common component style structs.

use bmux_tui::prelude::Style;
use bmux_tui::style::{Color, Modifier};

use crate::badge::BadgeStyles;
use crate::bar_chart::BarChartStyles;
use crate::breadcrumbs::BreadcrumbsStyles;
use crate::button::ButtonStyles;
use crate::chart::ChartStyles;
use crate::checkbox::CheckboxStyles;
use crate::common::InteractionStyles;
use crate::empty_state::EmptyStateStyles;
use crate::form_field::FormFieldStyles;
use crate::key_hint_bar::KeyHintBarStyles;
use crate::labeled_details::LabeledDetailsStyles;
use crate::modal_frame::ModalTheme;
use crate::pane::PaneStyles;
use crate::panel_group::PanelGroupStyles;
use crate::picker_frame::PickerFrameStyles;
use crate::progress_bar::ProgressBarStyles;
use crate::radio_group::RadioGroupStyles;
use crate::scrollbar::ScrollbarStyles;
use crate::select_dropdown::SelectDropdownStyles;
use crate::selectable_list::SelectableListStyles;
use crate::sparkline::SparklineStyles;
use crate::status_bar::StatusBarStyles;
use crate::stepper::StepperStyles;
use crate::tab_bar::TabBarStyles;
use crate::table::TableStyles;
#[cfg(feature = "text-input")]
use crate::text_input_box::TextInputBoxStyles;
use crate::text_view::TextViewStyles;
use crate::toast_stack::ToastStackStyles;
use crate::tree_view::TreeViewStyles;

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

    /// Convert into radio-group styles.
    #[must_use]
    pub fn radio_group_styles(self) -> RadioGroupStyles {
        RadioGroupStyles::from(self)
    }

    /// Convert into select/dropdown styles.
    #[must_use]
    pub fn select_dropdown_styles(self) -> SelectDropdownStyles {
        SelectDropdownStyles::from(self)
    }

    /// Convert into selectable-list styles.
    #[must_use]
    pub fn selectable_list_styles(self) -> SelectableListStyles {
        SelectableListStyles::from(self)
    }

    /// Convert into breadcrumb styles.
    #[must_use]
    pub fn breadcrumbs_styles(self) -> BreadcrumbsStyles {
        BreadcrumbsStyles::from(self)
    }

    /// Convert into tab-bar styles.
    #[must_use]
    pub fn tab_bar_styles(self) -> TabBarStyles {
        TabBarStyles::from(self)
    }

    /// Convert into tree-view styles.
    #[must_use]
    pub fn tree_view_styles(self) -> TreeViewStyles {
        TreeViewStyles::from(self)
    }

    /// Convert into form-field styles.
    #[must_use]
    pub fn form_field_styles(self) -> FormFieldStyles {
        FormFieldStyles::from(self)
    }

    /// Convert into text-input-box styles.
    #[cfg(feature = "text-input")]
    #[must_use]
    pub fn text_input_box_styles(self) -> TextInputBoxStyles {
        TextInputBoxStyles::from(self)
    }

    /// Convert into picker-frame styles.
    #[must_use]
    pub fn picker_frame_styles(self) -> PickerFrameStyles {
        PickerFrameStyles::from(self)
    }

    /// Convert into modal styles.
    #[must_use]
    pub fn modal_theme(self) -> ModalTheme {
        ModalTheme::from(self)
    }

    /// Convert into pane styles.
    #[must_use]
    pub fn pane_styles(self) -> PaneStyles {
        PaneStyles::from(self)
    }

    /// Convert into panel-group styles.
    #[must_use]
    pub fn panel_group_styles(self) -> PanelGroupStyles {
        PanelGroupStyles::from(self)
    }

    /// Convert into table styles.
    #[must_use]
    pub fn table_styles(self) -> TableStyles {
        TableStyles::from(self)
    }

    /// Convert into badge styles.
    #[must_use]
    pub fn badge_styles(self) -> BadgeStyles {
        BadgeStyles::from(self)
    }

    /// Convert into status-bar styles.
    #[must_use]
    pub fn status_bar_styles(self) -> StatusBarStyles {
        StatusBarStyles::from(self)
    }

    /// Convert into toast-stack styles.
    #[must_use]
    pub fn toast_stack_styles(self) -> ToastStackStyles {
        ToastStackStyles::from(self)
    }

    /// Convert into stepper styles.
    #[must_use]
    pub fn stepper_styles(self) -> StepperStyles {
        StepperStyles::from(self)
    }

    /// Convert into progress-bar styles.
    #[must_use]
    pub fn progress_bar_styles(self) -> ProgressBarStyles {
        ProgressBarStyles::from(self)
    }

    /// Convert into scrollbar styles.
    #[must_use]
    pub fn scrollbar_styles(self) -> ScrollbarStyles {
        ScrollbarStyles::from(self)
    }

    /// Convert into key-hint-bar styles.
    #[must_use]
    pub fn key_hint_bar_styles(self) -> KeyHintBarStyles {
        KeyHintBarStyles::from(self)
    }

    /// Convert into empty-state styles.
    #[must_use]
    pub fn empty_state_styles(self) -> EmptyStateStyles {
        EmptyStateStyles::from(self)
    }

    /// Convert into labeled-details styles.
    #[must_use]
    pub fn labeled_details_styles(self) -> LabeledDetailsStyles {
        LabeledDetailsStyles::from(self)
    }

    /// Convert into text-view styles.
    #[must_use]
    pub fn text_view_styles(self) -> TextViewStyles {
        TextViewStyles::from(self)
    }

    /// Convert into bar-chart styles.
    #[must_use]
    pub fn bar_chart_styles(self) -> BarChartStyles {
        BarChartStyles::from(self)
    }

    /// Convert into chart styles.
    #[must_use]
    pub fn chart_styles(self) -> ChartStyles {
        ChartStyles::from(self)
    }

    /// Convert into sparkline styles.
    #[must_use]
    pub fn sparkline_styles(self) -> SparklineStyles {
        SparklineStyles::from(self)
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

impl From<ComponentTheme> for RadioGroupStyles {
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

impl From<ComponentTheme> for SelectDropdownStyles {
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

impl From<ComponentTheme> for BreadcrumbsStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            normal: theme.base,
            current: theme.info.add_modifier(Modifier::BOLD),
            hovered: theme.info,
            pressed: theme.selected,
            disabled: theme.disabled,
            separator: theme.muted,
        }
    }
}

impl From<ComponentTheme> for TabBarStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            normal: theme.muted,
            selected: theme.selected.add_modifier(Modifier::BOLD),
            focused: theme.focused,
            hovered: theme.info,
            pressed: theme.selected,
            disabled: theme.disabled,
            separator: theme.border,
        }
    }
}

impl From<ComponentTheme> for TreeViewStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            normal: theme.base,
            selected: theme.selected.add_modifier(Modifier::BOLD),
            hovered: theme.info,
            pressed: theme.selected,
            disabled: theme.disabled,
            marker: theme.muted,
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

#[cfg(feature = "text-input")]
impl From<ComponentTheme> for TextInputBoxStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            text: theme.base,
            focused_text: theme.base.add_modifier(Modifier::BOLD),
            disabled_text: theme.disabled,
            placeholder: theme.muted,
            selection: theme.selected,
            border: theme.border,
            focused_border: theme.focused,
            background: theme.background,
            focused_background: theme.background,
            disabled_background: theme.background,
        }
    }
}

impl From<ComponentTheme> for PickerFrameStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            border: theme.focused,
            background: theme.background,
            header: theme.base.add_modifier(Modifier::BOLD),
            input: theme.base,
            list: theme.base,
            footer: theme.muted,
        }
    }
}

impl From<ComponentTheme> for ModalTheme {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            background: theme.background,
            border: theme.focused,
            title: theme.info.add_modifier(Modifier::BOLD),
            text: theme.base,
            muted: theme.muted,
            focused: theme.focused,
            scrim: None,
        }
    }
}

impl From<ComponentTheme> for PaneStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            background: Some(theme.background),
            border: theme.border,
            focused_border: theme.focused,
        }
    }
}

impl From<ComponentTheme> for PanelGroupStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            divider: theme.border,
            hovered_divider: theme.info,
            active_divider: theme.focused,
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

impl From<ComponentTheme> for BadgeStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            default: theme.base.add_modifier(Modifier::BOLD),
            info: theme.info.add_modifier(Modifier::BOLD),
            success: theme.success.add_modifier(Modifier::BOLD),
            warning: theme.warning.add_modifier(Modifier::BOLD),
            error: theme.error.add_modifier(Modifier::BOLD),
            muted: theme.muted,
        }
    }
}

impl From<ComponentTheme> for StatusBarStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            default: theme.base,
            muted: theme.muted,
            info: theme.info,
            success: theme.success,
            warning: theme.warning.add_modifier(Modifier::BOLD),
            error: theme.error.add_modifier(Modifier::BOLD),
            separator: theme.border,
            background: theme.background,
        }
    }
}

impl From<ComponentTheme> for ToastStackStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            default: theme.base.add_modifier(Modifier::BOLD),
            info: theme.info.add_modifier(Modifier::BOLD),
            success: theme.success.add_modifier(Modifier::BOLD),
            warning: theme.warning.add_modifier(Modifier::BOLD),
            error: theme.error.add_modifier(Modifier::BOLD),
            body: theme.base,
            close: theme.muted,
            border: theme.border,
        }
    }
}

impl From<ComponentTheme> for StepperStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            pending: theme.muted,
            current: theme.info.add_modifier(Modifier::BOLD),
            complete: theme.success,
            warning: theme.warning.add_modifier(Modifier::BOLD),
            error: theme.error.add_modifier(Modifier::BOLD),
            disabled: theme.disabled,
            connector: theme.border,
        }
    }
}

impl From<ComponentTheme> for ProgressBarStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            filled: theme.success,
            empty: theme.muted,
            label: theme.base.add_modifier(Modifier::BOLD),
            complete: theme.success.add_modifier(Modifier::BOLD),
            indeterminate: theme.info,
            background: theme.background,
        }
    }
}

impl From<ComponentTheme> for ScrollbarStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            begin: theme.border,
            track: theme.border,
            thumb: theme.info,
            end: theme.border,
        }
    }
}

impl From<ComponentTheme> for KeyHintBarStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            key: theme.base.add_modifier(Modifier::BOLD),
            label: theme.muted,
            separator: theme.border,
            disabled: theme.disabled,
            background: theme.background,
        }
    }
}

impl From<ComponentTheme> for EmptyStateStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            icon: theme.muted,
            title: theme.base.add_modifier(Modifier::BOLD),
            body: theme.muted,
            action: theme.info,
            background: theme.background,
        }
    }
}

impl From<ComponentTheme> for LabeledDetailsStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            label: theme.muted.add_modifier(Modifier::BOLD),
            value: theme.base,
            continuation: theme.muted,
        }
    }
}

impl From<ComponentTheme> for TextViewStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            text: theme.base,
            empty: theme.muted,
            background: theme.background,
        }
    }
}

impl From<ComponentTheme> for BarChartStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            label: theme.base,
            bar: theme.info.add_modifier(Modifier::BOLD),
            empty: theme.muted,
            value: theme.muted,
            empty_message: theme.muted,
        }
    }
}

impl From<ComponentTheme> for ChartStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            dataset: theme.info,
            empty: theme.muted,
        }
    }
}

impl From<ComponentTheme> for SparklineStyles {
    fn from(theme: ComponentTheme) -> Self {
        Self {
            normal: theme.info,
            latest: theme.info.add_modifier(Modifier::BOLD),
            first: theme.base,
            high: theme.success.add_modifier(Modifier::BOLD),
            low: theme.error,
            empty: theme.muted,
            background: theme.background,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::style::{Color, Modifier, Style};

    use super::ComponentTheme;

    #[test]
    fn default_theme_exposes_expected_semantic_styles() {
        let theme = ComponentTheme::default();

        assert_eq!(theme.base.fg, Some(Color::White));
        assert!(theme.focused.modifiers.contains(Modifier::REVERSED));
        assert_eq!(theme.error.fg, Some(Color::Red));
    }

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
}
