//! Optional key-to-command mapping for text editing.
//!
//! [`TextBindings`] is the rebindable stroke-to-[`TextEditAction`] table;
//! [`command_for_key`] is the fixed legacy mapping it reproduces by default.

use std::collections::BTreeMap;

use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

use crate::{
    TextBoundary, TextBoundaryPolicy, TextDelete, TextEditAction, TextEditCommand, TextMotion,
};

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

    /// Return the default rebindable action table for this keymap.
    #[must_use]
    pub fn bindings(self) -> TextBindings {
        TextBindings::defaults(self.profile, self.boundary_policy)
    }
}

const NONE: Modifiers = Modifiers::NONE;
const SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::NONE
};
const CTRL: Modifiers = Modifiers {
    ctrl: true,
    ..Modifiers::NONE
};
const CTRL_SHIFT: Modifiers = Modifiers {
    ctrl: true,
    shift: true,
    ..Modifiers::NONE
};
const ALT: Modifiers = Modifiers {
    alt: true,
    ..Modifiers::NONE
};
const ALT_SHIFT: Modifiers = Modifiers {
    alt: true,
    shift: true,
    ..Modifiers::NONE
};
const CTRL_ALT_SHIFT: Modifiers = Modifiers {
    ctrl: true,
    alt: true,
    shift: true,
    ..Modifiers::NONE
};
const SUPER: Modifiers = Modifiers {
    super_key: true,
    ..Modifiers::NONE
};
const SUPER_SHIFT: Modifiers = Modifiers {
    super_key: true,
    shift: true,
    ..Modifiers::NONE
};

const fn key(code: KeyCode, modifiers: Modifiers) -> KeyStroke {
    KeyStroke::with_modifiers(code, modifiers)
}

/// Conventional undo strokes: ctrl-Z and super-Z.
pub const DEFAULT_UNDO_STROKES: &[KeyStroke] = &[
    key(KeyCode::Char('z'), CTRL),
    key(KeyCode::Char('z'), SUPER),
];

/// Conventional redo strokes: ctrl-shift-Z, super-shift-Z and ctrl-Y.
pub const DEFAULT_REDO_STROKES: &[KeyStroke] = &[
    key(KeyCode::Char('z'), CTRL_SHIFT),
    key(KeyCode::Char('z'), SUPER_SHIFT),
    key(KeyCode::Char('y'), CTRL),
];

/// Policy-independent defaults shared by every profile.
const COMMON_DEFAULTS: &[(TextEditAction, &[KeyStroke])] = {
    use KeyCode::{Backspace, Char, Delete, Down, Left, Right, Up};
    &[
        (TextEditAction::MoveLeft, &[key(Left, NONE)]),
        (TextEditAction::MoveRight, &[key(Right, NONE)]),
        (
            TextEditAction::MoveWordLeft,
            &[
                key(Left, CTRL),
                key(Left, ALT),
                key(Char('b'), ALT),
                key(Char('b'), ALT_SHIFT),
            ],
        ),
        (
            TextEditAction::MoveWordRight,
            &[
                key(Right, CTRL),
                key(Right, ALT),
                key(Char('f'), ALT),
                key(Char('f'), ALT_SHIFT),
            ],
        ),
        (TextEditAction::MoveUp, &[key(Up, NONE)]),
        (TextEditAction::MoveDown, &[key(Down, NONE)]),
        (TextEditAction::SelectLeft, &[key(Left, SHIFT)]),
        (TextEditAction::SelectRight, &[key(Right, SHIFT)]),
        (
            TextEditAction::SelectWordLeft,
            &[
                key(Left, CTRL_SHIFT),
                key(Left, ALT_SHIFT),
                key(Left, CTRL_ALT_SHIFT),
            ],
        ),
        (
            TextEditAction::SelectWordRight,
            &[
                key(Right, CTRL_SHIFT),
                key(Right, ALT_SHIFT),
                key(Right, CTRL_ALT_SHIFT),
            ],
        ),
        (
            TextEditAction::SelectUp,
            &[
                key(Up, SHIFT),
                key(Up, CTRL_SHIFT),
                key(Up, ALT_SHIFT),
                key(Up, CTRL_ALT_SHIFT),
            ],
        ),
        (
            TextEditAction::SelectDown,
            &[
                key(Down, SHIFT),
                key(Down, CTRL_SHIFT),
                key(Down, ALT_SHIFT),
                key(Down, CTRL_ALT_SHIFT),
            ],
        ),
        (
            TextEditAction::DeleteBackward,
            &[key(Backspace, NONE), key(Backspace, SHIFT)],
        ),
        (
            TextEditAction::DeleteForward,
            &[key(Delete, NONE), key(Delete, SHIFT)],
        ),
        (
            TextEditAction::DeleteWordBackward,
            &[
                key(Char('w'), CTRL),
                key(Char('w'), CTRL_SHIFT),
                key(Backspace, CTRL),
                key(Backspace, CTRL_SHIFT),
                key(Backspace, ALT),
                key(Backspace, ALT_SHIFT),
            ],
        ),
        (
            TextEditAction::DeleteWordForward,
            &[
                key(Delete, CTRL),
                key(Delete, CTRL_SHIFT),
                key(Delete, ALT),
                key(Delete, ALT_SHIFT),
            ],
        ),
        (TextEditAction::Undo, DEFAULT_UNDO_STROKES),
        (TextEditAction::Redo, DEFAULT_REDO_STROKES),
    ]
};

