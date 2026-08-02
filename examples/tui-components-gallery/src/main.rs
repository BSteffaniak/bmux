use std::io::{Stdout, stdout};

use anyhow::Result;
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::crossterm::CrosstermTerminalGuard;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use bmux_tui_components_gallery::{HEIGHT, WIDTH, render_gallery_into};
use bmux_tui_runtime::{
    Lifecycle, Program, Runtime, RuntimeConfig, RuntimeEvent, TerminalInput, TerminalPresenter,
    Update,
};

struct GalleryProgram;

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
            RuntimeEvent::Message(error) => Err(error),
            RuntimeEvent::Terminal(Event::Resize(_)) => Ok(Update::reset()),
            RuntimeEvent::Terminal(_) | RuntimeEvent::Timer(_) => Ok(Update::none()),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut guard = CrosstermTerminalGuard::enter(stdout())?;
    let result = {
        let writer = guard.writer_mut().expect("guard should own stdout");
        let terminal = Terminal::new(writer, Rect::new(0, 0, WIDTH, HEIGHT));
        let presenter = TerminalPresenter::new(terminal, render_gallery_program);
        let (runtime, handle) = Runtime::new(
            GalleryProgram,
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

fn render_gallery_program(_program: &mut GalleryProgram, frame: &mut Frame<'_>) {
    render_gallery_into(frame);
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
