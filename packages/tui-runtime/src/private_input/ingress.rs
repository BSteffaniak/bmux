//! Single raw reader with synchronous capture barriers and generation-fenced admission.
use super::{CaptureSuppression, PrivateInputError, PrivateInputGate, PrivateInputRoute};
use bmux_tui::crossterm::input::RawInput;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(test)]
mod tests;

struct Generation {
    value: AtomicU64,
    paused: AtomicBool,
}
impl Generation {
    fn current(&self, generation: u64) -> bool {
        self.value.load(Ordering::SeqCst) == generation && !self.paused.load(Ordering::SeqCst)
    }
    fn fence(&self) -> Result<(), PrivateInputError> {
        self.paused.store(true, Ordering::SeqCst);
        self.value
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |g| g.checked_add(1))
            .map(|_| ())
            .map_err(|_| PrivateInputError::BarrierFailed)
    }
}

struct Owner {
    reader: Mutex<RawInput>,
    generation: Generation,
}

struct Barrier {
    owner: Arc<Owner>,
    capture: Box<dyn CaptureSuppression>,
}
impl Barrier {
    fn fence(&mut self, entering: bool) -> Result<(), PrivateInputError> {
        let mut reader = self
            .owner
            .reader
            .lock()
            .map_err(|_| PrivateInputError::BarrierFailed)?;
        // Fence before any potentially failing acknowledgment, including partial suppression.
        self.owner.generation.fence()?;
        reader
            .drain_and_reset()
            .map_err(|_| PrivateInputError::BarrierFailed)?;
        if entering {
            self.capture.suppress_with_reader(&mut reader)?;
        }
        reader
            .drain_and_reset()
            .map_err(|_| PrivateInputError::BarrierFailed)?;
        if !entering {
            self.capture.resume_with_reader(&mut reader)?;
            reader
                .drain_and_reset()
                .map_err(|_| PrivateInputError::BarrierFailed)?;
        }
        self.owner.generation.paused.store(false, Ordering::SeqCst);
        // Keep the raw reader exclusively locked through capture restoration.
        drop(reader);
        Ok(())
    }
}
impl CaptureSuppression for Barrier {
    fn suppress(&mut self) -> Result<(), PrivateInputError> {
        self.fence(true)
    }
    fn drain_and_resume(&mut self) -> Result<(), PrivateInputError> {
        self.fence(false)
    }
}

/// Generation-tagged event. Consume at the serialized application boundary before
/// tracing or normal input processing. Drop wipes private or stale payloads.
pub struct IngressEvent {
    generation: u64,
    route: Option<PrivateInputRoute>,
}
impl IngressEvent {
    /// Run serialized application admission while holding the raw barrier lock.
    /// `admit` must not acquire or release a private lease. This closes the race
    /// between a generation check and the application's ordinary event processing.
    pub fn with_current<R>(
        mut self,
        ingress: &PrivateIngress,
        admit: impl FnOnce(PrivateInputRoute) -> R,
    ) -> Option<R> {
        let _reader = ingress.owner.reader.lock().ok()?;
        if ingress.owner.generation.current(self.generation) {
            self.route.take().map(admit)
        } else {
            None
        }
    }
}
impl Drop for IngressEvent {
    fn drop(&mut self) {
        if let Some(route) = self.route.take() {
            route.discard();
        }
    }
}

/// Cloneable control handle, never a second reader. All decoding and capture
/// barriers serialize on the same raw owner. There is no raw input recording here.
#[derive(Clone)]
pub struct PrivateIngress {
    owner: Arc<Owner>,
    gate: PrivateInputGate,
}
impl PrivateIngress {
    /// Adopt the sole startup reader and the owner of every cooperating capture surface.
    /// This must replace, never accompany, Crossterm's global event reader.
    /// Opaque protocol frames are discarded and wiped: this source is for applications
    /// without outstanding terminal query ownership, not a transparent protocol proxy.
    #[must_use]
    pub fn new(reader: RawInput, capture: impl CaptureSuppression + 'static) -> Self {
        let owner = Arc::new(Owner {
            reader: Mutex::new(reader),
            generation: Generation {
                value: AtomicU64::new(0),
                paused: AtomicBool::new(false),
            },
        });
        let gate = PrivateInputGate::new(Barrier {
            owner: Arc::clone(&owner),
            capture: Box::new(capture),
        });
        Self { owner, gate }
    }
    /// Private focus may be acquired only through this owner's real barrier.
    #[must_use]
    pub const fn gate(&self) -> &PrivateInputGate {
        &self.gate
    }

    /// Poll the sole raw reader. Call from one ingress task only. No event tracing,
    /// cloning or ordinary admission occurs before private routing.
    /// # Errors
    /// Reports normalized terminal/poison failures; the caller must stop admission.
    pub fn read_ready(&self) -> std::io::Result<Option<IngressEvent>> {
        let (packet, generation) = {
            let mut reader = self
                .owner
                .reader
                .lock()
                .map_err(|_| std::io::Error::other("terminal ingress unavailable"))?;
            if self.owner.generation.paused.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let generation = self.owner.generation.value.load(Ordering::SeqCst);
            (reader.read_ready(std::time::Instant::now())?, generation)
        };
        let Some(packet) = packet else {
            return Ok(None);
        };
        let Some(event) = packet.into_event() else {
            return Ok(None);
        };
        let route = self
            .gate
            .route_checked(event, || self.owner.generation.current(generation));
        if matches!(route, PrivateInputRoute::Consumed) {
            return Ok(None);
        }
        Ok(Some(IngressEvent {
            generation,
            route: Some(route),
        }))
    }
}
