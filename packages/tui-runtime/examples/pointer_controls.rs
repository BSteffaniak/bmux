//! Standard-presenter pointer controls; no application-owned escape sequences.
//! Run with `cargo run -p bmux_tui_runtime --example pointer_controls --no-default-features --features crossterm`.

use std::cell::{Cell, RefCell};
use std::io::{self, Write};
use std::time::Duration;

use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::component::{Component, Constraints, Element, LayoutCx, LayoutNode};
use bmux_tui::composition::{Column, Overlay, SizeBox, TextBlock};
use bmux_tui::crossterm::{CrosstermTerminalGuard, poll_event, terminal_size};
use bmux_tui::event::{Event, FocusEvent, MouseButton, MouseEventKind};
use bmux_tui::focus::FocusTrap;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::hit::{HitId, HitMap, HitRegion, HitRole};
use bmux_tui::interaction::InteractionRouter;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::pointer::{PointerIntent, PointerShape};
use bmux_tui::style::Color;
use bmux_tui::terminal::Terminal;
use bmux_tui::text::Line;
use bmux_tui_components::button::{Button, ButtonComponent, ButtonState};
use bmux_tui_components::modal_frame::{ModalFrame, ModalFrameComponent, ModalSizing, ModalTheme};
use bmux_tui_components::text_input::{
    TextInputComponent, TextInputControl, TextInputPolicy, TextInputState,
};
use bmux_tui_runtime::{PointerContextPresenter, Presenter, TerminalPresenter};

struct App {
    router: InteractionRouter,
    input: RefCell<TextInputState>,
    policy: TextInputPolicy,
    button: Cell<ButtonState>,
    disabled: Cell<ButtonState>,
    close: Cell<ButtonState>,
    modal: bool,
    override_pointer: bool,
    clicks: u64,
    drag_column: u16,
}

impl App {
    fn new() -> Self {
        let mut disabled = ButtonState::new();
        disabled.set_disabled(true);
        Self {
            router: InteractionRouter::new(),
            input: RefCell::new(TextInputState::new(TextEditBuffer::from_text("Edit me"))),
            policy: TextInputPolicy::chat_composer(),
            button: Cell::new(ButtonState::new()),
            disabled: Cell::new(disabled),
            close: Cell::new(ButtonState::new()),
            modal: false,
            override_pointer: false,
            clicks: 0,
            drag_column: 0,
        }
    }

