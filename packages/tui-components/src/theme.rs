//! Neutral opt-in theme bundle for component style defaults.
//!
//! Component-specific style structs remain the source of truth for precise
//! caller control. This module only provides ergonomic conversions from a small
//! shared palette into common component style structs.

use bmux_tui::prelude::Style;
use bmux_tui::style::{Color, Modifier};

use crate::common::InteractionStyles;

/// Generic interactive surface depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentSurfaceDepth {
    /// Ordinary content resting directly on the application canvas.
    Normal,
    /// Raised controls such as composers, palettes, and cards.
    Raised,
    /// Opaque overlays and modal panels.
    Overlay,
}

/// Semantic component surfaces independent of a product theme schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentSurfaces {
    /// Ordinary content surface.
    pub normal: Style,
    /// Raised interactive surface.
    pub raised: Style,
    /// Overlay/modal surface.
    pub overlay: Style,
    /// Optional full-parent scrim used beneath overlays.
    pub scrim: Option<Style>,
}

impl ComponentSurfaces {
    /// Resolve one deterministic surface depth.
    #[must_use]
    pub const fn resolve(self, depth: ComponentSurfaceDepth) -> Style {
        match depth {
            ComponentSurfaceDepth::Normal => self.normal,
            ComponentSurfaceDepth::Raised => self.raised,
            ComponentSurfaceDepth::Overlay => self.overlay,
        }
    }
}

