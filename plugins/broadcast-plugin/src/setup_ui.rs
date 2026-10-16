//! Inline setup field, sharing BMUX terminal/input lifecycle with the picker.
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::{
    buffer::Buffer,
    component::{Component, Constraints, LayoutCx},
    composition::{Column, TextBlock},
    event::Event,
    frame::Frame,
    geometry::{Rect, Size},
    inline::InlineTerminal,
    paint::PaintCx,
    style::{Color, Style},
};
use bmux_tui_components::text_input::{
    TextInputComponent, TextInputControl, TextInputPolicy, TextInputState,
};
use bmux_tui_runtime::{
    PresentReport, Presenter, Program, ResetReason, Runtime, RuntimeConfig, RuntimeError,
    RuntimeEvent, TerminalInput, Update,
};
use std::{
    cell::RefCell,
    io::{self, IsTerminal},
};
struct Field {
    label: String,
    state: RefCell<TextInputState>,
    size: Size,
    result: Option<String>,
}
impl Program for Field {
    type Message = io::Error;
    type Error = io::Error;
    fn update(&mut self, event: RuntimeEvent<io::Error>) -> io::Result<Update<io::Error>> {
        match event {
            RuntimeEvent::Message(error) => return Err(error),
            RuntimeEvent::Terminal(Event::Resize(size)) => self.size = size,
            RuntimeEvent::Terminal(Event::Key(key))
                if key.key == KeyCode::Escape
                    || (key.key == KeyCode::Char('c') && key.modifiers.ctrl) =>
            {
                return Ok(Update::exit());
            }
            RuntimeEvent::Terminal(Event::Key(key)) if key.key == KeyCode::Enter => {
                self.result = Some(self.state.borrow().buffer().text().trim().to_owned());
                return Ok(Update::exit());
            }
            RuntimeEvent::Terminal(event) => {
                let length = self.state.borrow().buffer().text().len();
                if matches!(&event,Event::Paste(text) if length.saturating_add(text.len())>4096)
                    || matches!(&event,Event::Key(key) if matches!(key.key,KeyCode::Char(_)) && !key.modifiers.ctrl && length>=4096)
                {
                    return Ok(Update::none());
                }
                TextInputControl::new(&TextInputPolicy::raw())
                    .handle_event(&mut self.state.borrow_mut(), &event);
            }
            _ => return Ok(Update::none()),
        }
        Ok(Update::redraw())
    }
}
struct FieldPresenter {
    terminal: InlineTerminal<io::Stderr>,
}
impl Presenter<Field> for FieldPresenter {
    type Error = io::Error;
    fn reset(&mut self, _: ResetReason) {}
    fn present(&mut self, field: &mut Field) -> io::Result<PresentReport> {
        let width = field.size.width.saturating_sub(1).min(90);
        let height = field.size.height.saturating_sub(1).min(6);
        let policy = TextInputPolicy::raw();
        let root = Column::new()
            .child(TextBlock::new("Broadcast · Setup").style(Style::new().fg(Color::Cyan)))
            .child(TextBlock::new(field.label.as_str()))
            .child(TextInputComponent::new("value", &field.state, &policy).focused(true))
            .child(TextBlock::new("Enter continue · Esc cancel"));
        let layout = root.layout(
            Constraints::tight(Size::new(width, height)),
            &mut LayoutCx::default(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        root.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
        self.terminal.draw(&buffer)?;
        Ok(PresentReport::default())
    }
}
pub(super) async fn text(label: &str) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(io::Error::other(
            "interactive setup requires a TTY; use explicit setup flags",
        ));
    }
    let (width, height) = crossterm::terminal::size()?;
    let presenter = FieldPresenter {
        terminal: InlineTerminal::enter(io::stderr())?,
    };
    let field = Field {
        label: label.into(),
        state: RefCell::new(TextInputState::new(TextEditBuffer::new())),
        size: Size::new(width, height),
        result: None,
    };
    let (runtime, handle) = Runtime::new(field, presenter, RuntimeConfig::default());
    let mut input = TerminalInput::start::<Field>(handle, |error| error);
    let result = runtime.run().await;
    input.shutdown().await;
    let mut output = match result {
        Ok(output) => output,
        Err(RuntimeError::Program { error, .. } | RuntimeError::Presenter { error, .. }) => {
            return Err(error);
        }
    };
    output.presenter.terminal.finish()?;
    Ok(output.program.result)
}
