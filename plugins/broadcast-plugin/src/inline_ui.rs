#[cfg(test)]
#[path = "inline_ui_tests.rs"]
mod tests;

// Caller-owned inline interaction. No alternate screen or line-oriented input.
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::{
    buffer::Buffer,
    component::{Component, Constraints, LayoutCx},
    composition::{Column, Flex, TextBlock},
    event::Event,
    frame::Frame,
    geometry::{Rect, Size},
    inline::InlineTerminal,
    paint::PaintCx,
    style::{Color, Style},
};
use bmux_tui_components::{
    key_hint_bar::{KeyHint, KeyHintBarComponent},
    selectable_list::{SelectableListComponent, SelectableListItem, SelectableListState},
    text_input::{TextInputComponent, TextInputControl, TextInputPolicy, TextInputState},
};
use bmux_tui_runtime::{
    PresentReport, Presenter, Program, ResetReason, Runtime, RuntimeConfig, RuntimeError,
    RuntimeEvent, TerminalInput, Update,
};
use std::{
    cell::{Cell, RefCell},
    io::{self, IsTerminal},
};

pub(super) struct Choice {
    pub id: String,
    pub title: String,
    pub details: String,
}

enum Message {
    Input(io::Error),
    Details { id: String, text: String },
}

struct Picker {
    choices: Vec<Choice>,
    visible: Vec<usize>,
    search: RefCell<TextInputState>,
    list: Cell<SelectableListState>,
    list_layout: RefCell<Option<bmux_tui::component::LayoutNode>>,
    size: Size,
    review: bool,
    accepted: Option<String>,
    destination: String,
}
impl Picker {
    fn new(choices: Vec<Choice>, size: Size, destination: String) -> Self {
        let visible = (0..choices.len()).collect();
        Self {
            choices,
            visible,
            search: RefCell::new(TextInputState::new(TextEditBuffer::new())),
            list: Cell::new(SelectableListState::new(Some(0))),
            list_layout: RefCell::new(None),
            size,
            review: false,
            accepted: None,
            destination,
        }
    }
    fn selected(&self) -> Option<&Choice> {
        self.list
            .get()
            .focused()
            .and_then(|index| self.visible.get(index))
            .map(|index| &self.choices[*index])
    }
    fn items(&self) -> Vec<SelectableListItem> {
        self.visible
            .iter()
            .map(|index| {
                let choice = &self.choices[*index];
                SelectableListItem::new(&choice.id, &choice.title)
            })
            .collect()
    }
    fn filter(&mut self) {
        let selected = self.selected().map(|choice| choice.id.clone());
        let query = self.search.borrow().buffer().text().to_lowercase();
        self.visible = self
            .choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                choice.title.to_lowercase().contains(&query)
                    || choice.details.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        let focus = selected
            .and_then(|id| {
                self.visible
                    .iter()
                    .position(|index| self.choices[*index].id == id)
            })
            .or_else(|| (!self.visible.is_empty()).then_some(0));
        self.list.set(SelectableListState::new(focus));
    }
    fn render(&self) -> Buffer {
        let width = self.size.width.saturating_sub(1).min(100);
        let height = self.size.height.saturating_sub(1).min(15);
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        if width < 20 || height < 7 {
            let root = TextBlock::new("Resize terminal · Esc cancels");
            let layout = root.layout(
                Constraints::tight(Size::new(width, height)),
                &mut LayoutCx::default(),
            );
            root.paint(&layout, &mut PaintCx::new(&mut frame));
            return buffer;
        }
        drop(frame);
        let mut cx = LayoutCx::default();
        let title = TextBlock::new(if self.review {
            "Broadcast · Review public sharing"
        } else {
            "Broadcast · Choose a terminal"
        })
        .style(Style::new().fg(Color::Cyan));
        let policy = TextInputPolicy::raw();
        let items = self.items();
        let list = SelectableListComponent::new("sources", &items, &self.list);
        let selected = self.selected();
        let details = selected.map_or_else(
            || "No matching terminals".into(),
            |choice| {
                if self.review {
                    format!("{}\n{}", choice.title, choice.details)
                } else {
                    choice.details.clone()
                }
            },
        );
        let hints = if self.review {
            vec![
                KeyHint::new("Esc", "back"),
                KeyHint::new("Ctrl-C", "cancel"),
                KeyHint::new("Ctrl-S", "start broadcasting"),
            ]
        } else {
            vec![
                KeyHint::new("↑↓", "move"),
                KeyHint::new("PgUp/PgDn", "scroll"),
                KeyHint::new("Type", "search"),
                KeyHint::new("Enter", "review"),
                KeyHint::new("Esc", "cancel"),
            ]
        };
        let footer = KeyHintBarComponent::new("keys", &hints);
        let root = Column::new().child(title);
        let root = if self.review {
            root.child(
                TextBlock::new("PUBLIC · Entire session, all panes and future output")
                    .style(Style::new().fg(Color::Yellow)),
            )
            .child(TextBlock::new(format!("Destination: {}", self.destination)))
            .flex(Flex::new(1, TextBlock::new(details)))
        } else {
            root.child(
                TextInputComponent::new("search", &self.search, &policy)
                    .focused(true)
                    .placeholder("Search terminals…", Style::new().fg(Color::BrightBlack)),
            )
            .flex(Flex::new(1, list))
            .child(TextBlock::new(format!(
                "{} matching · {}",
                self.visible.len(),
                details.lines().next().unwrap_or("")
            )))
        }
        .child(footer);
        let layout = root.layout(Constraints::tight(Size::new(width, height)), &mut cx);
        fn reveal(
            node: &bmux_tui::component::LayoutNode,
            items: &[SelectableListItem],
            state: &Cell<SelectableListState>,
        ) {
            if node.id == bmux_tui::component::LayoutId::new("sources") {
                SelectableListComponent::new("sources", items, state).reveal_focus(node);
            }
            for child in &node.children {
                reveal(&child.node, items, state);
            }
        }
        *self.list_layout.borrow_mut() = layout
            .find(&bmux_tui::component::LayoutId::new("sources"))
            .cloned();
        reveal(&layout, &items, &self.list);
        let mut frame = Frame::new(&mut buffer);
        root.paint(&layout, &mut PaintCx::new(&mut frame));
        buffer
    }
}
fn key_is_confirm(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.key == KeyCode::Enter || (key.key == KeyCode::Char('s') && key.modifiers.ctrl))
}

