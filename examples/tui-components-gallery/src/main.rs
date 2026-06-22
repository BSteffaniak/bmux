use std::io::{Stdout, stdout};
use std::time::Duration;

use anyhow::Result;
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::crossterm::{CrosstermTerminalGuard, poll_event};
use bmux_tui::event::Event;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::terminal::Terminal;
use bmux_tui_components_gallery::{HEIGHT, WIDTH, render_gallery_into};

fn main() -> Result<()> {
    let mut guard = CrosstermTerminalGuard::enter(stdout())?;
    {
        let writer = guard.writer_mut().expect("guard should own stdout");
        let mut terminal = Terminal::new(writer, Rect::new(0, 0, WIDTH, HEIGHT));

        loop {
            terminal.draw(render_gallery_into)?;
            if let Some(event) = poll_event(Duration::from_millis(100))? {
                match event {
                    Event::Key(stroke) if should_quit(stroke) => break,
                    Event::Resize(size) => terminal.resize(rect_from_size(size)),
                    Event::Key(_)
                    | Event::Mouse(_)
                    | Event::Paste(_)
                    | Event::Focus(_)
                    | Event::Tick
                    | Event::User(_) => {}
                }
            }
        }
    }

    let _stdout: Stdout = guard.leave()?;
    Ok(())
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

const fn rect_from_size(size: Size) -> Rect {
    Rect::new(0, 0, size.width, size.height)
}
