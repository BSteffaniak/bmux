//! Visible text-input box composition around [`TextInputState`].

use std::cell::RefCell;

use bmux_tui::chrome::Border;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode,
};
use bmux_tui::composition::{SizeBox, Surface};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Insets;
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Color, Modifier, Style};

use crate::form_field::FormFieldComponent;
use crate::text_input::{TextInputComponent, TextInputPolicy, TextInputState};

/// Behavior policy for [`TextInputBox`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TextInputBoxPolicy {
    /// Render label/help/error field chrome when configured.
    pub field_chrome: bool,
    /// Render a panel around the text content.
    pub panel_chrome: bool,
    /// Fill the control background before rendering text.
    pub background: bool,
    /// Render terminal cursor via the underlying text widget when focused.
    pub cursor: bool,
    /// Whether this input is focused.
    pub focused: bool,
    /// Whether input handling is disabled at this box layer.
    pub disabled: bool,
    /// Minimum content rows to reserve.
    pub min_rows: u16,
    /// Maximum content rows to reserve.
    pub max_rows: Option<u16>,
}

impl TextInputBoxPolicy {
    /// Bare text input: no field chrome, no panel, no background.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            field_chrome: false,
            panel_chrome: false,
            background: false,
            cursor: true,
            focused: false,
            disabled: false,
            min_rows: 1,
            max_rows: None,
        }
    }

    /// Visible text field with background and border chrome.
    #[must_use]
    pub const fn field() -> Self {
        Self {
            field_chrome: false,
            panel_chrome: true,
            background: true,
            cursor: true,
            focused: false,
            disabled: false,
            min_rows: 1,
            max_rows: None,
        }
    }

    /// Labeled field with visible text field chrome.
    #[must_use]
    pub const fn labeled_field() -> Self {
        Self {
            field_chrome: true,
            panel_chrome: true,
            background: true,
            cursor: true,
            focused: false,
            disabled: false,
            min_rows: 1,
            max_rows: None,
        }
    }

    /// Return this policy with focus updated.
    #[must_use]
    pub const fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Return this policy with disabled state updated.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Return this policy with content row bounds updated.
    #[must_use]
    pub const fn rows(mut self, min_rows: u16, max_rows: Option<u16>) -> Self {
        self.min_rows = min_rows;
        self.max_rows = max_rows;
        self
    }
}

impl Default for TextInputBoxPolicy {
    fn default() -> Self {
        Self::field()
    }
}

/// Visual styles for [`TextInputBox`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextInputBoxStyles {
    /// Text style when enabled and unfocused.
    pub text: Style,
    /// Text style when focused.
    pub focused_text: Style,
    /// Text style when disabled.
    pub disabled_text: Style,
    /// Placeholder style.
    pub placeholder: Style,
    /// Selection style.
    pub selection: Style,
    /// Panel border style when enabled and unfocused.
    pub border: Style,
    /// Panel border style when focused.
    pub focused_border: Style,
    /// Background style when enabled and unfocused.
    pub background: Style,
    /// Background style when focused.
    pub focused_background: Style,
    /// Background style when disabled.
    pub disabled_background: Style,
}

impl Default for TextInputBoxStyles {
    fn default() -> Self {
        Self {
            text: Style::new().fg(Color::BrightWhite),
            focused_text: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            disabled_text: Style::new().fg(Color::BrightBlack),
            placeholder: Style::new().fg(Color::BrightBlack),
            selection: Style::new().fg(Color::Black).bg(Color::Yellow),
            border: Style::new().fg(Color::BrightBlack),
            focused_border: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            background: Style::new(),
            focused_background: Style::new().bg(Color::Black),
            disabled_background: Style::new().bg(Color::Black),
        }
    }
}

/// Configurable visible text-input wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInputBox<'a> {
    label: Option<&'a str>,
    required: bool,
    help: Option<&'a str>,
    error: Option<&'a str>,
    placeholder: Option<&'a str>,
    policy: TextInputBoxPolicy,
    text_policy: TextInputPolicy,
    styles: TextInputBoxStyles,
}

/// Canonical component-lifecycle text input box.
pub struct TextInputBoxComponent<'a, 'state> {
    id: LayoutId,
    box_control: TextInputBox<'a>,
    state: &'state RefCell<TextInputState>,
}

