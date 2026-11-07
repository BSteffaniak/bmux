//! Neutral opt-in theme bundle for component style defaults.
//!
//! Component-specific style structs remain the source of truth for precise
//! caller control. This module only provides ergonomic conversions from a small
//! shared palette into common component style structs.

use bmux_color::{Rgb, ensure_contrast, mix};
use bmux_tui::chrome::{Border, BorderSet};
use bmux_tui::gradient::ColorGradient;
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

/// Optional background tints for semantic states.
///
/// Badges, toasts, banners, and status segments patch these beneath their state
/// foreground. Empty styles leave the surface unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentStateSurfaces {
    /// Informational surface.
    pub info: Style,
    /// Success surface.
    pub success: Style,
    /// Warning surface.
    pub warning: Style,
    /// Error surface.
    pub error: Style,
}

impl ComponentStateSurfaces {
    /// State surfaces that leave every surface unchanged.
    pub const NONE: Self = Self {
        info: Style::new(),
        success: Style::new(),
        warning: Style::new(),
        error: Style::new(),
    };
}

/// Shape of bordered components, independent of their colors.
///
/// Bordered components (panes, picker frames, modal frames, text input boxes)
/// accept chrome through their `chrome` builder. Empty chrome keeps every
/// component's own default glyphs and focus styling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ComponentChrome {
    /// Border glyphs; `None` keeps each component's default set.
    pub border_set: Option<BorderSet>,
    /// Foreground gradient for focused borders; `None` uses the focused border style.
    pub focus_gradient: Option<ColorGradient>,
}

impl ComponentChrome {
    /// Chrome that leaves every component's defaults unchanged.
    pub const DEFAULT: Self = Self {
        border_set: None,
        focus_gradient: None,
    };

    /// Build a border from this chrome.
    ///
    /// `default_set` applies when no glyph set is configured. The focus
    /// gradient applies only when `focused` is true.
    #[must_use]
    pub fn border(self, default_set: BorderSet, style: Style, focused: bool) -> Border {
        let border = Border::new(self.border_set.unwrap_or(default_set), style);
        match self.focus_gradient {
            Some(gradient) if focused => border.gradient(gradient),
            _ => border,
        }
    }
}

/// Seed colors from which [`ComponentTheme::derive`] builds a complete theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentThemeSeed {
    /// Canvas background.
    pub background: Rgb,
    /// Primary text.
    pub foreground: Rgb,
    /// Interactive accent (focus, titles, hover).
    pub accent: Rgb,
    /// Informational state hue.
    pub info: Rgb,
    /// Success state hue.
    pub success: Rgb,
    /// Warning state hue.
    pub warning: Rgb,
    /// Error state hue.
    pub error: Rgb,
}

/// Minimum contrast for derived primary text, accents, and state foregrounds.
const DERIVED_TEXT_CONTRAST: f64 = 4.5;
/// Minimum contrast for derived secondary text.
const DERIVED_MUTED_CONTRAST: f64 = 3.0;

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
    /// Primary interactive accent, used for current/active items.
    pub accent: Style,
    /// Panel, dialog, and column titles.
    pub title: Style,
    /// Focused interactive style. Should carry a non-color cue.
    pub focused: Style,
    /// Hovered interactive style.
    pub hover: Style,
    /// Selected interactive style.
    pub selected: Style,
    /// Selection in a control that does not currently have focus.
    pub selected_inactive: Style,
    /// Disabled/muted unavailable style.
    pub disabled: Style,
    /// Muted metadata/separator style.
    pub muted: Style,
    /// Hyperlink style.
    pub link: Style,
    /// Informational accent style.
    pub info: Style,
    /// Success accent style.
    pub success: Style,
    /// Warning accent style.
    pub warning: Style,
    /// Error accent style.
    pub error: Style,
    /// Background tints for semantic states.
    pub state_surfaces: ComponentStateSurfaces,
    /// Border/separator style.
    pub border: Style,
    /// Border of the focused panel or control.
    pub border_focused: Style,
    /// Border glyphs and focus effects for bordered components.
    pub chrome: ComponentChrome,
}