/// Start-boundary moves; the action follows the [`TextBoundaryPolicy`].
const BOUNDARY_START_MOTION_STROKES: &[KeyStroke] = &[
    key(KeyCode::Home, NONE),
    key(KeyCode::Home, SHIFT),
    key(KeyCode::Char('a'), CTRL),
    key(KeyCode::Char('a'), CTRL_SHIFT),
];
/// End-boundary moves; the action follows the [`TextBoundaryPolicy`].
const BOUNDARY_END_MOTION_STROKES: &[KeyStroke] = &[
    key(KeyCode::End, NONE),
    key(KeyCode::End, SHIFT),
    key(KeyCode::Char('e'), CTRL),
    key(KeyCode::Char('e'), CTRL_SHIFT),
];
/// Start-boundary deletes; the action follows the [`TextBoundaryPolicy`].
const BOUNDARY_START_DELETE_STROKES: &[KeyStroke] = &[
    key(KeyCode::Char('u'), CTRL),
    key(KeyCode::Char('u'), CTRL_SHIFT),
];
/// End-boundary deletes; the action follows the [`TextBoundaryPolicy`].
const BOUNDARY_END_DELETE_STROKES: &[KeyStroke] = &[
    key(KeyCode::Char('k'), CTRL),
    key(KeyCode::Char('k'), CTRL_SHIFT),
];
/// Additional [`TextInputProfile::MacTerminal`] defaults.
const MAC_TERMINAL_DEFAULTS: &[(TextEditAction, &[KeyStroke])] = &[
    (
        TextEditAction::MoveLineStart,
        &[key(KeyCode::Left, SUPER), key(KeyCode::Left, SUPER_SHIFT)],
    ),
    (
        TextEditAction::MoveLineEnd,
        &[key(KeyCode::Right, SUPER), key(KeyCode::Right, SUPER_SHIFT)],
    ),
];

/// Rebindable stroke-to-[`TextEditAction`] table.
///
/// Lookups are exact: a stroke resolves only when its key and every modifier
/// match a bound stroke. A stroke bound to more than one action fails closed
/// ([`resolve`](Self::resolve) returns `None`); use
/// [`conflicts`](Self::conflicts) to report such bindings and
/// [`unbind_stroke`](Self::unbind_stroke) before rebinding a stroke to a
/// different action. Typed printable characters are not actions; see
/// [`typed_char_for_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBindings {
    /// Strokes per action, indexed by [`TextEditAction`] declaration order.
    strokes: Vec<Vec<KeyStroke>>,
}

impl Default for TextBindings {
    /// Defaults for [`TextKeymap::default`].
    fn default() -> Self {
        TextKeymap::default().bindings()
    }
}