impl Program for Picker {
    type Message = Message;
    type Error = io::Error;
    fn update(&mut self, event: RuntimeEvent<Self::Message>) -> io::Result<Update<Self::Message>> {
        let event = match event {
            RuntimeEvent::Terminal(event) => event,
            RuntimeEvent::Message(Message::Input(error)) => return Err(error),
            RuntimeEvent::Message(Message::Details { id, text }) => {
                if let Some(choice) = self.choices.iter_mut().find(|choice| choice.id == id) {
                    choice.details = text;
                }
                return Ok(Update::redraw());
            }
            _ => return Ok(Update::none()),
        };
        if let Event::Resize(size) = event {
            self.size = size;
            return Ok(Update::redraw());
        }
        if key_is_confirm(&event) && (self.size.width < 21 || self.size.height < 8) {
            return Ok(Update::none());
        }
        if let Event::Key(key) = &event {
            if key.key == KeyCode::Char('c') && key.modifiers.ctrl {
                return Ok(Update::exit());
            }
            if key.key == KeyCode::Escape {
                if self.review {
                    self.review = false;
                    return Ok(Update::redraw());
                }
                return Ok(Update::exit());
            }
            if self.review {
                if key.key == KeyCode::Char('s') && key.modifiers.ctrl {
                    self.accepted = self.selected().map(|choice| choice.id.clone());
                    return Ok(Update::exit());
                }
                return Ok(Update::none());
            }
            if matches!(key.key, KeyCode::Char(_))
                && !key.modifiers.ctrl
                && self.search.borrow().buffer().text().len() >= 4096
            {
                return Ok(Update::none());
            }
            if key.key == KeyCode::Enter {
                self.review = self.selected().is_some();
                return Ok(Update::redraw());
            }
            if matches!(
                key.key,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::PageUp
                    | KeyCode::PageDown
            ) {
                let items = self.items();
                if let Some(layout) = self.list_layout.borrow().as_ref() {
                    let component = SelectableListComponent::new("sources", &items, &self.list);
                    component.handle_event(
                        &event,
                        layout,
                        &mut bmux_tui::component::EventCx::new(layout),
                    );
                }
                return Ok(Update::redraw());
            }
        }
        if let Event::Paste(text) = &event
            && text
                .len()
                .saturating_add(self.search.borrow().buffer().text().len())
                > 4096
        {
            return Ok(Update::none());
        }
        if !self.review {
            TextInputControl::new(&TextInputPolicy::raw())
                .handle_event(&mut self.search.borrow_mut(), &event);
            self.filter();
        }
        Ok(Update::redraw())
    }
}
struct InlinePresenter {
    terminal: InlineTerminal<io::Stderr>,
}
impl Presenter<Picker> for InlinePresenter {
    type Error = io::Error;
    fn reset(&mut self, _: ResetReason) {}
    fn present(&mut self, picker: &mut Picker) -> io::Result<PresentReport> {
        self.terminal.draw(&picker.render())?;
        Ok(PresentReport::default())
    }
}