/// Neutral component theme palette.
///
/// The palette is deliberately domain-neutral. Applications can construct a
/// palette from their own semantic theme and then derive consistent generic
/// component styles while retaining the ability to override any component's
/// precise style structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentTheme {
    /// Complete application canvas style.
    pub canvas: Style,
    /// Surface styles ordered by interactive depth.
    pub surfaces: ComponentSurfaces,
    /// Primary text style.
    pub text: Style,
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
            canvas: Style::new(),
            surfaces: ComponentSurfaces {
                normal: Style::new(),
                raised: Style::new(),
                overlay: Style::new().bg(Color::Black),
                scrim: None,
            },
            text: Style::new().fg(Color::White),
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

    /// Return an opaque dark semantic theme suitable for component galleries
    /// and applications that do not supply their own palette.
    #[must_use]
    pub const fn opaque_dark() -> Self {
        Self {
            canvas: Style::new().bg(Color::Black),
            surfaces: ComponentSurfaces {
                normal: Style::new().bg(Color::Black),
                raised: Style::new().bg(Color::Rgb(24, 24, 27)),
                overlay: Style::new().bg(Color::Rgb(39, 39, 42)),
                scrim: Some(Style::new().bg(Color::Black)),
            },
            text: Style::new().fg(Color::BrightWhite),
            focused: Style::new().fg(Color::BrightCyan),
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

    /// Return an opaque light semantic theme suitable for component galleries
    /// and applications that do not supply their own palette.
    #[must_use]
    pub const fn opaque_light() -> Self {
        Self {
            canvas: Style::new().bg(Color::White),
            surfaces: ComponentSurfaces {
                normal: Style::new().bg(Color::White),
                raised: Style::new().bg(Color::Rgb(244, 244, 245)),
                overlay: Style::new().bg(Color::Rgb(228, 228, 231)),
                scrim: Some(Style::new().bg(Color::BrightBlack)),
            },
            text: Style::new().fg(Color::Black),
            focused: Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
            selected: Style::new().fg(Color::White).bg(Color::Blue),
            disabled: Style::new()
                .fg(Color::BrightBlack)
                .add_modifier(Modifier::DIM),
            muted: Style::new().fg(Color::BrightBlack),
            info: Style::new().fg(Color::Blue),
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
            canvas: Style::new().bg(Color::Default),
            surfaces: ComponentSurfaces {
                normal: Style::new().bg(Color::Default),
                raised: Style::new().bg(Color::Default),
                overlay: Style::new().bg(Color::Default),
                scrim: None,
            },
            text: Style::new().fg(Color::Default),
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

    /// Resolve all semantic roles against one surface depth.
    ///
    /// Surface presentation is patched beneath role presentation, so explicit
    /// role foregrounds, backgrounds, and modifiers win deterministically.
    #[must_use]
    pub const fn for_surface(self, depth: ComponentSurfaceDepth) -> Self {
        let surface = self.surfaces.resolve(depth);
        Self {
            canvas: self.canvas,
            surfaces: self.surfaces,
            text: surface.patch(self.text),
            focused: surface.patch(self.focused),
            selected: surface.patch(self.selected),
            disabled: surface.patch(self.disabled),
            muted: surface.patch(self.muted),
            info: surface.patch(self.info),
            success: surface.patch(self.success),
            warning: surface.patch(self.warning),
            error: surface.patch(self.error),
            border: surface.patch(self.border),
        }
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
            theme.text,
            theme.focused,
            theme.info,
            theme.selected,
            theme.disabled,
        )
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::style::Style;
    use bmux_tui::style::{Color, Modifier};

    use super::{ComponentSurfaces, ComponentTheme};

    #[test]
    fn default_theme_exposes_expected_semantic_styles() {
        let theme = ComponentTheme::default();

        assert_eq!(theme.text.fg, Some(Color::White));
        assert!(theme.focused.modifiers.contains(Modifier::REVERSED));
        assert_eq!(theme.error.fg, Some(Color::Red));
    }

    #[test]
    fn surface_depth_patches_roles_deterministically() {
        let theme = ComponentTheme {
            text: Style::new().fg(Color::White),
            canvas: Style::new().bg(Color::Black),
            surfaces: ComponentSurfaces {
                normal: Style::new().bg(Color::Blue),
                raised: Style::new().bg(Color::Green),
                overlay: Style::new().bg(Color::Red),
                scrim: Some(Style::new().bg(Color::BrightBlack)),
            },
            focused: Style::new().fg(Color::Cyan),
            selected: Style::new().fg(Color::Black).bg(Color::Yellow),
            disabled: Style::new().add_modifier(Modifier::DIM),
            muted: Style::new().fg(Color::BrightBlack),
            info: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            border: Style::new().fg(Color::Magenta),
        };

        let raised = theme.for_surface(super::ComponentSurfaceDepth::Raised);
        assert_eq!(raised.text.bg, Some(Color::Green));
        assert_eq!(raised.text.fg, Some(Color::White));
        assert_eq!(raised.focused.bg, Some(Color::Green));
        assert_eq!(raised.selected.bg, Some(Color::Yellow));
        assert_eq!(raised.disabled.bg, Some(Color::Green));
        assert!(raised.disabled.modifiers.contains(Modifier::DIM));

        let overlay = theme.for_surface(super::ComponentSurfaceDepth::Overlay);
        assert_eq!(overlay.border.bg, Some(Color::Red));
        assert_eq!(overlay.surfaces.scrim, theme.surfaces.scrim);
    }

    #[test]
    fn terminal_default_preserves_default_at_every_surface_depth() {
        let theme = ComponentTheme::terminal_default();
        for depth in [
            super::ComponentSurfaceDepth::Normal,
            super::ComponentSurfaceDepth::Raised,
            super::ComponentSurfaceDepth::Overlay,
        ] {
            let resolved = theme.for_surface(depth);
            assert_eq!(resolved.text.fg, Some(Color::Default));
            assert_eq!(resolved.text.bg, Some(Color::Default));
            assert_eq!(resolved.border.bg, Some(Color::Default));
        }
    }

    #[cfg(feature = "all")]
    #[test]
    fn dark_light_and_terminal_themes_cover_every_component_family() {
        for theme in [
            ComponentTheme::opaque_dark(),
            ComponentTheme::opaque_light(),
            ComponentTheme::terminal_default(),
        ] {
            assert_eq!(
                theme.button_styles().normal,
                theme.for_surface(super::ComponentSurfaceDepth::Normal).text
            );
            assert_eq!(
                theme.picker_frame_styles().background,
                theme.surfaces.raised
            );
            assert_eq!(theme.modal_theme().background, theme.surfaces.overlay);
            assert_eq!(theme.modal_theme().scrim, theme.surfaces.scrim);
            assert_eq!(
                theme.text_input_box_styles().focused_background,
                theme.surfaces.normal
            );
            assert_eq!(theme.status_bar_styles().error.fg, theme.error.fg);
            assert_eq!(theme.badge_styles().success.fg, theme.success.fg);
            assert_eq!(theme.table_styles().separator.fg, theme.border.fg);
        }
    }

    #[cfg(feature = "all")]
    #[test]
    fn terminal_default_theme_leaves_component_backgrounds_terminal_native() {
        let theme = ComponentTheme::terminal_default();

        assert_eq!(theme.text.fg, Some(Color::Default));
        assert_eq!(theme.surfaces.normal.bg, Some(Color::Default));
        assert_eq!(
            theme.picker_frame_styles().background,
            theme.surfaces.normal
        );
        assert_eq!(theme.modal_theme().background, theme.surfaces.overlay);
        #[cfg(feature = "text-input")]
        assert_eq!(
            theme.text_input_box_styles().background,
            theme.surfaces.normal
        );
    }

    #[cfg(feature = "all")]
    #[test]
    fn theme_converts_to_generic_component_style_families() {
        let theme = ComponentTheme {
            text: Style::new().fg(Color::White),
            canvas: Style::new().bg(Color::Black),
            surfaces: ComponentSurfaces {
                normal: Style::new().bg(Color::Black),
                raised: Style::new().bg(Color::Black),
                overlay: Style::new().bg(Color::Black),
                scrim: None,
            },
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

        let normal = theme.for_surface(super::ComponentSurfaceDepth::Normal);
        let raised = theme.for_surface(super::ComponentSurfaceDepth::Raised);
        let overlay = theme.for_surface(super::ComponentSurfaceDepth::Overlay);

        assert_eq!(theme.button_styles().disabled, normal.disabled);
        assert_eq!(theme.checkbox_styles().focused, normal.focused);
        assert_eq!(theme.selectable_list_styles().selected, normal.selected);
        assert_eq!(theme.form_field_styles().error, normal.error);
        assert_eq!(theme.table_styles().separator, normal.border);
        assert_eq!(
            theme.picker_frame_styles().background,
            theme.surfaces.normal
        );
        assert_eq!(theme.modal_theme().border, overlay.focused);
        assert_eq!(theme.status_bar_styles().success, normal.success);
        assert_eq!(theme.toast_stack_styles().error.fg, raised.error.fg);
        assert_eq!(theme.stepper_styles().complete, normal.success);
        assert_eq!(theme.progress_bar_styles().indeterminate, normal.info);
        assert_eq!(theme.scrollbar_styles().track, normal.border);
        assert_eq!(theme.empty_state_styles().action, normal.info);
        assert_eq!(theme.text_view_styles().text, normal.text);
        assert_eq!(theme.chart_styles().dataset, normal.info);
        assert_eq!(theme.sparkline_styles().low, normal.error);
    }

    #[cfg(feature = "all")]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn component_state_matrix_uses_only_supplied_theme_styles() {
        use std::cell::Cell;

        use bmux_tui::buffer::Buffer;
        use bmux_tui::component::{Component, Constraints, LayoutCx};
        use bmux_tui::frame::Frame;
        use bmux_tui::geometry::{Point, Rect};
        use bmux_tui::paint::PaintCx;

        use crate::button::{ButtonComponent, ButtonState};
        use crate::common::InteractionState;

        let theme = ComponentTheme {
            text: Style::new().fg(Color::Rgb(1, 1, 1)),
            canvas: Style::new().bg(Color::Rgb(2, 2, 2)),
            surfaces: ComponentSurfaces {
                normal: Style::new().bg(Color::Rgb(2, 2, 2)),
                raised: Style::new().bg(Color::Rgb(12, 12, 12)),
                overlay: Style::new().bg(Color::Rgb(13, 13, 13)),
                scrim: None,
            },
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
        let normal = theme.for_surface(super::ComponentSurfaceDepth::Normal);
        let states = [
            ("normal", InteractionState::new(), normal.text),
            (
                "hovered",
                InteractionState {
                    hovered: true,
                    ..InteractionState::new()
                },
                normal.info,
            ),
            (
                "focused",
                InteractionState {
                    focused: true,
                    ..InteractionState::new()
                },
                normal.focused,
            ),
            (
                "pressed",
                InteractionState {
                    pressed: true,
                    ..InteractionState::new()
                },
                normal.selected,
            ),
            (
                "disabled",
                InteractionState {
                    disabled: true,
                    ..InteractionState::new()
                },
                normal.disabled,
            ),
            (
                "disabled-precedence",
                InteractionState {
                    focused: true,
                    hovered: true,
                    pressed: true,
                    disabled: true,
                },
                normal.disabled,
            ),
        ];

        for (label, interaction, expected) in states {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
            let mut frame = Frame::new(&mut buffer);
            let state = Cell::new(ButtonState { interaction });
            let button = ButtonComponent::new("theme.action", "Action", &state)
                .styles(theme.button_styles());
            let layout = button.layout(Constraints::for_width(12), &mut LayoutCx::new());
            button.paint(&layout, &mut PaintCx::new(&mut frame));
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
        assert_eq!(checkbox.normal, normal.text);
        assert_eq!(checkbox.hovered, normal.info);
        assert_eq!(checkbox.focused, normal.focused);
        assert_eq!(checkbox.pressed, normal.selected);
        assert_eq!(checkbox.disabled, normal.disabled);

        let badge = theme.badge_styles();
        assert_eq!(badge.info.fg, theme.info.fg);
        assert_eq!(badge.success.fg, theme.success.fg);
        assert_eq!(badge.warning.fg, theme.warning.fg);
        assert_eq!(badge.error.fg, theme.error.fg);
        assert_eq!(theme.empty_state_styles().body, normal.muted);
        assert_eq!(theme.progress_bar_styles().indeterminate, normal.info);
    }
}
