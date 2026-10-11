//! Private terminal ingress. Route here before tracing or ordinary event admission.
//!
//! The capture owner must acknowledge suppression before acquisition, and must
//! drain both decoded and raw pending input before resuming. Dropping a lease
//! deliberately does NOT resume capture or ordinary input.
use std::fmt;
use std::sync::{Arc, Mutex};

use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::sensitive_input::{PrivateInputBytes, SensitiveInputController};
use zeroize::Zeroize;

/// Owner of every input capture boundary surrounding this terminal.
/// Implementations must suppress raw recordings AND key/paste traces upstream.
/// Unsupported enclosing terminals must refuse acquisition.
pub trait CaptureSuppression: Send {
    /// Acknowledge suppression before private focus. No secret has been accepted yet.
    /// Establish an ingress barrier and discard pending raw, decoded and application
    /// input BEFORE acknowledging; previously queued public keys must not reach
    /// a background composer after private focus. A boolean/config flag is not proof.
    /// # Errors
    /// Return an error if any capture boundary cannot be suppressed.
    fn suppress(&mut self) -> Result<(), PrivateInputError>;
    /// Drain raw, decoded, and application-pending input, then restore capture.
    /// # Errors
    /// Return an error without restoring capture if the barrier cannot be proved.
    fn drain_and_resume(&mut self) -> Result<(), PrivateInputError>;
}

/// Secret-free error vocabulary. Do not attach backend errors or event contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateInputError {
    /// A private lease or quarantine is already present.
    Busy,
    /// Enclosing capture boundaries cannot be suppressed.
    Unsupported,
    /// Synchronization or drain proof failed. Input remains quarantined.
    BarrierFailed,
}

struct State {
    capture: Box<dyn CaptureSuppression>,
    controller: Option<SensitiveInputController>,
    quarantined: bool,
}

/// Serialized input gate shared by the ingress reader and the active local UI.
#[derive(Clone)]
pub struct PrivateInputGate(Arc<Mutex<State>>);
impl fmt::Debug for PrivateInputGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateInputGate([REDACTED])")
    }
}
impl PrivateInputGate {
    /// Install an explicit capture owner. There is no permissive default.
    #[must_use]
    pub fn new(capture: impl CaptureSuppression + 'static) -> Self {
        Self(Arc::new(Mutex::new(State {
            capture: Box::new(capture),
            controller: None,
            quarantined: false,
        })))
    }
    /// Suppress all capture before creating a controller or granting private focus.
    /// # Errors
    /// Refuses busy, unsupported, or poisoned capture ownership.
    pub fn acquire(&self) -> Result<PrivateInputLease, PrivateInputError> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| PrivateInputError::BarrierFailed)?;
        if state.quarantined {
            return Err(PrivateInputError::Busy);
        }
        // Quarantine first: partial suppression failure must never enable ordinary input.
        state.quarantined = true;
        state.capture.suppress()?;
        state.controller = Some(SensitiveInputController::new());
        drop(state);
        Ok(PrivateInputLease { gate: self.clone() })
    }
    /// Route an owned event before any diagnostic, cloning, or normal admission.
    /// Secret key/paste events never leave this function while the gate is private.
    #[must_use]
    pub fn route(&self, mut event: Event) -> PrivateInputRoute {
        let Ok(mut state) = self.0.lock() else {
            wipe_event(&mut event);
            return PrivateInputRoute::Consumed;
        };
        if !state.quarantined {
            return PrivateInputRoute::Public(event);
        }
        let Some(controller) = state.controller.as_mut() else {
            wipe_event(&mut event);
            return PrivateInputRoute::Consumed;
        };
        match &mut event {
            Event::Paste(text) => {
                controller.insert(text);
                text.zeroize();
            }
            Event::Key(stroke) => {
                use bmux_keyboard::KeyCode;
                match stroke.key {
                    KeyCode::Char(ch)
                        if !stroke.modifiers.ctrl
                            && !stroke.modifiers.alt
                            && !stroke.modifiers.super_key
                            && !stroke.modifiers.hyper
                            && !stroke.modifiers.meta =>
                    {
                        let mut bytes = [0; 4];
                        let ch = if stroke.modifiers.shift && ch.is_ascii_alphabetic() {
                            ch.to_ascii_uppercase()
                        } else {
                            ch
                        };
                        controller.insert(ch.encode_utf8(&mut bytes));
                        bytes.zeroize();
                    }
                    KeyCode::Space => {
                        controller.insert(" ");
                    }
                    KeyCode::Backspace => controller.backspace(),
                    KeyCode::Enter => {
                        if let Some(bytes) = controller.submit() {
                            wipe_event(&mut event);
                            return PrivateInputRoute::Submitted(bytes);
                        }
                    }
                    KeyCode::Escape => {
                        controller.close();
                        wipe_event(&mut event);
                        return PrivateInputRoute::Cancelled;
                    }
                    _ => {}
                }
            }
            Event::Focus(FocusEvent::Lost) => {
                controller.close();
                return PrivateInputRoute::Cancelled;
            }
            // Non-text events can update geometry; they cannot release private focus.
            Event::Resize(_) | Event::Focus(FocusEvent::Gained) | Event::Tick => {
                return PrivateInputRoute::Public(event);
            }
            // Private input is modal: no background hit target receives pointer input.
            Event::Mouse(_) => {}
            Event::User(text) => {
                text.zeroize();
            }
        }
        wipe_event(&mut event);
        PrivateInputRoute::Consumed
    }
    /// Resume only after the lease has closed and all late-input queues are drained.
    /// Call from the input owner, never from a renderer's paint callback.
    /// # Errors
    /// Returns `Busy` for an active lease; failed drains preserve quarantine.
    pub fn drain_and_resume(&self) -> Result<(), PrivateInputError> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| PrivateInputError::BarrierFailed)?;
        if state.controller.is_some() {
            return Err(PrivateInputError::Busy);
        }
        if state.quarantined {
            state.capture.drain_and_resume()?;
            state.quarantined = false;
        }
        drop(state);
        Ok(())
    }
}

