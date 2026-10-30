//! Winit translation quarantine. Public events contain only BMUX-owned types.
//! The adapter is window-local and resets modifier/composition state on focus loss.

use crate::ime::ImeState;
pub use bmux_frontend_runtime::input::{
    ImeEvent, InputEvent, KeyEvent, KeyPhase, LogicalKey, Modifiers, NamedKey, PhysicalKey,
    PhysicalPoint, PointerButton, ScrollDelta,
};
use winit::event::{ElementState, Ime, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key, ModifiersState};

/// Normalize high-resolution wheel input for semantic shortcuts, not terminal
/// scrolling. One line is 40 pixels; pathological deltas are bounded per event.
/// Non-finite and horizontal-only input has no vertical zoom effect.
#[must_use]
pub fn normalized_scroll_lines(delta: ScrollDelta) -> f64 {
    let value = match delta {
        ScrollDelta::Lines { y, .. } => y,
        ScrollDelta::Pixels { y, .. } => y / 40.0,
    };
    if value.is_finite() {
        value.clamp(-32.0, 32.0)
    } else {
        0.0
    }
}

#[derive(Debug, Default)]
pub(crate) struct InputAdapter {
    modifiers: Modifiers,
    ime: ImeState,
    paste_key: Option<winit::keyboard::PhysicalKey>,
}

impl InputAdapter {
    /// Native clipboard command recognition, separate from terminal key encoding.
    /// Track the physical key so releasing modifiers first cannot leak a key-up.
    pub(crate) fn paste_action(
        &mut self,
        physical: winit::keyboard::PhysicalKey,
        logical: &Key,
        state: ElementState,
        repeat: bool,
        synthetic: bool,
    ) -> Option<bool> {
        if self.paste_key == Some(physical) {
            if state == ElementState::Released {
                self.paste_key = None;
            }
            return Some(false);
        }
        let named = matches!(logical, Key::Named(winit::keyboard::NamedKey::Paste));
        let shortcut = matches!(logical, Key::Character(text) if text.eq_ignore_ascii_case("v"))
            && paste_modifiers(self.modifiers, cfg!(target_os = "macos"));
        if !named && !shortcut {
            return None;
        }
        let invoke = !synthetic && state == ElementState::Pressed && !repeat;
        if state == ElementState::Pressed {
            self.paste_key = Some(physical);
        }
        Some(invoke)
    }

    // Winit's KeyEvent has private platform fields. This seam tests the exact
    // translation without fabricating platform-specific native event internals.
    fn key_event(
        &self,
        physical: winit::keyboard::PhysicalKey,
        logical: &Key,
        text: Option<&str>,
        state: ElementState,
        repeat: bool,
    ) -> Option<InputEvent> {
        if self.ime.composing() {
            return None;
        }
        let phase = if state == ElementState::Released {
            KeyPhase::Release
        } else if repeat {
            KeyPhase::Repeat
        } else {
            KeyPhase::Press
        };
        let physical = match physical {
            winit::keyboard::PhysicalKey::Code(code) => PhysicalKey::Code(format!("{code:?}")),
            winit::keyboard::PhysicalKey::Unidentified(_) => PhysicalKey::Unidentified,
        };
        Some(InputEvent::Key(KeyEvent {
            physical,
            logical: logical_key(logical),
            phase,
            modifiers: self.modifiers,
            text: if phase == KeyPhase::Release {
                None
            } else {
                text.map(str::to_owned)
            },
        }))
    }

