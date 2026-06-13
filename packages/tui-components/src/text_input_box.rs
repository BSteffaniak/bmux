//! Visible text-input box composition around [`TextInputState`].

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::widget::Widget;

use crate::form_field::FormField;
use crate::text_input::{TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState};

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

/// Layout produced by [`TextInputBox`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextInputBoxLayout {
    /// Full outer area passed to the component.
    pub outer: Rect,
    /// Area used by optional field chrome.
    pub field_control: Rect,
    /// Area used by optional panel chrome.
    pub panel: Rect,
    /// Editable text content area.
    pub content: Rect,
}

/// Outcome from [`TextInputBox`] event handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputBoxOutcome {
    /// Event was ignored.
    Ignored,
    /// Text or cursor/selection changed.
    Edited,
    /// Redraw requested.
    Redraw,
    /// Submit was requested.
    Submitted,
    /// Up edge reached.
    EdgeUp,
    /// Down edge reached.
    EdgeDown,
}

impl From<TextInputOutcome> for TextInputBoxOutcome {
    fn from(value: TextInputOutcome) -> Self {
        match value {
            TextInputOutcome::Ignored => Self::Ignored,
            TextInputOutcome::Edited => Self::Edited,
            TextInputOutcome::Redraw => Self::Redraw,
            TextInputOutcome::Submitted => Self::Submitted,
            TextInputOutcome::EdgeUp => Self::EdgeUp,
            TextInputOutcome::EdgeDown => Self::EdgeDown,
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

    /// Return layout for `area`.
    #[must_use]
    pub fn layout(&self, area: Rect) -> TextInputBoxLayout {
        let field_control = if self.policy.field_chrome {
            self.form_field().layout(area).control
        } else {
            area
        };
        let panel = field_control;
        let content = if self.policy.panel_chrome {
            self.panel().inner_area(panel)
        } else {
            panel
        };
        TextInputBoxLayout {
            outer: area,
            field_control,
            panel,
            content,
        }
    }

    /// Render the text input box and update state's content area.
    pub fn render(&self, area: Rect, state: &mut TextInputState, frame: &mut Frame<'_>) {
        let field_control = if self.policy.field_chrome {
            self.form_field().render(area, frame)
        } else {
            area
        };
        let panel = self.panel();
        let content = if self.policy.panel_chrome {
            panel.render(field_control, frame);
            panel.inner_area(field_control)
        } else {
            field_control
        };
        if self.policy.background {
            frame.fill(content, " ", self.background_style());
        }
        state.set_content_area(content, &self.text_policy);
        let mut input = TextInput::new(state.buffer())
            .style(self.text_style())
            .selection_style(self.styles.selection)
            .placeholder_style(self.styles.placeholder)
            .cursor_visible(self.policy.cursor && self.policy.focused && !self.policy.disabled)
            .vertical_scroll(state.vertical_scroll());
        if let Some(placeholder) = self.placeholder {
            input = input.placeholder(Line::from_spans([Span::styled(
                placeholder,
                self.styles.placeholder,
            )]));
        }
        input.render(content, frame);
    }

    /// Handle one event after updating state's content area for `area`.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut TextInputState,
        event: &Event,
    ) -> TextInputBoxOutcome {
        if self.policy.disabled {
            return TextInputBoxOutcome::Ignored;
        }
        state.set_content_area(self.layout(area).content, &self.text_policy);
        TextInputControl::new(&self.text_policy)
            .handle_event(state, event)
            .into()
    }

    fn form_field(&self) -> FormField<'a> {
        let mut field = FormField::new(self.label.unwrap_or_default()).required(self.required);
        if let Some(help) = self.help {
            field = field.help(help);
        }
        if let Some(error) = self.error {
            field = field.error(error);
        }
        field
    }

    const fn panel(&self) -> Panel {
        let mut panel = Panel::new();
        if self.policy.panel_chrome {
            panel = panel.border(Border::single().style(if self.policy.focused {
                self.styles.focused_border
            } else {
                self.styles.border
            }));
        }
        if self.policy.background {
            panel = panel.background(self.background_style());
        }
        panel.padding(Insets::new(0, 1, 0, 1))
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

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
    use bmux_text_edit::TextEditBuffer;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::{TextInputBox, TextInputBoxOutcome, TextInputBoxPolicy};
    use crate::text_input::{TextInputPolicy, TextInputState};

    #[test]
    fn renders_bare_text_input() {
        let policy = TextInputPolicy::chat_composer();
        let mut state = TextInputState::new(TextEditBuffer::from_text("Ada"));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        TextInputBox::new(policy)
            .policy(TextInputBoxPolicy::bare().focused(true))
            .render(Rect::new(0, 0, 8, 1), &mut state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("Ada     "));
        assert_eq!(state.content_area(), Rect::new(0, 0, 8, 1));
    }

    #[test]
    fn renders_visible_focused_field() {
        let policy = TextInputPolicy::chat_composer();
        let mut state = TextInputState::new(TextEditBuffer::from_text("Ada"));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        let mut frame = Frame::new(&mut buffer);

        TextInputBox::new(policy)
            .policy(TextInputBoxPolicy::field().focused(true))
            .render(Rect::new(0, 0, 12, 3), &mut state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("┌──────────┐")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("│ Ada      │")
        );
        assert_eq!(state.content_area(), Rect::new(2, 1, 8, 1));
    }

    #[test]
    fn renders_placeholder_when_empty() {
        let policy = TextInputPolicy::chat_composer();
        let mut state = TextInputState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 3));
        let mut frame = Frame::new(&mut buffer);

        TextInputBox::new(policy)
            .placeholder("Name")
            .policy(TextInputBoxPolicy::field())
            .render(Rect::new(0, 0, 14, 3), &mut state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("│ Name       │")
        );
    }

    #[test]
    fn handles_uppercase_input_through_text_control() {
        let policy = TextInputPolicy::chat_composer();
        let mut state = TextInputState::new(TextEditBuffer::from_text("Ada"));
        let outcome = TextInputBox::new(policy).handle_event(
            Rect::new(0, 0, 12, 3),
            &mut state,
            &Event::Key(KeyStroke::with_modifiers(
                KeyCode::Char('b'),
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            )),
        );

        assert_eq!(outcome, TextInputBoxOutcome::Edited);
        assert_eq!(state.buffer().text(), "AdaB");
    }

    #[test]
    fn mouse_click_uses_box_content_area() {
        let policy = TextInputPolicy::chat_composer();
        let mut state = TextInputState::new(TextEditBuffer::from_text("Ada"));
        let outcome = TextInputBox::new(policy).handle_event(
            Rect::new(0, 0, 12, 3),
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(2, 1),
            )),
        );

        assert_eq!(outcome, TextInputBoxOutcome::Redraw);
        assert_eq!(state.content_area(), Rect::new(2, 1, 8, 1));
    }
}