impl ComponentTheme {
    /// Return the default neutral BMUX component theme.
    #[must_use]
    pub const fn bmux_default() -> Self {
        let info = Style::new().fg(Color::Cyan);
        let focused = Style::new()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::UNDERLINE);
        let selected = Style::new().fg(Color::Black).bg(Color::Cyan);
        Self {
            canvas: Style::new(),
            surfaces: ComponentSurfaces {
                normal: Style::new(),
                raised: Style::new(),
                overlay: Style::new().bg(Color::Black),
                scrim: None,
            },
            text: Style::new().fg(Color::White),
            accent: info,
            title: info.add_modifier(Modifier::BOLD),
            focused,
            hover: info,
            selected,
            selected_inactive: selected,
            disabled: Style::new()
                .fg(Color::BrightBlack)
                .add_modifier(Modifier::DIM),
            muted: Style::new().fg(Color::BrightBlack),
            link: info.add_modifier(Modifier::UNDERLINE),
            info,
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            state_surfaces: ComponentStateSurfaces::NONE,
            border: Style::new().fg(Color::BrightBlack),
            border_focused: focused,
            chrome: ComponentChrome::DEFAULT,
        }
    }

    /// Return an opaque dark semantic theme suitable for component galleries
    /// and applications that do not supply their own palette.
    #[must_use]
    pub const fn opaque_dark() -> Self {
        let info = Style::new().fg(Color::Cyan);
        let focused = Style::new()
            .fg(Color::BrightCyan)
            .add_modifier(Modifier::UNDERLINE);
        let selected = Style::new().fg(Color::Black).bg(Color::Cyan);
        Self {
            canvas: Style::new().bg(Color::Black),
            surfaces: ComponentSurfaces {
                normal: Style::new().bg(Color::Black),
                raised: Style::new().bg(Color::Rgb(24, 24, 27)),
                overlay: Style::new().bg(Color::Rgb(39, 39, 42)),
                scrim: Some(Style::new().bg(Color::Black)),
            },
            text: Style::new().fg(Color::BrightWhite),
            accent: info,
            title: info.add_modifier(Modifier::BOLD),
            focused,
            hover: info,
            selected,
            selected_inactive: selected,
            disabled: Style::new()
                .fg(Color::BrightBlack)
                .add_modifier(Modifier::DIM),
            muted: Style::new().fg(Color::BrightBlack),
            link: info.add_modifier(Modifier::UNDERLINE),
            info,
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            state_surfaces: ComponentStateSurfaces::NONE,
            border: Style::new().fg(Color::BrightBlack),
            border_focused: focused,
            chrome: ComponentChrome::DEFAULT,
        }
    }

    /// Return an opaque light semantic theme suitable for component galleries
    /// and applications that do not supply their own palette.
    #[must_use]
    pub const fn opaque_light() -> Self {
        let info = Style::new().fg(Color::Blue);
        let focused = Style::new()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::UNDERLINE);
        let selected = Style::new().fg(Color::White).bg(Color::Blue);
        Self {
            canvas: Style::new().bg(Color::White),
            surfaces: ComponentSurfaces {
                normal: Style::new().bg(Color::White),
                raised: Style::new().bg(Color::Rgb(244, 244, 245)),
                overlay: Style::new().bg(Color::Rgb(228, 228, 231)),
                scrim: Some(Style::new().bg(Color::BrightBlack)),
            },
            text: Style::new().fg(Color::Black),
            accent: info,
            title: info.add_modifier(Modifier::BOLD),
            focused,
            hover: info,
            selected,
            selected_inactive: selected,
            disabled: Style::new()
                .fg(Color::BrightBlack)
                .add_modifier(Modifier::DIM),
            muted: Style::new().fg(Color::BrightBlack),
            link: info.add_modifier(Modifier::UNDERLINE),
            info,
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            state_surfaces: ComponentStateSurfaces::NONE,
            border: Style::new().fg(Color::BrightBlack),
            border_focused: focused,
            chrome: ComponentChrome::DEFAULT,
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
        let info = Style::new().fg(Color::Cyan);
        let focused = Style::new()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::UNDERLINE);
        let selected = Style::new().add_modifier(Modifier::REVERSED);
        Self {
            canvas: Style::new().bg(Color::Default),
            surfaces: ComponentSurfaces {
                normal: Style::new().bg(Color::Default),
                raised: Style::new().bg(Color::Default),
                overlay: Style::new().bg(Color::Default),
                scrim: None,
            },
            text: Style::new().fg(Color::Default),
            accent: info,
            title: info.add_modifier(Modifier::BOLD),
            focused,
            hover: info,
            selected,
            selected_inactive: selected,
            disabled: Style::new().add_modifier(Modifier::DIM),
            muted: Style::new().add_modifier(Modifier::DIM),
            link: info.add_modifier(Modifier::UNDERLINE),
            info,
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            state_surfaces: ComponentStateSurfaces::NONE,
            border: Style::new().add_modifier(Modifier::DIM),
            border_focused: focused,
            chrome: ComponentChrome::DEFAULT,
        }
    }

    /// Derive a complete opaque theme from seed colors.
    ///
    /// Surfaces, borders, muted text, selection, and state tints are mixed from
    /// the seeds in OKLab. Text, accents, and state foregrounds are adjusted in
    /// lightness only as far as needed to stay readable on the canvas (4.5:1;
    /// 3:1 for muted text), so light and dark seeds both produce legible
    /// themes. Focus keeps an underline so it never depends on color alone.
    #[must_use]
    pub fn derive(seed: ComponentThemeSeed) -> Self {
        let background = seed.background;
        let readable = |color| ensure_contrast(color, background, DERIVED_TEXT_CONTRAST);
        let fg = |color: Rgb| Style::new().fg(Color::from(color));
        let bg = |color: Rgb| Style::new().bg(Color::from(color));

        let text = readable(seed.foreground);
        let accent = readable(seed.accent);
        let selection = mix(seed.accent, background, 0.30);
        let inactive_selection = mix(seed.foreground, background, 0.12);
        let state_surface = |hue| bg(mix(hue, background, 0.16));

        Self {
            canvas: fg(text).bg(Color::from(background)),
            surfaces: ComponentSurfaces {
                normal: bg(background),
                raised: bg(mix(seed.foreground, background, 0.05)),
                overlay: bg(mix(seed.foreground, background, 0.09)),
                scrim: None,
            },
            text: fg(text),
            accent: fg(accent),
            title: fg(accent).add_modifier(Modifier::BOLD),
            focused: fg(accent).add_modifier(Modifier::UNDERLINE),
            hover: fg(accent),
            selected: fg(ensure_contrast(text, selection, DERIVED_TEXT_CONTRAST))
                .bg(Color::from(selection)),
            selected_inactive: fg(ensure_contrast(
                text,
                inactive_selection,
                DERIVED_TEXT_CONTRAST,
            ))
            .bg(Color::from(inactive_selection)),
            disabled: fg(mix(seed.foreground, background, 0.38)),
            muted: fg(ensure_contrast(
                mix(seed.foreground, background, 0.60),
                background,
                DERIVED_MUTED_CONTRAST,
            )),
            link: fg(readable(seed.info)).add_modifier(Modifier::UNDERLINE),
            info: fg(readable(seed.info)),
            success: fg(readable(seed.success)),
            warning: fg(readable(seed.warning)),
            error: fg(readable(seed.error)),
            state_surfaces: ComponentStateSurfaces {
                info: state_surface(seed.info),
                success: state_surface(seed.success),
                warning: state_surface(seed.warning),
                error: state_surface(seed.error),
            },
            border: fg(mix(seed.foreground, background, 0.25)),
            border_focused: fg(accent),
            chrome: ComponentChrome::DEFAULT,
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
            accent: surface.patch(self.accent),
            title: surface.patch(self.title),
            focused: surface.patch(self.focused),
            hover: surface.patch(self.hover),
            selected: surface.patch(self.selected),
            selected_inactive: surface.patch(self.selected_inactive),
            disabled: surface.patch(self.disabled),
            muted: surface.patch(self.muted),
            link: surface.patch(self.link),
            info: surface.patch(self.info),
            success: surface.patch(self.success),
            warning: surface.patch(self.warning),
            error: surface.patch(self.error),
            state_surfaces: ComponentStateSurfaces {
                info: surface.patch(self.state_surfaces.info),
                success: surface.patch(self.state_surfaces.success),
                warning: surface.patch(self.state_surfaces.warning),
                error: surface.patch(self.state_surfaces.error),
            },
            border: surface.patch(self.border),
            border_focused: surface.patch(self.border_focused),
            chrome: self.chrome,
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
            theme.hover,
            theme.selected,
            theme.disabled,
        )
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::style::Style;
    use bmux_tui::style::{Color, Modifier};

    use super::{ComponentStateSurfaces, ComponentSurfaces, ComponentTheme};

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
            accent: Style::new().fg(Color::BrightBlue),
            title: Style::new().fg(Color::BrightMagenta),
            focused: Style::new().fg(Color::Cyan),
            hover: Style::new().fg(Color::BrightCyan),
            selected: Style::new().fg(Color::Black).bg(Color::Yellow),
            selected_inactive: Style::new().bg(Color::BrightBlack),
            disabled: Style::new().add_modifier(Modifier::DIM),
            muted: Style::new().fg(Color::BrightBlack),
            link: Style::new().fg(Color::Blue),
            info: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            state_surfaces: ComponentStateSurfaces {
                error: Style::new().bg(Color::Red),
                ..ComponentStateSurfaces::NONE
            },
            border: Style::new().fg(Color::Magenta),
            border_focused: Style::new().fg(Color::BrightGreen),
            chrome: super::ComponentChrome::DEFAULT,
        };

        let raised = theme.for_surface(super::ComponentSurfaceDepth::Raised);
        assert_eq!(raised.text.bg, Some(Color::Green));
        assert_eq!(raised.text.fg, Some(Color::White));
        assert_eq!(raised.focused.bg, Some(Color::Green));
        assert_eq!(raised.selected.bg, Some(Color::Yellow));
        assert_eq!(raised.disabled.bg, Some(Color::Green));
        assert!(raised.disabled.modifiers.contains(Modifier::DIM));

        assert_eq!(raised.hover.bg, Some(Color::Green));
        assert_eq!(raised.state_surfaces.error.bg, Some(Color::Red));
        assert_eq!(raised.state_surfaces.info.bg, Some(Color::Green));

        let overlay = theme.for_surface(super::ComponentSurfaceDepth::Overlay);
        assert_eq!(overlay.border.bg, Some(Color::Red));
        assert_eq!(overlay.border_focused.bg, Some(Color::Red));
        assert_eq!(overlay.border_focused.fg, Some(Color::BrightGreen));
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
            accent: Style::new().fg(Color::BrightBlue),
            title: Style::new().fg(Color::BrightMagenta),
            focused: Style::new().fg(Color::BrightCyan),
            hover: Style::new().fg(Color::Magenta),
            selected: Style::new().fg(Color::Black).bg(Color::Cyan),
            selected_inactive: Style::new().bg(Color::BrightBlack),
            disabled: Style::new().fg(Color::BrightBlack),
            muted: Style::new().fg(Color::BrightBlack),
            link: Style::new().fg(Color::Blue),
            info: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            state_surfaces: ComponentStateSurfaces::NONE,
            border: Style::new().fg(Color::Blue),
            border_focused: Style::new().fg(Color::BrightGreen),
            chrome: super::ComponentChrome::DEFAULT,
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
        assert_eq!(theme.modal_theme().border, overlay.border_focused);
        assert_eq!(theme.modal_theme().title, overlay.title);
        assert_eq!(theme.selectable_list_styles().hovered, normal.hover);
        assert_eq!(theme.table_styles().header, normal.title);
        assert_eq!(theme.status_bar_styles().success, normal.success);
        assert_eq!(theme.toast_stack_styles().error.fg, raised.error.fg);
        assert_eq!(theme.stepper_styles().complete, normal.success);
        assert_eq!(theme.progress_bar_styles().indeterminate, normal.accent);
        assert_eq!(theme.scrollbar_styles().track, normal.border);
        assert_eq!(theme.empty_state_styles().action, normal.accent);
        assert_eq!(theme.text_view_styles().text, normal.text);
        assert_eq!(theme.chart_styles().dataset, normal.accent);
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
            accent: Style::new().fg(Color::Rgb(14, 14, 14)),
            title: Style::new().fg(Color::Rgb(15, 15, 15)),
            focused: Style::new().fg(Color::Rgb(3, 3, 3)),
            hover: Style::new().fg(Color::Rgb(16, 16, 16)),
            selected: Style::new().fg(Color::Rgb(4, 4, 4)),
            selected_inactive: Style::new().fg(Color::Rgb(17, 17, 17)),
            disabled: Style::new().fg(Color::Rgb(5, 5, 5)),
            muted: Style::new().fg(Color::Rgb(6, 6, 6)),
            link: Style::new().fg(Color::Rgb(18, 18, 18)),
            info: Style::new().fg(Color::Rgb(7, 7, 7)),
            success: Style::new().fg(Color::Rgb(8, 8, 8)),
            warning: Style::new().fg(Color::Rgb(9, 9, 9)),
            error: Style::new().fg(Color::Rgb(10, 10, 10)),
            state_surfaces: ComponentStateSurfaces::NONE,
            border: Style::new().fg(Color::Rgb(11, 11, 11)),
            border_focused: Style::new().fg(Color::Rgb(19, 19, 19)),
            chrome: super::ComponentChrome::DEFAULT,
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
                normal.hover,
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
        assert_eq!(checkbox.hovered, normal.hover);
        assert_eq!(checkbox.focused, normal.focused);
        assert_eq!(checkbox.pressed, normal.selected);
        assert_eq!(checkbox.disabled, normal.disabled);

        let badge = theme.badge_styles();
        assert_eq!(badge.info.fg, theme.info.fg);
        assert_eq!(badge.success.fg, theme.success.fg);
        assert_eq!(badge.warning.fg, theme.warning.fg);
        assert_eq!(badge.error.fg, theme.error.fg);
        assert_eq!(theme.empty_state_styles().body, normal.muted);
        assert_eq!(theme.progress_bar_styles().indeterminate, normal.accent);
    }
}

