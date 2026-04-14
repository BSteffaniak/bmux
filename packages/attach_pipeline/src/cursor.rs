use crate::types::AttachCursorState;
use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, RestorePosition, Show};
use crossterm::queue;
use std::io::Write;

/// Apply cursor visibility/position updates for an attach-rendered frame.
///
/// # Errors
///
/// Returns an error when writing terminal cursor control sequences fails.
pub fn apply_attach_cursor_state(
    stdout: &mut impl Write,
    cursor_state: Option<AttachCursorState>,
    last_cursor_state: &mut Option<AttachCursorState>,
    force_move: bool,
) -> Result<()> {
    match (cursor_state, *last_cursor_state) {
        (Some(current), Some(previous)) if current == previous => {
            if force_move {
                queue!(stdout, MoveTo(current.x, current.y))
                    .context("failed forcing attach cursor move")?;
            } else {
                queue!(stdout, RestorePosition).context("failed restoring cursor position")?;
            }
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

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[test]
    fn equal_cursor_state_uses_restore_position_without_force() {
        let mut out = Vec::new();
        let cursor = AttachCursorState {
            x: 4,
            y: 2,
            visible: false,
        };
        let mut last = Some(cursor);

        apply_attach_cursor_state(&mut out, Some(cursor), &mut last, false)
            .expect("cursor apply should succeed");

        assert_eq!(out, b"\x1b8");
    }

    #[test]
    fn equal_cursor_state_forces_explicit_move_when_requested() {
        let mut out = Vec::new();
        let cursor = AttachCursorState {
            x: 4,
            y: 2,
            visible: false,
        };
        let mut last = Some(cursor);

        apply_attach_cursor_state(&mut out, Some(cursor), &mut last, true)
            .expect("cursor apply should succeed");

        assert_ne!(out, b"\x1b8");
        assert!(
            std::str::from_utf8(&out)
                .expect("cursor bytes should be utf8")
                .ends_with('H')
        );
    }
}
