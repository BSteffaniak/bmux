use std::io::{Stdout, stdout};
use std::time::Duration;

use anyhow::Result;
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::crossterm::{CrosstermTerminalGuard, poll_event};
use bmux_tui::event::Event;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::terminal::Terminal;
use bmux_tui_components_layout_playground::LayoutPlaygroundDemo;

fn main() -> Result<()> {
    let initial_area = Rect::new(0, 0, 72, 18);
    let mut demo = LayoutPlaygroundDemo::new(initial_area);
    let mut guard = CrosstermTerminalGuard::enter(stdout())?;
    let result = {
        let writer = guard.writer_mut().expect("guard should own stdout");
        let mut terminal = Terminal::new(writer, initial_area);

        let result = (|| -> Result<()> {
            loop {
                terminal.draw(|cx| demo.render(cx))?;
                if let Some(event) = poll_event(Duration::from_millis(100))? {
                    terminal.observe_event(&event);
                    match event {
                        Event::Key(stroke) if should_quit(stroke) => break,
                        Event::Resize(size) => {
                            let area = rect_from_size(size);
                            terminal.resize(area);
                            demo.resize_terminal(area);
                        }
                        Event::Key(_)
                        | Event::Mouse(_)
                        | Event::Paste(_)
                        | Event::Focus(_)
                        | Event::Tick
                        | Event::User(_) => {
                            if demo.handle_event(&event) {
                                break;
                            }
                        }
                    }
                }
            }
            Ok(())
        })();
        combine_restoration(result, terminal.restore_pointer().map_err(Into::into))
    };

    let restoration = guard.leave().map(|_stdout: Stdout| ()).map_err(Into::into);
    combine_restoration(result, restoration)
}

fn combine_restoration(result: Result<()>, restoration: Result<()>) -> Result<()> {
    match (result, restoration) {
        (Err(error), Err(cleanup)) => Err(anyhow::anyhow!(
            "{error}; restoration also failed: {cleanup}"
        )),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn should_quit(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Char('q')
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