#[cfg(test)]
mod focus_visual_tests {
    use bmux_tui::style::Modifier;

    use super::ComponentTheme;

    #[test]
    fn built_in_focus_has_a_non_color_cue_distinct_from_selection() {
        for theme in [
            ComponentTheme::default(),
            ComponentTheme::opaque_dark(),
            ComponentTheme::opaque_light(),
            ComponentTheme::terminal_default(),
        ] {
            assert!(theme.focused.modifiers.contains(Modifier::UNDERLINE));
            assert!(!theme.selected.modifiers.contains(Modifier::UNDERLINE));
            assert_ne!(theme.selected, theme.focused);
        }
    }
}

#[cfg(test)]
mod derive_tests {
    use bmux_color::Rgb;
    use bmux_tui::style::{Color, Modifier, Style};

    use super::{ComponentTheme, ComponentThemeSeed};

    fn rgb(style: Style, field: fn(Style) -> Option<Color>) -> Rgb {
        field(style)
            .and_then(Color::rgb)
            .unwrap_or_else(|| panic!("derived style lacks explicit RGB: {style:?}"))
    }

    fn fg(style: Style) -> Rgb {
        rgb(style, |style| style.fg)
    }

    fn bg(style: Style) -> Rgb {
        rgb(style, |style| style.bg)
    }

