use super::state::AttachCursorState;
use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, RestorePosition, Show};
use crossterm::queue;
use std::io::Write;

pub fn apply_attach_cursor_state(
    stdout: &mut impl Write,
    cursor_state: Option<AttachCursorState>,
    last_cursor_state: &mut Option<AttachCursorState>,
) -> Result<()> {
    match (cursor_state, *last_cursor_state) {
        (Some(current), Some(previous)) if current == previous => {
            queue!(stdout, RestorePosition).context("failed restoring cursor position")?;
        }
        (Some(current), Some(previous)) => {
            if current.visible != previous.visible {
                if current.visible {
                    queue!(stdout, Show).context("failed showing attach cursor")?;
                } else {
                    queue!(stdout, Hide).context("failed hiding attach cursor")?;
                }
            }
            queue!(stdout, MoveTo(current.x, current.y)).context("failed moving attach cursor")?;
        }
        (Some(current), None) => {
            if current.visible {
                queue!(stdout, Show).context("failed showing attach cursor")?;
            } else {
                queue!(stdout, Hide).context("failed hiding attach cursor")?;
            }
            queue!(stdout, MoveTo(current.x, current.y)).context("failed moving attach cursor")?;
        }
        (None, Some(previous)) => {
            if previous.visible {
                queue!(stdout, Hide).context("failed hiding attach cursor")?;
            }
        }
        (None, None) => {}
    }

    *last_cursor_state = cursor_state;
    Ok(())
}