    pub(crate) fn window_event(&mut self, event: &WindowEvent) -> Option<InputEvent> {
        match event {
            WindowEvent::ModifiersChanged(value) => {
                self.modifiers = modifiers(value.state());
                None
            }
            // Synthesized held-key presses on focus entry are not fresh user input.
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => self.key_event(
                event.physical_key,
                &event.logical_key,
                event.text.as_deref(),
                event.state,
                event.repeat,
            ),
            WindowEvent::Ime(event) => {
                let event = match event {
                    Ime::Enabled => ImeEvent::Enabled,
                    Ime::Disabled => ImeEvent::Disabled,
                    Ime::Preedit(text, cursor) => ImeEvent::Preedit {
                        text: text.clone(),
                        cursor: *cursor,
                    },
                    Ime::Commit(text) => ImeEvent::Commit(text.clone()),
                };
                Some(InputEvent::Ime(self.ime.update(event)))
            }
            WindowEvent::CursorMoved { position, .. } => Some(InputEvent::PointerMoved {
                position: PhysicalPoint {
                    x: position.x,
                    y: position.y,
                },
            }),
            WindowEvent::CursorLeft { .. } => Some(InputEvent::PointerLeft),
            WindowEvent::MouseInput { state, button, .. } => Some(InputEvent::PointerButton {
                button: pointer_button(*button),
                phase: if *state == ElementState::Pressed {
                    KeyPhase::Press
                } else {
                    KeyPhase::Release
                },
                modifiers: self.modifiers,
            }),
            WindowEvent::MouseWheel { delta, .. } => Some(InputEvent::Scroll {
                delta: match delta {
                    MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines {
                        x: f64::from(*x),
                        y: f64::from(*y),
                    },
                    MouseScrollDelta::PixelDelta(p) => ScrollDelta::Pixels { x: p.x, y: p.y },
                },
                modifiers: self.modifiers,
            }),
            WindowEvent::Focused(focused) => {
                if !focused {
                    self.modifiers = Modifiers::default();
                    self.paste_key = None;
                    self.ime.cancel();
                }
                Some(InputEvent::Focus(*focused))
            }
            _ => None,
        }
    }
}

fn paste_modifiers(modifiers: Modifiers, macos: bool) -> bool {
    if macos {
        modifiers.super_key && !modifiers.control && !modifiers.alt && !modifiers.shift
    } else {
        modifiers.control && modifiers.shift && !modifiers.alt && !modifiers.super_key
    }
}

fn modifiers(value: ModifiersState) -> Modifiers {
    Modifiers {
        shift: value.shift_key(),
        control: value.control_key(),
        alt: value.alt_key(),
        super_key: value.super_key(),
    }
}

fn pointer_button(value: winit::event::MouseButton) -> PointerButton {
    match value {
        winit::event::MouseButton::Left => PointerButton::Left,
        winit::event::MouseButton::Right => PointerButton::Right,
        winit::event::MouseButton::Middle => PointerButton::Middle,
        winit::event::MouseButton::Back => PointerButton::Back,
        winit::event::MouseButton::Forward => PointerButton::Forward,
        winit::event::MouseButton::Other(value) => PointerButton::Other(value),
    }
}

fn logical_key(value: &Key) -> LogicalKey {
    match value {
        Key::Character(text) => LogicalKey::Character(text.to_string()),
        Key::Dead(value) => LogicalKey::Dead(*value),
        Key::Unidentified(_) => LogicalKey::Unidentified,
        Key::Named(value) => named_key(*value).map_or(LogicalKey::Unidentified, LogicalKey::Named),
    }
}