pub(super) async fn select(
    choices: Vec<Choice>,
    destination: String,
    paths: bmux_config::ConfigPaths,
) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(io::Error::other(
            "interactive selection requires a TTY; pass --session or --source",
        ));
    }
    let (width, height) = crossterm::terminal::size()?;
    let presenter = InlinePresenter {
        terminal: InlineTerminal::enter(io::stderr())?,
    };
    let (runtime, handle) = Runtime::new(
        Picker::new(choices, Size::new(width, height), destination),
        presenter,
        RuntimeConfig::default(),
    );
    let mut input = TerminalInput::start::<Picker>(handle.clone(), Message::Input);
    let metadata = async {
        let Ok(mut client) =
            bmux_client::BmuxClient::connect_with_paths(&paths, "broadcast-picker-details").await
        else {
            return std::future::pending::<()>().await;
        };
        let Ok(sessions) =
            bmux_sessions_plugin_api::sessions_state::client::list_sessions(&mut client).await
        else {
            return std::future::pending::<()>().await;
        };
        for session in sessions.into_iter().take(256) {
            if let Ok(Ok(list)) =
                bmux_pane_runtime_plugin_api::pane_runtime_state::client::list_panes(
                    &mut client,
                    Some(session.id),
                )
                .await
            {
                let hints = list
                    .panes
                    .iter()
                    .take(3)
                    .map(|pane| {
                        let command = pane.active_command.as_deref().unwrap_or(&pane.shell);
                        let command = std::path::Path::new(command)
                            .file_name()
                            .and_then(|value| value.to_str())
                            .unwrap_or(command);
                        format!(
                            "{}: {}",
                            super::guided::display_hint(pane.name.as_deref().unwrap_or("pane")),
                            super::guided::display_hint(command)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                let text = format!(
                    "{} pane(s) · {}\nSession {}",
                    list.panes.len(),
                    hints,
                    session.id
                );
                if handle
                    .send(Message::Details {
                        id: session.id.to_string(),
                        text,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
        std::future::pending::<()>().await
    };
    tokio::pin!(metadata);
    let result =
        tokio::select! { output = runtime.run() => output, () = &mut metadata => unreachable!() };
    input.shutdown().await;
    let mut output = match result {
        Ok(output) => output,
        Err(RuntimeError::Program { error, .. } | RuntimeError::Presenter { error, .. }) => {
            return Err(error);
        }
    };
    output.presenter.terminal.finish()?;
    Ok(output.program.accepted)
}