impl<'a, 'state> TextInputBoxComponent<'a, 'state> {
    /// Create a text input box with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        text_policy: TextInputPolicy,
        state: &'state RefCell<TextInputState>,
    ) -> Self {
        Self {
            id: id.into(),
            box_control: TextInputBox::new(text_policy),
            state,
        }
    }

    /// Set optional label.
    #[must_use]
    pub const fn label(mut self, label: &'a str) -> Self {
        self.box_control.label = Some(label);
        self
    }

    /// Set required marker visibility.
    #[must_use]
    pub const fn required(mut self, required: bool) -> Self {
        self.box_control.required = required;
        self
    }

    /// Set optional help text.
    #[must_use]
    pub const fn help(mut self, help: &'a str) -> Self {
        self.box_control.help = Some(help);
        self
    }

    /// Set optional error text.
    #[must_use]
    pub const fn error(mut self, error: &'a str) -> Self {
        self.box_control.error = Some(error);
        self
    }

    /// Set placeholder text.
    #[must_use]
    pub const fn placeholder(mut self, placeholder: &'a str) -> Self {
        self.box_control.placeholder = Some(placeholder);
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TextInputBoxPolicy) -> Self {
        self.box_control.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TextInputBoxStyles) -> Self {
        self.box_control.styles = styles;
        self
    }

    fn tree(&self) -> bmux_tui::component::Element<'_> {
        let input = TextInputComponent::new(
            format!("{}.input", self.id.as_str()),
            self.state,
            &self.box_control.text_policy,
        )
        .style(self.box_control.text_style())
        .selection_style(self.box_control.styles.selection)
        .focused(self.box_control.policy.cursor && self.box_control.policy.focused)
        .disabled(self.box_control.policy.disabled);
        let input = if let Some(placeholder) = self.box_control.placeholder {
            input.placeholder(placeholder, self.box_control.styles.placeholder)
        } else {
            input
        };
        let bounded = SizeBox::new(input)
            .id(format!("{}.rows", self.id.as_str()))
            .min_height(usize::from(self.box_control.policy.min_rows))
            .max_height(usize::from(
                self.box_control.policy.max_rows.unwrap_or(u16::MAX),
            ));
        let control = if self.box_control.policy.panel_chrome {
            let mut surface = Surface::new(bounded)
                .id(format!("{}.surface", self.id.as_str()))
                .padding(Insets::new(0, 1, 0, 1));
            if self.box_control.policy.background {
                surface = surface.background(self.box_control.background_style());
            }
            surface = surface.border(Border::single().style(if self.box_control.policy.focused {
                self.box_control.styles.focused_border
            } else {
                self.box_control.styles.border
            }));
            bmux_tui::component::Element::new(surface)
        } else {
            bmux_tui::component::Element::new(bounded)
        };
        if self.box_control.policy.field_chrome {
            let mut field = FormFieldComponent::new(
                self.id.clone(),
                self.box_control.label.unwrap_or_default(),
                control,
            )
            .required(self.box_control.required);
            if let Some(help) = self.box_control.help {
                field = field.help(help);
            }
            if let Some(error) = self.box_control.error {
                field = field.error(error);
            }
            bmux_tui::component::Element::new(field)
        } else {
            control
        }
    }
}

impl Component for TextInputBoxComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        self.tree().revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree().layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree().paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.tree().event(event, layout, cx)
    }
}

impl<'a> TextInputBox<'a> {
    /// Create a text input box using the supplied text-edit policy.
    #[must_use]
    pub fn new(text_policy: TextInputPolicy) -> Self {
        Self {
            label: None,
            required: false,
            help: None,
            error: None,
            placeholder: None,
            policy: TextInputBoxPolicy::default(),
            text_policy,
            styles: TextInputBoxStyles::default(),
        }
    }

    /// Set optional label.
    #[must_use]
    pub const fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// Set required marker visibility for labeled fields.
    #[must_use]
    pub const fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Set optional help text.
    #[must_use]
    pub const fn help(mut self, help: &'a str) -> Self {
        self.help = Some(help);
        self
    }

    /// Set optional error text.
    #[must_use]
    pub const fn error(mut self, error: &'a str) -> Self {
        self.error = Some(error);
        self
    }

    /// Set placeholder text.
    #[must_use]
    pub const fn placeholder(mut self, placeholder: &'a str) -> Self {
        self.placeholder = Some(placeholder);
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TextInputBoxPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TextInputBoxStyles) -> Self {
        self.styles = styles;
        self
    }

    const fn background_style(&self) -> Style {
        if self.policy.disabled {
            self.styles.disabled_background
        } else if self.policy.focused {
            self.styles.focused_background
        } else {
            self.styles.background
        }
    }

