use std::io::{Stdout, stdout};

use anyhow::Result;
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::crossterm::{CrosstermTerminalGuard, terminal_size};
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::interaction::InteractionRouter;
use bmux_tui::terminal::Terminal;
use bmux_tui_components_gallery::render_gallery_interactive;
use bmux_tui_runtime::{
    Lifecycle, Program, Runtime, RuntimeConfig, RuntimeEvent, TerminalInput, TerminalPresenter,
    Update,
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
                let routed = self.interactions.route(event);
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
    let result = {
        let writer = guard.writer_mut().expect("guard should own stdout");
        let size = terminal_size()?;
        let terminal = Terminal::new(writer, Rect::new(0, 0, size.width, size.height));
        let presenter = TerminalPresenter::with_commit(
            terminal,
            render_gallery_program,
            |program: &mut GalleryProgram,
             hits: &bmux_tui::hit::HitMap,
             _focus: &bmux_tui::focus::FocusTrap| {
                program.interactions.commit_scene(hits.clone(), None);
            },
        );
        let (runtime, handle) = Runtime::new(
            GalleryProgram::new(),
            presenter,
            RuntimeConfig {
                frame_interval: None,
                ..RuntimeConfig::default()
            },
        );
        let _input = TerminalInput::start::<GalleryProgram>(handle, std::convert::identity);
        match runtime.run().await {
            Ok(_output) => Ok(()),
            Err(bmux_tui_runtime::RuntimeError::Program { error, .. })
            | Err(bmux_tui_runtime::RuntimeError::Presenter { error, .. }) => Err(error),
        }
    };
    let _stdout: Stdout = guard.leave()?;
    result.map_err(Into::into)
}

fn render_gallery_program(program: &mut GalleryProgram, frame: &mut Frame<'_>) {
    render_gallery_interactive(
        frame,
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
