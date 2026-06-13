//! Optional key-to-command mapping for text editing.

use bmux_keyboard::{KeyCode, KeyStroke};

use crate::{TextBoundary, TextBoundaryPolicy, TextDelete, TextEditCommand, TextMotion};

/// Default text-input key binding profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputProfile {
    /// Readline-style terminal editing defaults.
    Readline,
    /// macOS terminal-style defaults layered on top of readline-compatible keys.
    MacTerminal,
    /// Windows terminal-style defaults layered on top of readline-compatible keys.
    WindowsTerminal,
}

/// Editor key mapping options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextKeymap {
    /// Platform/profile defaults to apply.
    pub profile: TextInputProfile,
    /// How Home/End-style boundaries should be interpreted.
    pub boundary_policy: TextBoundaryPolicy,
}

impl Default for TextKeymap {
    fn default() -> Self {
        Self {
            profile: TextInputProfile::Readline,
            boundary_policy: TextBoundaryPolicy::Buffer,
        }
    }
}

impl TextKeymap {
    /// Return the text edit command for `stroke`, if it is a standard editor binding.
    #[must_use]
    pub const fn command_for_key(self, stroke: KeyStroke) -> Option<TextEditCommand> {
        command_for_key(stroke, self.profile, self.boundary_policy)
    }
}

/// Return the text edit command for `stroke`, if it is a standard editor binding.
#[must_use]
pub const fn command_for_key(
    stroke: KeyStroke,
    profile: TextInputProfile,
    boundary_policy: TextBoundaryPolicy,
) -> Option<TextEditCommand> {
    let mods = stroke.modifiers;

    if !mods.ctrl && !mods.alt && !mods.super_key && !mods.hyper && !mods.meta {
        return match stroke.key {
            KeyCode::Backspace => Some(TextEditCommand::Delete(TextDelete::Backward)),
            KeyCode::Delete => Some(TextEditCommand::Delete(TextDelete::Forward)),
            KeyCode::Left => Some(TextEditCommand::Move(TextMotion::Left)),
            KeyCode::Right => Some(TextEditCommand::Move(TextMotion::Right)),
            KeyCode::Up => Some(TextEditCommand::Move(TextMotion::VisualUp)),
            KeyCode::Down => Some(TextEditCommand::Move(TextMotion::VisualDown)),
            KeyCode::Home => Some(TextEditCommand::Move(
                boundary_policy.motion(TextBoundary::Start),
            )),
            KeyCode::End => Some(TextEditCommand::Move(
                boundary_policy.motion(TextBoundary::End),
            )),
            KeyCode::Char(ch) => Some(TextEditCommand::Insert(shifted_input_char(ch, mods.shift))),
            KeyCode::Space => Some(TextEditCommand::Insert(' ')),
            _ => None,
        };
    }

    if mods.ctrl && !mods.alt && !mods.super_key && !mods.hyper && !mods.meta {
        return match stroke.key {
            KeyCode::Char('a') => Some(TextEditCommand::Move(
                boundary_policy.motion(TextBoundary::Start),
            )),
            KeyCode::Char('e') => Some(TextEditCommand::Move(
                boundary_policy.motion(TextBoundary::End),
            )),
            KeyCode::Char('u') => Some(TextEditCommand::Delete(
                boundary_policy.delete(TextBoundary::Start),
            )),
            KeyCode::Char('k') => Some(TextEditCommand::Delete(
                boundary_policy.delete(TextBoundary::End),
            )),
            KeyCode::Char('w') | KeyCode::Backspace => {
                Some(TextEditCommand::Delete(TextDelete::WordBackward))
            }
            KeyCode::Left => Some(TextEditCommand::Move(TextMotion::WordLeft)),
            KeyCode::Right => Some(TextEditCommand::Move(TextMotion::WordRight)),
            KeyCode::Delete => Some(TextEditCommand::Delete(TextDelete::WordForward)),
            _ => None,
        };
    }

    if mods.alt && !mods.ctrl && !mods.super_key && !mods.hyper && !mods.meta {
        return match stroke.key {
            KeyCode::Left | KeyCode::Char('b') => Some(TextEditCommand::Move(TextMotion::WordLeft)),
            KeyCode::Right | KeyCode::Char('f') => {
                Some(TextEditCommand::Move(TextMotion::WordRight))
            }
            KeyCode::Backspace => Some(TextEditCommand::Delete(TextDelete::WordBackward)),
            KeyCode::Delete => Some(TextEditCommand::Delete(TextDelete::WordForward)),
            _ => None,
        };
    }

    match profile {
        TextInputProfile::MacTerminal if mods.super_key && !mods.ctrl && !mods.alt => {
            match stroke.key {
                KeyCode::Left => Some(TextEditCommand::Move(TextMotion::LineStart)),
                KeyCode::Right => Some(TextEditCommand::Move(TextMotion::LineEnd)),
                _ => None,
            }
        }
        TextInputProfile::WindowsTerminal if mods.ctrl && !mods.alt => match stroke.key {
            KeyCode::Left => Some(TextEditCommand::Move(TextMotion::WordLeft)),
            KeyCode::Right => Some(TextEditCommand::Move(TextMotion::WordRight)),
            _ => None,
        },
        TextInputProfile::Readline
        | TextInputProfile::MacTerminal
        | TextInputProfile::WindowsTerminal => None,
    }
}

const fn shifted_input_char(ch: char, shift: bool) -> char {
    if shift && ch.is_ascii_lowercase() {
        ch.to_ascii_uppercase()
    } else {
        ch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::Modifiers;

    fn stroke(key: KeyCode, modifiers: Modifiers) -> KeyStroke {
        KeyStroke::with_modifiers(key, modifiers)
    }

    #[test]
    fn maps_readline_movement_and_deletion_keys() {
        let keymap = TextKeymap::default();

        assert_eq!(
            keymap.command_for_key(stroke(KeyCode::Left, Modifiers::NONE)),
            Some(TextEditCommand::Move(TextMotion::Left))
        );
        assert_eq!(
            keymap.command_for_key(stroke(
                KeyCode::Char('w'),
                Modifiers {
                    ctrl: true,
                    ..Modifiers::NONE
                },
            )),
            Some(TextEditCommand::Delete(TextDelete::WordBackward))
        );
        assert_eq!(
            keymap.command_for_key(stroke(
                KeyCode::Delete,
                Modifiers {
                    alt: true,
                    ..Modifiers::NONE
                },
            )),
            Some(TextEditCommand::Delete(TextDelete::WordForward))
        );
    }

    #[test]
    fn inserts_shifted_ascii_letters_as_uppercase() {
        let keymap = TextKeymap::default();

        assert_eq!(
            keymap.command_for_key(stroke(
                KeyCode::Char('b'),
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            )),
            Some(TextEditCommand::Insert('B'))
        );
    }

    #[test]
    fn honors_boundary_policy() {
        let keymap = TextKeymap {
            profile: TextInputProfile::Readline,
            boundary_policy: TextBoundaryPolicy::Line,
        };

        assert_eq!(
            keymap.command_for_key(stroke(
                KeyCode::Char('a'),
                Modifiers {
                    ctrl: true,
                    ..Modifiers::NONE
                },
            )),
            Some(TextEditCommand::Move(TextMotion::LineStart))
        );
    }
}