impl TextBindings {
    /// Return a table with every action unbound.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            strokes: vec![Vec::new(); TextEditAction::ALL.len()],
        }
    }

    /// Return the default bindings for a profile and boundary policy.
    ///
    /// Movement and deletion bindings reproduce [`command_for_key`], including
    /// its tolerance of a redundant shift on non-arrow keys. Shift-extended
    /// selection (shift plus arrows, optionally with ctrl or alt; ctrl or alt
    /// select by word horizontally) and
    /// [`DEFAULT_UNDO_STROKES`]/[`DEFAULT_REDO_STROKES`] take precedence over the
    /// shift-tolerant movement of those strokes. `command_for_key` also accepts
    /// some strokes carrying hyper or meta modifiers (and, for
    /// [`TextInputProfile::WindowsTerminal`], ctrl-super arrows); those lenient
    /// combinations are intentionally not part of the table.
    #[must_use]
    pub fn defaults(profile: TextInputProfile, boundary_policy: TextBoundaryPolicy) -> Self {
        let (start_motion, end_motion, start_delete, end_delete) = match boundary_policy {
            TextBoundaryPolicy::Buffer => (
                TextEditAction::MoveStart,
                TextEditAction::MoveEnd,
                TextEditAction::DeleteToStart,
                TextEditAction::DeleteToEnd,
            ),
            TextBoundaryPolicy::Line => (
                TextEditAction::MoveLineStart,
                TextEditAction::MoveLineEnd,
                TextEditAction::DeleteToLineStart,
                TextEditAction::DeleteToLineEnd,
            ),
        };
        let mut bindings = Self::empty();
        let mut bind = |action: TextEditAction, strokes: &[KeyStroke]| {
            bindings.strokes[action as usize].extend_from_slice(strokes);
        };
        for (action, strokes) in COMMON_DEFAULTS {
            bind(*action, strokes);
        }
        bind(start_motion, BOUNDARY_START_MOTION_STROKES);
        bind(end_motion, BOUNDARY_END_MOTION_STROKES);
        bind(start_delete, BOUNDARY_START_DELETE_STROKES);
        bind(end_delete, BOUNDARY_END_DELETE_STROKES);
        if profile == TextInputProfile::MacTerminal {
            for (action, strokes) in MAC_TERMINAL_DEFAULTS {
                bind(*action, strokes);
            }
        }
        bindings
    }

    /// Replace every stroke bound to `action`; an empty slice unbinds it.
    ///
    /// Duplicate strokes within `strokes` are kept once, in first-seen order.
    pub fn set(&mut self, action: TextEditAction, strokes: &[KeyStroke]) {
        let slot = &mut self.strokes[action as usize];
        slot.clear();
        for stroke in strokes {
            if !slot.contains(stroke) {
                slot.push(*stroke);
            }
        }
    }

    /// Remove `stroke` from every action it is bound to.
    pub fn unbind_stroke(&mut self, stroke: KeyStroke) {
        for slot in &mut self.strokes {
            slot.retain(|bound| *bound != stroke);
        }
    }

    /// Return the action bound to `stroke`.
    ///
    /// Returns `None` when the stroke is unbound or bound to several actions.
    #[must_use]
    pub fn resolve(&self, stroke: KeyStroke) -> Option<TextEditAction> {
        let mut matches = self.iter().filter(|(_, strokes)| strokes.contains(&stroke));
        let (action, _) = matches.next()?;
        matches.next().is_none().then_some(action)
    }

    /// Return the strokes bound to `action`, in binding order (for help and hints).
    #[must_use]
    pub fn strokes(&self, action: TextEditAction) -> &[KeyStroke] {
        &self.strokes[action as usize]
    }

    /// Iterate every action in [`TextEditAction::ALL`] order with its strokes.
    ///
    /// Unbound actions are included with an empty slice.
    pub fn iter(&self) -> impl Iterator<Item = (TextEditAction, &[KeyStroke])> + '_ {
        TextEditAction::ALL
            .iter()
            .copied()
            .zip(self.strokes.iter().map(Vec::as_slice))
    }

    /// Return strokes bound to more than one action, ordered by stroke.
    #[must_use]
    pub fn conflicts(&self) -> Vec<(KeyStroke, Vec<TextEditAction>)> {
        let mut by_stroke: BTreeMap<KeyStroke, Vec<TextEditAction>> = BTreeMap::new();
        for (action, strokes) in self.iter() {
            for stroke in strokes {
                by_stroke.entry(*stroke).or_default().push(action);
            }
        }
        by_stroke
            .into_iter()
            .filter(|(_, actions)| actions.len() > 1)
            .collect()
    }
}