fn wipe_event(event: &mut Event) {
    match event {
        Event::Paste(text) | Event::User(text) => text.zeroize(),
        Event::Key(stroke) => stroke.key = bmux_keyboard::KeyCode::Escape,
        _ => {}
    }
}

/// Exclusive local lifetime. Drop wipes the controller and leaves late input quarantined.
pub struct PrivateInputLease {
    gate: PrivateInputGate,
}
impl fmt::Debug for PrivateInputLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateInputLease([REDACTED])")
    }
}
impl Drop for PrivateInputLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.0.lock()
            && let Some(mut controller) = state.controller.take()
        {
            controller.close();
        }
    }
}

/// Private ingress outcome. Never serialize or persist this value.
pub enum PrivateInputRoute {
    /// Secret-free event admitted to ordinary input processing.
    Public(Event),
    /// Private input consumed without exporting text or length.
    Consumed,
    /// Exactly one private delivery. Drop immediately after writing privately.
    Submitted(PrivateInputBytes),
    /// Cancel/focus loss. Owner drops the lease and executes the drain barrier.
    Cancelled,
}
impl fmt::Debug for PrivateInputRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Public(_) => "PrivateInputRoute::Public([REDACTED])",
            Self::Consumed => "PrivateInputRoute::Consumed",
            Self::Submitted(_) => "PrivateInputRoute::Submitted([REDACTED])",
            Self::Cancelled => "PrivateInputRoute::Cancelled",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Capture {
        fail_resume: bool,
    }
    impl CaptureSuppression for Capture {
        fn suppress(&mut self) -> Result<(), PrivateInputError> {
            Ok(())
        }
        fn drain_and_resume(&mut self) -> Result<(), PrivateInputError> {
            if self.fail_resume {
                Err(PrivateInputError::BarrierFailed)
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn drop_quarantines_late_input_until_explicit_barrier() {
        let gate = PrivateInputGate::new(Capture { fail_resume: false });
        let lease = gate.acquire().unwrap();
        assert!(matches!(
            gate.route(Event::Paste("synthetic".into())),
            PrivateInputRoute::Consumed
        ));
        drop(lease);
        assert!(matches!(
            gate.route(Event::Paste("late".into())),
            PrivateInputRoute::Consumed
        ));
        gate.drain_and_resume().unwrap();
        assert!(matches!(
            gate.route(Event::Paste("public".into())),
            PrivateInputRoute::Public(_)
        ));
    }
    #[test]
    fn private_delivery_is_one_shot_and_never_public() {
        use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
        let gate = PrivateInputGate::new(Capture { fail_resume: false });
        let _lease = gate.acquire().unwrap();
        let shifted = Event::Key(KeyStroke::with_modifiers(
            KeyCode::Char('a'),
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        ));
        assert!(matches!(gate.route(shifted), PrivateInputRoute::Consumed));
        assert!(matches!(
            gate.route(Event::Paste("界e\u{301}".into())),
            PrivateInputRoute::Consumed
        ));
        let _ = gate.route(Event::Key(KeyStroke::simple(KeyCode::Backspace)));
        let submitted = gate.route(Event::Key(KeyStroke::simple(KeyCode::Enter)));
        let PrivateInputRoute::Submitted(bytes) = submitted else {
            panic!("expected private delivery");
        };
        assert_eq!(bytes.expose(), "A界".as_bytes());
        assert!(matches!(
            gate.route(Event::Key(KeyStroke::simple(KeyCode::Enter))),
            PrivateInputRoute::Consumed
        ));
        assert!(matches!(
            gate.route(Event::Paste("late".into())),
            PrivateInputRoute::Consumed
        ));
    }
    #[test]
    fn unsupported_suppression_never_grants_private_focus() {
        struct Unsupported;
        impl CaptureSuppression for Unsupported {
            fn suppress(&mut self) -> Result<(), PrivateInputError> {
                Err(PrivateInputError::Unsupported)
            }
            fn drain_and_resume(&mut self) -> Result<(), PrivateInputError> {
                Err(PrivateInputError::BarrierFailed)
            }
        }
        let gate = PrivateInputGate::new(Unsupported);
        assert!(matches!(
            gate.acquire(),
            Err(PrivateInputError::Unsupported)
        ));
        assert!(matches!(
            gate.route(Event::Paste("late".into())),
            PrivateInputRoute::Consumed
        ));
    }
    #[test]
    fn failed_barrier_never_restores_public_input() {
        let gate = PrivateInputGate::new(Capture { fail_resume: true });
        drop(gate.acquire().unwrap());
        assert_eq!(
            gate.drain_and_resume(),
            Err(PrivateInputError::BarrierFailed)
        );
        assert!(matches!(
            gate.route(Event::Paste("late".into())),
            PrivateInputRoute::Consumed
        ));
        assert!(matches!(gate.acquire(), Err(PrivateInputError::Busy)));
    }
}