    const DARK: ComponentThemeSeed = ComponentThemeSeed {
        background: Rgb::new(0x1a, 0x1b, 0x26),
        foreground: Rgb::new(0xc0, 0xca, 0xf5),
        accent: Rgb::new(0x7a, 0xa2, 0xf7),
        info: Rgb::new(0x7d, 0xcf, 0xff),
        success: Rgb::new(0x9e, 0xce, 0x6a),
        warning: Rgb::new(0xe0, 0xaf, 0x68),
        error: Rgb::new(0xf7, 0x76, 0x8e),
    };

    const LIGHT: ComponentThemeSeed = ComponentThemeSeed {
        background: Rgb::new(0xef, 0xf1, 0xf5),
        foreground: Rgb::new(0x4c, 0x4f, 0x69),
        accent: Rgb::new(0x88, 0x39, 0xef),
        info: Rgb::new(0x04, 0xa5, 0xe5),
        success: Rgb::new(0x40, 0xa0, 0x2b),
        warning: Rgb::new(0xdf, 0x8e, 0x1d),
        error: Rgb::new(0xd2, 0x0f, 0x39),
    };

    #[test]
    fn derived_text_and_states_are_readable_on_light_and_dark_seeds() {
        for seed in [DARK, LIGHT] {
            let theme = ComponentTheme::derive(seed);
            let canvas = bg(theme.canvas);
            for (role, style) in [
                ("text", theme.text),
                ("accent", theme.accent),
                ("info", theme.info),
                ("success", theme.success),
                ("warning", theme.warning),
                ("error", theme.error),
                ("link", theme.link),
            ] {
                let ratio = fg(style).contrast_ratio(canvas);
                assert!(ratio >= 4.5, "{role}: {ratio}");
            }
            assert!(fg(theme.muted).contrast_ratio(canvas) >= 3.0);
            let selected = theme.selected;
            assert!(fg(selected).contrast_ratio(bg(selected)) >= 4.5);
            let inactive = theme.selected_inactive;
            assert!(fg(inactive).contrast_ratio(bg(inactive)) >= 4.5);
        }
    }

