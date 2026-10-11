//! Concrete Linux raw-reader capture barrier.
//!
//! Applications install this only when the Linux reader owns terminal input from
//! startup, never after Crossterm.
use crate::private_input::{CaptureSuppression, PrivateInputError};
use bmux_tui::linux_input::LinuxInputReader;
use std::sync::{Arc, Mutex};

/// Owner of enclosing recording/tracing and the application's event epoch.
/// Implementations must perform actual acknowledged barriers, not set flags.
/// # Safety
/// Implementations must own and synchronously acknowledge ALL recording and
/// tracing boundaries upstream of this reader. Epoch fencing must serialize
/// event admission with the barrier and wipe stale queued input. A best-effort
/// toggle, timeout without ack, or unverifiable outer terminal is not sufficient.
/// Return Unsupported when these ownership obligations cannot be proved.
pub unsafe trait LinuxCaptureBoundary: Send {
    /// Stop every upstream recorder/trace before private focus or raw reads.
    /// # Errors
    /// Unsupported/unacknowledged boundaries must fail closed.
    fn suppress_capture(&mut self) -> Result<(), PrivateInputError>;
    /// Advance application ingress epoch and wipe/drop all prior queued key/paste
    /// events and continuations. Late prior-epoch events must never be admitted.
    /// # Errors
    /// Failure leaves recording suppressed and the raw owner paused.
    fn fence_application_epoch(&mut self, epoch: u64) -> Result<(), PrivateInputError>;
    /// Restore recording only after raw reset and epoch fence acknowledge.
    /// # Errors
    /// Failure leaves the private gate quarantined and raw reader paused.
    fn restore_capture(&mut self) -> Result<(), PrivateInputError>;
}

/// Shared raw-reader owner plus an acknowledged recording/application boundary.
///
/// Route every event obtained from the shared reader into `PrivateInputGate` before
/// any normal tracing/admission. No other code may call unsafe raw resume.
pub struct LinuxCaptureOwner<B> {
    reader: Arc<Mutex<LinuxInputReader>>,
    boundary: B,
}
impl<B: LinuxCaptureBoundary> LinuxCaptureOwner<B> {
    /// Bind an existing startup owner to its actual enclosing capture owners.
    #[must_use]
    pub const fn new(reader: Arc<Mutex<LinuxInputReader>>, boundary: B) -> Self {
        Self { reader, boundary }
    }
    fn fence(&mut self, entering: bool) -> Result<(), PrivateInputError> {
        let mut reader = self
            .reader
            .lock()
            .map_err(|_| PrivateInputError::BarrierFailed)?;
        fence_reader(&mut *reader, &mut self.boundary, entering)?;
        drop(reader);
        Ok(())
    }
}
impl<B: LinuxCaptureBoundary> CaptureSuppression for LinuxCaptureOwner<B> {
    fn suppress(&mut self) -> Result<(), PrivateInputError> {
        self.fence(true)
    }
    fn drain_and_resume(&mut self) -> Result<(), PrivateInputError> {
        self.fence(false)
    }
}

// Internal seam to test ordering/failure semantics independently of the Linux
// PTY drain tests. This is not an alternate public raw-reader implementation.
trait RawFence {
    fn pause(&mut self);
    fn reset(&mut self) -> Result<(), PrivateInputError>;
    fn epoch(&self) -> u64;
    fn resume(&mut self);
}
impl RawFence for LinuxInputReader {
    fn pause(&mut self) {
        self.pause();
    }
    fn reset(&mut self) -> Result<(), PrivateInputError> {
        self.drain_and_reset()
            .map_err(|_| PrivateInputError::BarrierFailed)
    }
    fn epoch(&self) -> u64 {
        self.epoch()
    }
    fn resume(&mut self) {
        // SAFETY: only called after all acknowledged barriers below, with the
        // raw reader exclusively locked by LinuxCaptureOwner throughout.
        unsafe {
            self.resume_after_fences();
        }
    }
}
fn fence_reader(
    reader: &mut impl RawFence,
    boundary: &mut impl LinuxCaptureBoundary,
    entering: bool,
) -> Result<(), PrivateInputError> {
    reader.pause();
    if entering {
        boundary.suppress_capture()?;
    }
    reader.reset()?;
    boundary.fence_application_epoch(reader.epoch())?;
    if !entering {
        boundary.restore_capture()?;
    }
    reader.resume();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Raw {
        paused: bool,
        epoch: u64,
        fail: bool,
    }
    impl RawFence for Raw {
        fn pause(&mut self) {
            self.paused = true;
        }
        fn reset(&mut self) -> Result<(), PrivateInputError> {
            assert!(self.paused);
            if self.fail {
                return Err(PrivateInputError::BarrierFailed);
            }
            self.epoch += 1;
            Ok(())
        }
        fn epoch(&self) -> u64 {
            self.epoch
        }
        fn resume(&mut self) {
            self.paused = false;
        }
    }
    #[derive(Default)]
    struct Boundary {
        calls: Vec<&'static str>,
        fail: Option<&'static str>,
    }
    impl Boundary {
        fn call(&mut self, name: &'static str) -> Result<(), PrivateInputError> {
            self.calls.push(name);
            if self.fail == Some(name) {
                Err(PrivateInputError::Unsupported)
            } else {
                Ok(())
            }
        }
    }
    // SAFETY: this isolated sequencing test has no terminal or external capture;
    // it verifies the orchestration contract, not actual recorder suppression.
    unsafe impl LinuxCaptureBoundary for Boundary {
        fn suppress_capture(&mut self) -> Result<(), PrivateInputError> {
            self.call("suppress")
        }
        fn fence_application_epoch(&mut self, epoch: u64) -> Result<(), PrivateInputError> {
            assert!(epoch > 0);
            self.call("epoch")
        }
        fn restore_capture(&mut self) -> Result<(), PrivateInputError> {
            self.call("restore")
        }
    }
    #[test]
    fn acknowledged_entry_exit_follow_raw_and_application_fences() {
        let mut raw = Raw::default();
        let mut boundary = Boundary::default();
        assert_eq!(fence_reader(&mut raw, &mut boundary, true), Ok(()));
        assert_eq!(boundary.calls, ["suppress", "epoch"]);
        assert!(!raw.paused);
        assert_eq!(fence_reader(&mut raw, &mut boundary, false), Ok(()));
        assert_eq!(boundary.calls, ["suppress", "epoch", "epoch", "restore"]);
        assert_eq!(raw.epoch, 2);
        assert!(!raw.paused);
    }
    #[test]
    fn any_unacknowledged_boundary_leaves_raw_ingress_paused() {
        for (entering, fail) in [
            (true, "suppress"),
            (true, "epoch"),
            (false, "epoch"),
            (false, "restore"),
        ] {
            let mut raw = Raw::default();
            let mut boundary = Boundary {
                fail: Some(fail),
                ..Boundary::default()
            };
            assert_eq!(
                fence_reader(&mut raw, &mut boundary, entering),
                Err(PrivateInputError::Unsupported)
            );
            assert!(raw.paused);
        }
        let mut raw = Raw {
            fail: true,
            ..Raw::default()
        };
        let mut boundary = Boundary::default();
        assert_eq!(
            fence_reader(&mut raw, &mut boundary, false),
            Err(PrivateInputError::BarrierFailed)
        );
        assert!(raw.paused);
        assert!(boundary.calls.is_empty());
    }
}
