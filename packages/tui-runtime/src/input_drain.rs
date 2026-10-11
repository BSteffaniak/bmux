//! Bounded ingress drain barriers for private-focus transitions.
//!
//! Run on the exclusive input-reader task while ordinary admission is paused.
//! Decoded quiescence alone does not prove the operating-system/backend queue
//! empty: a concrete backend fence and application admission fence are required.
use bmux_tui::event::Event;
use futures_util::{FutureExt, Stream, StreamExt};
use zeroize::Zeroize;

/// Backend/admission owners must provide real fences, never configuration flags.
pub trait InputDrainOwner {
    /// Stop raw input admission and discard pending raw/backend input.
    /// # Errors
    /// Return an error when exclusive raw input ownership or draining is unavailable.
    fn drain_raw(&mut self) -> Result<(), DrainError>;
    /// Invalidate previously admitted application keys/pastes and acknowledge that
    /// none can reach background controls after the transition.
    /// # Errors
    /// Return an error if application admission has not crossed the fence.
    fn fence_application(&mut self) -> Result<(), DrainError>;
}

/// Secret-free drain failure; the caller keeps admission quarantined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainError {
    /// Input cannot be drained within the explicit work bound.
    BudgetExceeded,
    /// Input backend ended or failed; do not restore private/public focus.
    BackendUnavailable,
    /// Raw/application ownership is not proved.
    UnprovedOwnership,
}

/// Drain a paused decoded stream and its owning raw/application boundaries.
///
/// Success is valid only while the caller retains exclusive ingress ownership.
/// Pending input is wiped without logging, rendering or ordinary admission.
///
/// # Errors
/// Returns backend/fence failures or budget exhaustion. Failure never authorizes resume.
pub fn drain_paused<S: Stream<Item = Result<Event, std::io::Error>> + Unpin>(
    stream: &mut S,
    owner: &mut impl InputDrainOwner,
    max_events: usize,
) -> Result<(), DrainError> {
    owner.drain_raw()?;
    for _ in 0..max_events {
        match stream.next().now_or_never() {
            None => return owner.fence_application(),
            Some(Some(Ok(mut event))) => wipe(&mut event),
            Some(Some(Err(_)) | None) => return Err(DrainError::BackendUnavailable),
        }
    }
    Err(DrainError::BudgetExceeded)
}

fn wipe(event: &mut Event) {
    match event {
        Event::Paste(text) | Event::User(text) => text.zeroize(),
        Event::Key(key) => key.key = bmux_keyboard::KeyCode::Escape,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Owner {
        raw: bool,
        application: bool,
    }
    impl InputDrainOwner for Owner {
        fn drain_raw(&mut self) -> Result<(), DrainError> {
            self.raw = true;
            Ok(())
        }
        fn fence_application(&mut self) -> Result<(), DrainError> {
            assert!(self.raw);
            self.application = true;
            Ok(())
        }
    }
    #[test]
    fn paused_stream_discards_private_canary_before_application_fence() {
        let mut stream =
            futures_util::stream::iter([Ok(Event::Paste("synthetic-private-canary".into()))])
                .chain(futures_util::stream::pending());
        let mut owner = Owner {
            raw: false,
            application: false,
        };
        assert_eq!(drain_paused(&mut stream, &mut owner, 4), Ok(()));
        assert!(owner.application);
    }
    #[test]
    fn bounded_drain_never_acknowledges_busy_or_failed_stream() {
        let mut stream = futures_util::stream::repeat_with(|| Ok(Event::Paste("canary".into())));
        let mut owner = Owner {
            raw: false,
            application: false,
        };
        assert_eq!(
            drain_paused(&mut stream, &mut owner, 4),
            Err(DrainError::BudgetExceeded)
        );
        assert!(!owner.application);
    }
}