    #[test]
    fn unreadable_seed_hues_are_corrected_toward_contrast() {
        let seed = ComponentThemeSeed {
            warning: Rgb::new(0x22, 0x22, 0x10),
            ..DARK
        };
        let theme = ComponentTheme::derive(seed);
        assert!(fg(theme.warning).contrast_ratio(seed.background) >= 4.5);
        // Readable seeds are left exactly as authored.
        assert_eq!(fg(theme.info), seed.info);
    }

    #[test]
    fn surfaces_step_away_from_the_canvas_in_depth_order() {
        for seed in [DARK, LIGHT] {
            let theme = ComponentTheme::derive(seed);
            let canvas = seed.background;
            let raised = bg(theme.surfaces.raised).contrast_ratio(canvas);
            let overlay = bg(theme.surfaces.overlay).contrast_ratio(canvas);
            assert!(1.0 < raised && raised < overlay, "{raised} {overlay}");
            assert_eq!(bg(theme.surfaces.normal), canvas);
        }
    }

    #[test]
    fn derived_focus_keeps_a_non_color_cue_and_states_have_tinted_surfaces() {
        let theme = ComponentTheme::derive(DARK);
        assert!(theme.focused.modifiers.contains(Modifier::UNDERLINE));
        assert!(!theme.selected.modifiers.contains(Modifier::UNDERLINE));
        for surface in [
            theme.state_surfaces.info,
            theme.state_surfaces.success,
            theme.state_surfaces.warning,
            theme.state_surfaces.error,
        ] {
            assert_ne!(bg(surface), DARK.background);
        }
        #[cfg(feature = "badge")]
        assert_eq!(theme.badge_styles().error.bg, theme.state_surfaces.error.bg);
    }

    #[test]
    fn chrome_keeps_component_defaults_and_applies_gradient_only_when_focused() {
        use bmux_tui::chrome::BorderSet;
        use bmux_tui::gradient::ColorGradient;

        let style = Style::new().fg(Color::Rgb(1, 2, 3));
        let plain = super::ComponentChrome::DEFAULT.border(BorderSet::SINGLE, style, true);
        assert_eq!(plain.set, BorderSet::SINGLE);
        assert_eq!(plain.gradient, None);

        let gradient = ColorGradient::new(DARK.accent, DARK.error);
        let riced = super::ComponentChrome {
            border_set: Some(BorderSet::ROUNDED),
            focus_gradient: Some(gradient),
        };
        let focused = riced.border(BorderSet::SINGLE, style, true);
        assert_eq!(focused.set, BorderSet::ROUNDED);
        assert_eq!(focused.gradient, Some(gradient));
        assert_eq!(focused.style, style);
        assert_eq!(riced.border(BorderSet::SINGLE, style, false).gradient, None);
    }
}