/// Return the character a stroke types, if it is a plain or shifted printable key.
///
/// This is the implicit insertion layer that [`TextBindings`] does not model:
/// it matches the [`TextEditCommand::Insert`] results of [`command_for_key`].
#[must_use]
pub const fn typed_char_for_key(stroke: KeyStroke) -> Option<char> {
    let mods = stroke.modifiers;
    if mods.ctrl || mods.alt || mods.super_key || mods.hyper || mods.meta {
        return None;
    }
    match stroke.key {
        KeyCode::Char(ch) => Some(shifted_input_char(ch, mods.shift)),
        KeyCode::Space => Some(' '),
        _ => None,
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

    const PROFILES: [TextInputProfile; 3] = [
        TextInputProfile::Readline,
        TextInputProfile::MacTerminal,
        TextInputProfile::WindowsTerminal,
    ];
    const POLICIES: [TextBoundaryPolicy; 2] =
        [TextBoundaryPolicy::Buffer, TextBoundaryPolicy::Line];

    fn every_modifier_combination() -> impl Iterator<Item = Modifiers> {
        (0_u8..64).map(|bits| Modifiers {
            ctrl: bits & 1 != 0,
            alt: bits & 2 != 0,
            shift: bits & 4 != 0,
            super_key: bits & 8 != 0,
            hyper: bits & 16 != 0,
            meta: bits & 32 != 0,
        })
    }

    fn sample_keys() -> Vec<KeyCode> {
        let mut keys: Vec<KeyCode> = ('a'..='z').map(KeyCode::Char).collect();
        keys.extend(['0', '9', '/', '-', '['].map(KeyCode::Char));
        keys.extend([
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Escape,
            KeyCode::Space,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Insert,
            KeyCode::F(1),
        ]);
        keys
    }

    fn legacy_command(action: TextEditAction) -> Option<TextEditCommand> {
        match action {
            TextEditAction::Clear => Some(TextEditCommand::Clear),
            action => match (action.motion(), action.deletion()) {
                (Some((motion, crate::SelectionMode::Move)), _) => {
                    Some(TextEditCommand::Move(motion))
                }
                (_, Some(deletion)) => Some(TextEditCommand::Delete(deletion)),
                _ => None,
            },
        }
    }

    #[test]
    fn defaults_reproduce_command_for_key_for_every_profile() {
        for profile in PROFILES {
            for policy in POLICIES {
                let bindings = TextBindings::defaults(profile, policy);
                assert_eq!(bindings.conflicts(), Vec::new(), "{profile:?} {policy:?}");
                for code in sample_keys() {
                    for modifiers in every_modifier_combination() {
                        let stroke = stroke(code, modifiers);
                        let legacy = command_for_key(stroke, profile, policy);
                        let context = format!("{profile:?} {policy:?} {stroke:?}");
                        match bindings.resolve(stroke) {
                            Some(action)
                                if action.is_selection()
                                    || matches!(
                                        action,
                                        TextEditAction::Undo | TextEditAction::Redo
                                    ) =>
                            {
                                // Overlays replace at most the same-direction plain move.
                                match &legacy {
                                    None => {}
                                    Some(TextEditCommand::Move(motion)) => assert_eq!(
                                        action.motion().map(|(motion, _)| motion),
                                        Some(*motion),
                                        "{context}"
                                    ),
                                    Some(other) => panic!("{context} shadows {other:?}"),
                                }
                            }
                            Some(action) => {
                                assert_eq!(legacy, legacy_command(action), "{context}");
                            }
                            None => match &legacy {
                                None => {}
                                Some(TextEditCommand::Insert(ch)) => {
                                    assert_eq!(typed_char_for_key(stroke), Some(*ch), "{context}");
                                }
                                Some(other) => {
                                    let lenient = modifiers.hyper
                                        || modifiers.meta
                                        || (profile == TextInputProfile::WindowsTerminal
                                            && modifiers.ctrl
                                            && modifiers.super_key);
                                    assert!(lenient, "{context} lost {other:?}");
                                }
                            },
                        }
                        if let Some(ch) = typed_char_for_key(stroke) {
                            assert_eq!(legacy, Some(TextEditCommand::Insert(ch)), "{context}");
                            assert_eq!(bindings.resolve(stroke), None, "{context}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn default_table_highlights() {
        let readline = TextBindings::default();
        assert_eq!(readline, TextKeymap::default().bindings());
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            readline.resolve(stroke(KeyCode::Char('a'), ctrl)),
            Some(TextEditAction::MoveStart)
        );
        assert_eq!(
            readline.resolve(stroke(KeyCode::Char('z'), ctrl)),
            Some(TextEditAction::Undo)
        );
        assert_eq!(readline.strokes(TextEditAction::Undo), DEFAULT_UNDO_STROKES);
        assert_eq!(readline.strokes(TextEditAction::Redo), DEFAULT_REDO_STROKES);
        assert_eq!(
            readline.resolve(stroke(KeyCode::Left, SHIFT)),
            Some(TextEditAction::SelectLeft)
        );
        assert_eq!(readline.strokes(TextEditAction::SelectAll), []);
        assert_eq!(readline.strokes(TextEditAction::Clear), []);

        let line = TextBindings::defaults(TextInputProfile::Readline, TextBoundaryPolicy::Line);
        assert_eq!(
            line.resolve(stroke(KeyCode::Char('u'), ctrl)),
            Some(TextEditAction::DeleteToLineStart)
        );
        assert_eq!(line.strokes(TextEditAction::MoveStart), []);

        let mac = TextBindings::defaults(TextInputProfile::MacTerminal, TextBoundaryPolicy::Buffer);
        assert_eq!(
            mac.resolve(stroke(KeyCode::Left, SUPER)),
            Some(TextEditAction::MoveLineStart)
        );
        assert_eq!(readline.resolve(stroke(KeyCode::Left, SUPER)), None);
    }

    #[test]
    fn set_unbind_and_resolve() {
        let mut bindings = TextBindings::default();
        let ctrl_a = stroke(
            KeyCode::Char('a'),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        let f2 = stroke(KeyCode::F(2), Modifiers::NONE);

        bindings.set(TextEditAction::SelectAll, &[f2, f2]);
        assert_eq!(bindings.strokes(TextEditAction::SelectAll), &[f2]);
        assert_eq!(bindings.resolve(f2), Some(TextEditAction::SelectAll));

        bindings.set(TextEditAction::MoveLeft, &[]);
        assert_eq!(bindings.strokes(TextEditAction::MoveLeft), []);
        assert_eq!(
            bindings.resolve(stroke(KeyCode::Left, Modifiers::NONE)),
            None
        );

        bindings.unbind_stroke(ctrl_a);
        assert!(
            !bindings
                .strokes(TextEditAction::MoveStart)
                .contains(&ctrl_a)
        );
        assert_eq!(bindings.resolve(ctrl_a), None);
        bindings.set(TextEditAction::SelectAll, &[ctrl_a]);
        assert_eq!(bindings.resolve(ctrl_a), Some(TextEditAction::SelectAll));
        assert_eq!(bindings.resolve(f2), None);
        assert_eq!(bindings.conflicts(), Vec::new());

        let empty = TextBindings::empty();
        assert!(empty.iter().all(|(_, strokes)| strokes.is_empty()));
        assert_eq!(empty.iter().count(), TextEditAction::ALL.len());
        assert_eq!(empty.resolve(ctrl_a), None);
    }

    #[test]
    fn conflicts_are_reported_and_fail_closed() {
        let mut bindings = TextBindings::default();
        let ctrl_a = stroke(
            KeyCode::Char('a'),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        let f3 = stroke(KeyCode::F(3), Modifiers::NONE);
        bindings.set(TextEditAction::SelectAll, &[ctrl_a]);
        bindings.set(TextEditAction::Undo, &[f3]);
        bindings.set(TextEditAction::Redo, &[f3]);
        assert_eq!(bindings.resolve(ctrl_a), None);
        assert_eq!(bindings.resolve(f3), None);
        let mut conflicts = bindings.conflicts();
        conflicts.sort();
        let mut expected = vec![
            (
                ctrl_a,
                vec![TextEditAction::MoveStart, TextEditAction::SelectAll],
            ),
            (f3, vec![TextEditAction::Undo, TextEditAction::Redo]),
        ];
        expected.sort();
        assert_eq!(conflicts, expected);
    }

    #[test]
    fn typed_chars_are_implicit() {
        assert_eq!(
            typed_char_for_key(stroke(KeyCode::Char('q'), SHIFT)),
            Some('Q')
        );
        assert_eq!(
            typed_char_for_key(stroke(KeyCode::Space, Modifiers::NONE)),
            Some(' ')
        );
        assert_eq!(typed_char_for_key(stroke(KeyCode::Char('q'), ALT)), None);
        assert_eq!(
            typed_char_for_key(stroke(KeyCode::Enter, Modifiers::NONE)),
            None
        );
    }
}