fn named_key(value: winit::keyboard::NamedKey) -> Option<NamedKey> {
    use winit::keyboard::NamedKey as W;
    Some(match value {
        W::Enter => NamedKey::Enter,
        W::Tab => NamedKey::Tab,
        W::Backspace => NamedKey::Backspace,
        W::Delete => NamedKey::Delete,
        W::Escape => NamedKey::Escape,
        W::Space => NamedKey::Space,
        W::ArrowUp => NamedKey::Up,
        W::ArrowDown => NamedKey::Down,
        W::ArrowLeft => NamedKey::Left,
        W::ArrowRight => NamedKey::Right,
        W::Home => NamedKey::Home,
        W::End => NamedKey::End,
        W::PageUp => NamedKey::PageUp,
        W::PageDown => NamedKey::PageDown,
        W::Insert => NamedKey::Insert,
        W::CapsLock => NamedKey::CapsLock,
        W::ScrollLock => NamedKey::ScrollLock,
        W::NumLock => NamedKey::NumLock,
        W::PrintScreen => NamedKey::PrintScreen,
        W::Pause => NamedKey::Pause,
        W::ContextMenu => NamedKey::Menu,
        W::Shift => NamedKey::Shift,
        W::Control => NamedKey::Control,
        W::Alt | W::AltGraph => NamedKey::Alt,
        W::Super | W::Meta => NamedKey::Super,
        W::F1 => NamedKey::Function(1),
        W::F2 => NamedKey::Function(2),
        W::F3 => NamedKey::Function(3),
        W::F4 => NamedKey::Function(4),
        W::F5 => NamedKey::Function(5),
        W::F6 => NamedKey::Function(6),
        W::F7 => NamedKey::Function(7),
        W::F8 => NamedKey::Function(8),
        W::F9 => NamedKey::Function(9),
        W::F10 => NamedKey::Function(10),
        W::F11 => NamedKey::Function(11),
        W::F12 => NamedKey::Function(12),
        W::F13 => NamedKey::Function(13),
        W::F14 => NamedKey::Function(14),
        W::F15 => NamedKey::Function(15),
        W::F16 => NamedKey::Function(16),
        W::F17 => NamedKey::Function(17),
        W::F18 => NamedKey::Function(18),
        W::F19 => NamedKey::Function(19),
        W::F20 => NamedKey::Function(20),
        W::F21 => NamedKey::Function(21),
        W::F22 => NamedKey::Function(22),
        W::F23 => NamedKey::Function(23),
        W::F24 => NamedKey::Function(24),
        W::F25 => NamedKey::Function(25),
        W::F26 => NamedKey::Function(26),
        W::F27 => NamedKey::Function(27),
        W::F28 => NamedKey::Function(28),
        W::F29 => NamedKey::Function(29),
        W::F30 => NamedKey::Function(30),
        W::F31 => NamedKey::Function(31),
        W::F32 => NamedKey::Function(32),
        W::F33 => NamedKey::Function(33),
        W::F34 => NamedKey::Function(34),
        W::F35 => NamedKey::Function(35),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_wheel_normalization_is_fractional_finite_and_bounded() {
        assert_eq!(
            normalized_scroll_lines(ScrollDelta::Pixels { x: 50.0, y: 20.0 }),
            0.5
        );
        assert_eq!(
            normalized_scroll_lines(ScrollDelta::Lines { x: 0.0, y: -0.25 }),
            -0.25
        );
        assert_eq!(
            normalized_scroll_lines(ScrollDelta::Lines {
                x: 0.0,
                y: f64::NAN
            }),
            0.0
        );
        assert_eq!(
            normalized_scroll_lines(ScrollDelta::Lines {
                x: 0.0,
                y: f64::MAX
            }),
            32.0
        );
    }

    #[test]
    fn paste_shortcuts_are_platform_specific_and_not_terminal_ctrl_v() {
        for macos in [false, true] {
            let command = if macos {
                Modifiers {
                    super_key: true,
                    ..Modifiers::default()
                }
            } else {
                Modifiers {
                    control: true,
                    shift: true,
                    ..Modifiers::default()
                }
            };
            assert!(paste_modifiers(command, macos));
            assert!(!paste_modifiers(command, !macos));
            assert!(!paste_modifiers(
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                macos
            ));
            assert!(!paste_modifiers(
                Modifiers {
                    alt: true,
                    ..command
                },
                macos
            ));
        }
    }

    #[test]
    fn paste_shortcut_consumes_release_after_modifiers_change() {
        let modifiers = if cfg!(target_os = "macos") {
            Modifiers {
                super_key: true,
                ..Modifiers::default()
            }
        } else {
            Modifiers {
                control: true,
                shift: true,
                ..Modifiers::default()
            }
        };
        let mut adapter = InputAdapter {
            modifiers,
            ..InputAdapter::default()
        };
        let physical = winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyV);
        let logical = Key::Character("V".into());
        assert_eq!(
            adapter.paste_action(physical, &logical, ElementState::Pressed, false, false),
            Some(true)
        );
        assert_eq!(
            adapter.paste_action(physical, &logical, ElementState::Pressed, true, false),
            Some(false)
        );
        adapter.modifiers = Modifiers::default();
        assert_eq!(
            adapter.paste_action(physical, &logical, ElementState::Released, false, false),
            Some(false)
        );
        assert_eq!(
            adapter.paste_action(physical, &logical, ElementState::Pressed, false, false),
            None
        );
    }

    #[test]
    fn key_phases_and_ime_commit_have_one_text_authority() {
        let mut adapter = InputAdapter::default();
        let physical = winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyA);
        let logical = Key::Character("é".into());
        let Some(InputEvent::Key(key)) =
            adapter.key_event(physical, &logical, Some("é"), ElementState::Pressed, true)
        else {
            panic!("repeat must be translated");
        };
        assert_eq!(key.phase, KeyPhase::Repeat);
        assert_eq!(key.text.as_deref(), Some("é"));
        let Some(InputEvent::Key(key)) =
            adapter.key_event(physical, &logical, Some("é"), ElementState::Released, false)
        else {
            panic!("release must be translated");
        };
        assert_eq!(key.phase, KeyPhase::Release);
        assert_eq!(key.text, None);
        adapter.window_event(&WindowEvent::Ime(Ime::Preedit("e".into(), Some((0, 1)))));
        assert!(
            adapter
                .key_event(physical, &logical, Some("é"), ElementState::Pressed, false)
                .is_none()
        );
        assert_eq!(
            adapter.window_event(&WindowEvent::Ime(Ime::Commit("é".into()))),
            Some(InputEvent::Ime(ImeEvent::Commit("é".into())))
        );
    }

    #[test]
    fn scroll_retains_fractional_pixel_and_line_units() {
        let mut adapter = InputAdapter::default();
        let device_id = winit::event::DeviceId::dummy();
        let event = WindowEvent::MouseWheel {
            device_id,
            delta: MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.25, -1.5)),
            phase: winit::event::TouchPhase::Moved,
        };
        assert_eq!(
            adapter.window_event(&event),
            Some(InputEvent::Scroll {
                delta: ScrollDelta::Pixels { x: 0.25, y: -1.5 },
                modifiers: Modifiers::default(),
            })
        );
        let event = WindowEvent::MouseWheel {
            device_id,
            delta: MouseScrollDelta::LineDelta(0.5, -2.0),
            phase: winit::event::TouchPhase::Moved,
        };
        assert_eq!(
            adapter.window_event(&event),
            Some(InputEvent::Scroll {
                delta: ScrollDelta::Lines { x: 0.5, y: -2.0 },
                modifiers: Modifiers::default(),
            })
        );
    }

    #[test]
    fn composition_focus_and_pointer_units_are_preserved() {
        let mut adapter = InputAdapter::default();
        assert!(matches!(
            adapter.window_event(&WindowEvent::Ime(Ime::Preedit("é".into(), Some((0, 2))))),
            Some(InputEvent::Ime(ImeEvent::Preedit { .. }))
        ));
        assert!(adapter.ime.composing());
        assert_eq!(
            adapter.window_event(&WindowEvent::Focused(false)),
            Some(InputEvent::Focus(false))
        );
        assert!(!adapter.ime.composing());
        assert_eq!(adapter.modifiers, Modifiers::default());
    }

    #[test]
    fn named_and_multiscalar_keys_are_not_guessed_from_physical_position() {
        assert_eq!(
            logical_key(&Key::Character("éあ".into())),
            LogicalKey::Character("éあ".into())
        );
        assert_eq!(
            named_key(winit::keyboard::NamedKey::F35),
            Some(NamedKey::Function(35))
        );
        assert_eq!(
            pointer_button(winit::event::MouseButton::Other(8)),
            PointerButton::Other(8)
        );
    }
}