    fn tree(&self) -> Element<'_> {
        let background = Column::new()
            .gap(1)
            .child(TextBlock::new(
                "Pointer controls: Tab / Shift-Tab to focus; Ctrl-C to exit",
            ))
            .child(TextBlock::new(
                "F2: modal   F3: crosshair override   F4: disable/enable action",
            ))
            .child(
                ButtonComponent::new("action", "Count click", &self.button).pointer_intent(
                    if self.override_pointer {
                        PointerIntent::Shape(PointerShape::Crosshair)
                    } else {
                        PointerIntent::Auto
                    },
                ),
            )
            .child(ButtonComponent::new(
                "disabled",
                "Always disabled",
                &self.disabled,
            ))
            .child(
                SizeBox::new(
                    TextInputComponent::new("input", &self.input, &self.policy)
                        .focused(self.router.focused() == Some(&HitId::new("input"))),
                )
                .height(3),
            )
            .child(DragHandle)
            .child(TextBlock::new(format!(
                "Clicks: {}   Drag column: {}   Override: {}",
                self.clicks, self.drag_column, self.override_pointer,
            )))
            .child(TextBlock::new(
                "Drag the handle outside its bounds; release to end capture.",
            ));
        if !self.modal {
            return Element::new(background);
        }
        let panel = ModalFrameComponent::new(
            "modal-panel",
            ModalFrame::new(
                ModalSizing::fixed(Size::new(48, 9), Insets::new(1, 1, 1, 1)),
                ModalTheme::dark(Color::Cyan),
            )
            .title("Modal: background input is excluded"),
            Column::new()
                .gap(1)
                .child(TextBlock::new("Escape or the button closes this modal."))
                .child(ButtonComponent::new("close", "Close modal", &self.close)),
        );
        Element::new(Overlay::new(background, panel).modal(HitId::new("modal")))
    }

    fn render(&mut self, cx: &mut PaintCx<'_, '_>) {
        let area = cx.area();
        let tree = self.tree();
        let layout = tree.layout(
            Constraints::tight(Size::new(area.width, area.height)),
            &mut LayoutCx::new(),
        );
        tree.paint(&layout, cx);
    }

    fn commit(&mut self, hits: &HitMap, focus: &FocusTrap) {
        self.router
            .commit_scene(hits.clone(), focus.active_scope().cloned());
        if let Some(region) = self.router.region(&HitId::new("input")) {
            self.input
                .borrow_mut()
                .set_content_area(region.area, &self.policy);
        }
    }

    /// Returns false to leave. All control bounds come from committed painting.
    fn event(&mut self, event: Event) -> bool {
        if let Event::Key(key) = &event {
            if key.key == KeyCode::Char('c') && key.modifiers.ctrl {
                return false;
            }
            match key.key {
                KeyCode::F(2) => {
                    self.modal = !self.modal;
                    return true;
                }
                KeyCode::Escape if self.modal => {
                    self.modal = false;
                    return true;
                }
                KeyCode::F(3) if !self.modal => {
                    self.override_pointer = !self.override_pointer;
                    return true;
                }
                KeyCode::F(4) if !self.modal => {
                    let mut state = self.button.get();
                    state.set_disabled(!state.interaction.disabled);
                    self.button.set(state);
                    return true;
                }
                _ => {}
            }
        }
        let route = self.router.route(event);
        if let Event::Mouse(mouse) = &route.event
            && matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
            )
            && self.router.pointer_context().capture.as_ref() == Some(&HitId::new("drag"))
        {
            self.drag_column = mouse.position.x;
        }
        if matches!(
            route.event,
            Event::Focus(FocusEvent::Lost) | Event::Resize(_)
        ) {
            let _ = TextInputControl::new(&self.policy)
                .handle_event(&mut self.input.borrow_mut(), &route.event);
        }
        for (id, state) in [
            ("action", &self.button),
            ("disabled", &self.disabled),
            ("close", &self.close),
        ] {
            let id = HitId::new(id);
            let mut value = state.get();
            value.set_focused(self.router.focused() == Some(&id));
            // Cancel old hover/press on leave, including release outside the target.
            if route.hover_left.as_ref() == Some(&id)
                || matches!(
                    route.event,
                    Event::Focus(FocusEvent::Lost) | Event::Resize(_)
                )
                || matches!(route.event, Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Up(_)) && !route.targets(&id))
            {
                let _ = Button::new("").handle_event(
                    Rect::default(),
                    &mut value,
                    &Event::Focus(FocusEvent::Lost),
                );
            }
            if let Some(event) = route.event_for(&id)
                && let Some(region) = self.router.region(&id)
            {
                let _ = Button::new("").handle_event(region.area, &mut value, event);
            }
            state.set(value);
        }
        if route.targets(&HitId::new("action")) && route.is_activation() {
            self.clicks = self.clicks.saturating_add(1);
        }
        if route.targets(&HitId::new("close")) && route.is_activation() {
            self.modal = false;
        }
        if let Event::Mouse(mouse) = &route.event
            && matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
            && !route.targets(&HitId::new("input"))
        {
            // End an editor selection gesture even when release is outside it.
            let _ = TextInputControl::new(&self.policy)
                .handle_event(&mut self.input.borrow_mut(), &route.event);
        }
        if let Some(event) = route.event_for(&HitId::new("input")) {
            let _ = TextInputControl::new(&self.policy)
                .handle_event(&mut self.input.borrow_mut(), event);
        }
        true
    }
}

/// A neutral custom control using the same component and hit-geometry path.
struct DragHandle;

impl Component for DragHandle {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        TextBlock::new("[ Drag handle ]").layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        cx.write_line(
            LocalRect::new(0, 0, width, 1),
            &Line::raw("[ Drag handle ]"),
        );
        cx.push_hit(
            HitRegion::new("drag", Rect::new(0, 0, width, 1))
                .role(HitRole::DragHandle)
                .hoverable(true)
                .focusable(true),
        );
    }
}

fn presenter<W: Write>(writer: W, size: Size) -> impl Presenter<App, Error = io::Error> {
    let terminal = Terminal::new(writer, Rect::new(0, 0, size.width, size.height));
    let presenter = TerminalPresenter::with_commit(terminal, App::render, App::commit);
    PointerContextPresenter::new(presenter, |app: &App| app.router.pointer_context())
}

