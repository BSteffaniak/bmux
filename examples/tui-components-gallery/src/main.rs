use std::io::{Stdout, stdout};

use anyhow::Result;
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::crossterm::{CrosstermTerminalGuard, terminal_size};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::interaction::InteractionRouter;
use bmux_tui::paint::PaintCx;
use bmux_tui::terminal::Terminal;
use bmux_tui_components_gallery::render_gallery_interactive;
use bmux_tui_runtime::{
    Lifecycle, PointerContextPresenter, Program, Runtime, RuntimeConfig, RuntimeEvent,
    TerminalInput, TerminalPresenter, Update,
};

struct GalleryProgram {
    interactions: InteractionRouter,
}

impl GalleryProgram {
    fn new() -> Self {
        Self {
            interactions: InteractionRouter::new(),
        }
    }
}

impl Program for GalleryProgram {
    type Message = std::io::Error;
    type Error = std::io::Error;

    fn update(
        &mut self,
        event: RuntimeEvent<Self::Message>,
    ) -> Result<Update<Self::Message>, Self::Error> {
        match event {
            RuntimeEvent::Terminal(Event::Key(stroke)) if should_quit(stroke) => Ok(Update {
                lifecycle: Lifecycle::Exit,
                ..Update::none()
            }),
            RuntimeEvent::Terminal(event) => {
                let previous_pointer_context = self.interactions.pointer_context();
                let routed = self.interactions.route(event);
                if previous_pointer_context != self.interactions.pointer_context() {
                    return Ok(Update::redraw());
                }
                if routed.traversal_consumed
                    || routed.focus_changed.is_some()
                    || routed.hover_left.is_some()
                    || routed.hover_entered.is_some()
                {
                    Ok(Update::reset())
                } else {
                    Ok(Update::none())
                }
            }
            RuntimeEvent::Message(error) => Err(error),
            RuntimeEvent::Timer(_) => Ok(Update::none()),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut guard = CrosstermTerminalGuard::enter(stdout())?;
    let result: Result<()> = async {
        let writer = guard.writer_mut().expect("guard should own stdout");
        let size = terminal_size()?;
        let terminal = Terminal::new(writer, Rect::new(0, 0, size.width, size.height));
        let presenter = TerminalPresenter::with_commit(
            terminal,
            render_gallery_program,
            |program: &mut GalleryProgram,
             hits: &bmux_tui::hit::HitMap,
             focus: &bmux_tui::focus::FocusTrap| {
                program
                    .interactions
                    .commit_scene(hits.clone(), focus.active_scope().cloned());
            },
        );
        let presenter = PointerContextPresenter::new(presenter, |program: &GalleryProgram| {
            program.interactions.pointer_context()
        });
        let (runtime, handle) = Runtime::new(
            GalleryProgram::new(),
            presenter,
            RuntimeConfig {
                frame_interval: None,
                ..RuntimeConfig::default()
            },
        );
        let mut input = TerminalInput::start::<GalleryProgram>(handle, std::convert::identity);
        let result = runtime.run().await;
        input.shutdown().await;
        match result {
            Ok(_output) => Ok(()),
            Err(bmux_tui_runtime::RuntimeError::Program {
                error,
                cleanup_error,
                ..
            })
            | Err(bmux_tui_runtime::RuntimeError::Presenter {
                error,
                cleanup_error,
                ..
            }) => match cleanup_error {
                Some(cleanup) => Err(anyhow::anyhow!(
                    "{error}; pointer restoration also failed: {cleanup}"
                )),
                None => Err(error.into()),
            },
        }
    }
    .await;
    let restoration = guard
        .leave()
        .map(|_stdout: Stdout| ())
        .map_err(anyhow::Error::from);
    match (result, restoration) {
        (Err(error), Err(cleanup)) => Err(anyhow::anyhow!(
            "{error}; terminal restoration also failed: {cleanup}"
        )),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn render_gallery_program(program: &mut GalleryProgram, cx: &mut PaintCx<'_, '_>) {
    render_gallery_interactive(
        cx,
        program
            .interactions
            .focused()
            .map(bmux_tui::hit::HitId::as_str),
    );
}

fn should_quit(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Escape
        || stroke.key == KeyCode::Char('q')
        || (stroke.key == KeyCode::Char('c')
            && stroke.modifiers
                == Modifiers {
                    ctrl: true,
                    ..Modifiers::NONE
                })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::geometry::Point;
    use bmux_tui::hit::{HitMap, HitRegion, HitRole};
    use bmux_tui_runtime::{Invalidation, Presenter};

    #[test]
    fn capture_changes_request_presentation_even_without_focus_or_hover_change() {
        let mut program = GalleryProgram::new();
        program.interactions.commit_scene(
            HitMap::new().with_region(
                HitRegion::new("drag", Rect::new(0, 0, 10, 1)).role(HitRole::DragHandle),
            ),
            None,
        );
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let update = program
                .update(RuntimeEvent::Terminal(Event::Mouse(MouseEvent::new(
                    kind,
                    Point::new(1, 0),
                ))))
                .unwrap();
            assert_eq!(update.invalidation, Invalidation::Redraw);
        }
    }

    #[test]
    fn standard_presenter_forwards_the_existing_router_context() {
        let mut program = GalleryProgram::new();
        let mut presenter = PointerContextPresenter::new(
            TerminalPresenter::with_commit(
                Terminal::new(std::io::sink(), Rect::new(0, 0, 10, 2)),
                |_program: &mut GalleryProgram, cx: &mut PaintCx<'_, '_>| {
                    cx.set_focus_scope(Some("modal".into()));
                    cx.push_hit(
                        HitRegion::new("drag", Rect::new(0, 0, 10, 1))
                            .role(HitRole::DragHandle)
                            .focus_scope("modal"),
                    );
                },
                |program: &mut GalleryProgram,
                 hits: &HitMap,
                 focus: &bmux_tui::focus::FocusTrap| {
                    program
                        .interactions
                        .commit_scene(hits.clone(), focus.active_scope().cloned());
                },
            ),
            |program: &GalleryProgram| program.interactions.pointer_context(),
        );
        presenter.present(&mut program).unwrap();
        assert_eq!(
            program.interactions.pointer_context().scope,
            Some("modal".into())
        );
        let event = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(1, 0),
        ));
        presenter.observe_terminal_event(&event);
        program.update(RuntimeEvent::Terminal(event)).unwrap();
        assert_eq!(
            program.interactions.pointer_context().capture,
            Some("drag".into()),
            "the explicitly scoped target captures before another frame"
        );
        presenter.present(&mut program).unwrap();
        assert_eq!(
            program.interactions.pointer_context().capture,
            Some("drag".into())
        );
    }
}