    const fn text_style(&self) -> Style {
        if self.policy.disabled {
            self.styles.disabled_text
        } else if self.policy.focused {
            self.styles.focused_text
        } else {
            self.styles.text
        }
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`TextInputBoxStyles`].
    #[must_use]
    pub fn text_input_box_styles(self) -> TextInputBoxStyles {
        TextInputBoxStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for TextInputBoxStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            text: theme.text,
            focused_text: theme.text.add_modifier(bmux_tui::style::Modifier::BOLD),
            disabled_text: theme.disabled,
            placeholder: theme.muted,
            selection: theme.selected,
            border: theme.border,
            focused_border: theme.focused,
            background: theme.surfaces.normal,
            focused_background: theme.surfaces.normal,
            disabled_background: theme.surfaces.normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use bmux_text_edit::TextEditBuffer;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
    use bmux_tui::event::{Event, EventOutcome};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::paint::PaintCx;

    use super::{TextInputBoxComponent, TextInputBoxPolicy};
    use crate::text_input::{TextInputPolicy, TextInputState};

    fn paint(
        component: &impl Component,
        width: u16,
        height: u16,
    ) -> (Buffer, bmux_tui::component::LayoutNode) {
        let layout = component.layout(
            Constraints::tight(bmux_tui::geometry::Size::new(width, height)),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        component.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
        (buffer, layout)
    }

    #[test]
    fn component_renders_bare_text_input() {
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text("Ada")));
        let component =
            TextInputBoxComponent::new("name", TextInputPolicy::chat_composer(), &state)
                .policy(TextInputBoxPolicy::bare().focused(true));

        let (buffer, _) = paint(&component, 8, 1);

        assert_eq!(buffer.row_symbols(0).as_deref(), Some("Ada     "));
        assert_eq!(state.borrow().content_area(), Rect::new(0, 0, 8, 1));
    }

    #[test]
    fn component_renders_visible_focused_field() {
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text("Ada")));
        let component =
            TextInputBoxComponent::new("name", TextInputPolicy::chat_composer(), &state)
                .policy(TextInputBoxPolicy::field().focused(true));

        let (buffer, _) = paint(&component, 12, 3);

        assert_eq!(buffer.row_symbols(0).as_deref(), Some("┌──────────┐"));
        assert_eq!(buffer.row_symbols(1).as_deref(), Some("│ Ada      │"));
        assert_eq!(state.borrow().content_area(), Rect::new(0, 0, 8, 1));
    }

    #[test]
    fn component_composes_field_surface_and_input_through_runtime() {
        let state = RefCell::new(TextInputState::default());
        let policy = TextInputPolicy::chat_composer();
        let component = TextInputBoxComponent::new("composer", policy, &state)
            .label("Message")
            .required(true)
            .help("Markdown supported")
            .placeholder("Type a message")
            .policy(
                TextInputBoxPolicy::labeled_field()
                    .focused(true)
                    .rows(2, Some(2)),
            );
        let (_, layout) = paint(&component, 24, 8);
        assert!(layout.find(&"composer".into()).is_some());
        assert!(layout.find(&"composer.surface".into()).is_some());
        assert!(layout.find(&"composer.input".into()).is_some());
        assert_eq!(
            component.event(
                &Event::Paste("hello".to_owned()),
                &layout,
                &mut EventCx::new(&layout),
            ),
            EventOutcome::Redraw
        );
        assert_eq!(state.borrow().buffer().text(), "hello");
    }

    #[test]
    fn disabled_component_ignores_events() {
        let state = RefCell::new(TextInputState::default());
        let component =
            TextInputBoxComponent::new("disabled", TextInputPolicy::chat_composer(), &state)
                .policy(TextInputBoxPolicy::field().disabled(true));
        let (_, layout) = paint(&component, 12, 3);

        assert_eq!(
            component.event(
                &Event::Paste("ignored".to_owned()),
                &layout,
                &mut EventCx::new(&layout),
            ),
            EventOutcome::Ignored
        );
        assert!(state.borrow().buffer().is_empty());
    }

    #[test]
    fn component_respects_max_content_rows() {
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text(
            "one\ntwo\nthree",
        )));
        let component =
            TextInputBoxComponent::new("bounded", TextInputPolicy::chat_composer(), &state)
                .policy(TextInputBoxPolicy::field().rows(1, Some(2)));

        let (_, layout) = paint(&component, 16, 8);
        let input = layout.find(&"bounded.input".into()).expect("input layout");

        assert_eq!(input.size.height, 2);
        assert_eq!(state.borrow().content_area().height, 2);
    }
}