async fn run(writer: &mut impl Write, size: Size) -> io::Result<()> {
    let mut app = App::new();
    let mut presenter = presenter(writer, size);
    let result = (|| {
        presenter.present(&mut app)?;
        loop {
            let Some(event) = poll_event(Duration::from_millis(100))? else {
                continue;
            };
            presenter.observe_terminal_event(&event);
            if let Event::Resize(size) = &event {
                presenter.resize(*size);
            }
            if !app.event(event) {
                break;
            }
            presenter.present(&mut app)?;
        }
        Ok(())
    })();
    // The same output owner restores its lease on normal exit and every error.
    let restoration = presenter.restore().await;
    combine(result, restoration)
}

fn combine(result: io::Result<()>, cleanup: io::Result<()>) -> io::Result<()> {
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(io::Error::other(format!(
            "{error}; restoration also failed: {cleanup}"
        ))),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let size = terminal_size()?;
    let mut guard = CrosstermTerminalGuard::enter(io::stdout())?;
    let result = run(guard.writer_mut().expect("guard owns stdout"), size).await;
    // Attempt outer-mode restoration even when pointer restoration failed.
    combine(result, guard.leave().map(|_| ()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::KeyStroke;
    use bmux_tui::event::MouseEvent;
    use bmux_tui::geometry::Point;

    fn mouse(kind: MouseEventKind, point: Point) -> Event {
        Event::Mouse(MouseEvent::new(kind, point))
    }

    fn point(app: &App, id: &str) -> Point {
        // Disabled controls remain painted/occluding, but region() intentionally
        // exposes enabled dispatch targets only. Inspect the committed scene.
        let area = app
            .router
            .scene()
            .regions()
            .iter()
            .find(|region| region.id == HitId::new(id))
            .unwrap_or_else(|| panic!("committed control {id} absent"))
            .area;
        assert!(!area.is_empty(), "committed control {id} is clipped away");
        Point::new(area.x, area.y)
    }

    fn key(app: &mut App, key: KeyCode) {
        assert!(app.event(Event::Key(KeyStroke::simple(key))));
    }

    fn click(app: &mut App, id: &str) {
        let position = point(app, id);
        assert!(app.event(mouse(MouseEventKind::Down(MouseButton::Left), position)));
        assert!(app.event(mouse(MouseEventKind::Up(MouseButton::Left), position)));
    }

    #[test]
    fn standard_presenter_commits_action_input_and_disabled_controls() {
        let mut app = App::new();
        let mut presenter = presenter(io::sink(), Size::new(80, 30));
        presenter.present(&mut app).unwrap();
        for id in ["action", "disabled", "input", "drag"] {
            let _ = point(&app, id);
        }
        let disabled = app
            .router
            .scene()
            .regions()
            .iter()
            .find(|region| region.id == HitId::new("disabled"))
            .expect("disabled geometry remains retained");
        assert!(!disabled.enabled);
        assert!(app.router.region(&HitId::new("disabled")).is_none());
        click(&mut app, "action");
        assert_eq!(app.clicks, 1);
        click(&mut app, "disabled");
        assert_eq!(app.clicks, 1);
        let position = point(&app, "disabled");
        app.event(mouse(MouseEventKind::Move, position));
        assert_eq!(
            app.router.pointer_intent(),
            PointerIntent::Shape(PointerShape::Default)
        );
        click(&mut app, "input");
        key(&mut app, KeyCode::Char('!'));
        assert!(app.input.borrow().buffer().text().contains('!'));
        let position = point(&app, "input");
        app.event(mouse(MouseEventKind::Move, position));
        assert_eq!(
            app.router.pointer_intent(),
            PointerIntent::Shape(PointerShape::Text)
        );
    }

    #[test]
    fn capture_override_and_modal_use_the_committed_scene() {
        let mut app = App::new();
        let mut presenter = presenter(io::sink(), Size::new(80, 30));
        presenter.present(&mut app).unwrap();
        let position = point(&app, "drag");
        app.event(mouse(MouseEventKind::Down(MouseButton::Left), position));
        app.event(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            Point::new(70, 29),
        ));
        assert_eq!(app.drag_column, 70);
        assert_eq!(
            app.router.pointer_intent(),
            PointerIntent::Shape(PointerShape::Grabbing)
        );
        app.event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            Point::new(70, 29),
        ));
        assert!(app.router.pointer_context().capture.is_none());

        let position = point(&app, "action");
        app.event(mouse(MouseEventKind::Move, position));
        key(&mut app, KeyCode::F(3));
        // A model change must not advance the router before output commits.
        assert_eq!(
            app.router.pointer_intent(),
            PointerIntent::Shape(PointerShape::Pointer)
        );
        presenter.present(&mut app).unwrap();
        assert_eq!(
            app.router.pointer_intent(),
            PointerIntent::Shape(PointerShape::Crosshair)
        );
        key(&mut app, KeyCode::F(3));
        presenter.present(&mut app).unwrap();
        assert_eq!(
            app.router.pointer_intent(),
            PointerIntent::Shape(PointerShape::Pointer)
        );

        key(&mut app, KeyCode::F(2));
        presenter.present(&mut app).unwrap();
        assert_eq!(
            app.router.pointer_context().scope,
            Some(HitId::new("modal"))
        );
        let count = app.clicks;
        app.event(mouse(MouseEventKind::Down(MouseButton::Left), position));
        app.event(mouse(MouseEventKind::Up(MouseButton::Left), position));
        assert_eq!(app.clicks, count);
        click(&mut app, "close");
        presenter.present(&mut app).unwrap();
        assert!(app.router.pointer_context().scope.is_none());
    }

    #[test]
    fn disabling_and_focus_loss_invalidate_capture_without_hidden_input() {
        let mut app = App::new();
        let mut presenter = presenter(io::sink(), Size::new(80, 30));
        presenter.present(&mut app).unwrap();
        let position = point(&app, "action");
        app.event(mouse(MouseEventKind::Down(MouseButton::Left), position));
        key(&mut app, KeyCode::F(4));
        presenter.present(&mut app).unwrap();
        assert!(app.router.pointer_context().capture.is_none());
        app.event(mouse(MouseEventKind::Up(MouseButton::Left), position));
        assert_eq!(app.clicks, 0);
        let position = point(&app, "drag");
        app.event(mouse(MouseEventKind::Down(MouseButton::Left), position));
        app.event(Event::Focus(FocusEvent::Lost));
        assert!(app.router.pointer_context().capture.is_none());
    }

    struct ControlledWriter {
        bytes: std::rc::Rc<RefCell<Vec<u8>>>,
        fail: std::rc::Rc<Cell<bool>>,
    }

    impl Write for ControlledWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail.get() {
                return Err(io::Error::other("injected output failure"));
            }
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail.get() {
                Err(io::Error::other("injected flush failure"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn supported_output_restores_and_failed_frames_do_not_commit() {
        use bmux_terminal_protocol::pointer_output::PointerShapeCapability;
        use std::rc::Rc;

        let bytes = Rc::new(RefCell::new(Vec::new()));
        let fail = Rc::new(Cell::new(false));
        let writer = ControlledWriter {
            bytes: Rc::clone(&bytes),
            fail: Rc::clone(&fail),
        };
        let mut terminal = Terminal::new(writer, Rect::new(0, 0, 80, 30));
        terminal.set_pointer_capability(PointerShapeCapability::Stack);
        let mut presenter = PointerContextPresenter::new(
            TerminalPresenter::with_commit(terminal, App::render, App::commit),
            |app: &App| app.router.pointer_context(),
        );
        let mut app = App::new();
        presenter.present(&mut app).unwrap();
        let event = mouse(MouseEventKind::Move, point(&app, "action"));
        presenter.observe_terminal_event(&event);
        app.event(event);
        presenter.present(&mut app).unwrap();
        let before_restore = bytes.borrow().len();
        presenter.restore().await.unwrap();
        assert!(
            bytes.borrow().len() > before_restore,
            "owned lease must be released through output"
        );
        let after_restore = bytes.borrow().len();
        presenter.restore().await.unwrap();
        assert_eq!(
            bytes.borrow().len(),
            after_restore,
            "restoration is idempotent"
        );

        // Exercise cell-output failure separately from uncertain stack mutation.
        presenter
            .inner_mut()
            .terminal_mut()
            .set_pointer_capability(PointerShapeCapability::Unsupported);
        key(&mut app, KeyCode::F(3));
        fail.set(true);
        assert!(presenter.present(&mut app).is_err());
        assert_eq!(
            app.router.pointer_intent(),
            PointerIntent::Shape(PointerShape::Pointer)
        );
        fail.set(false);
        presenter.restore().await.unwrap();
    }

    #[tokio::test]
    async fn standard_presenter_restoration_and_error_reporting() {
        let mut app = App::new();
        let mut presenter = presenter(io::sink(), Size::new(80, 30));
        presenter.present(&mut app).unwrap();
        presenter.restore().await.unwrap();
        let error = combine(
            Err(io::Error::other("application failed")),
            Err(io::Error::other("cleanup failed")),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("application failed"));
        assert!(error.contains("cleanup failed"));
    }
}
