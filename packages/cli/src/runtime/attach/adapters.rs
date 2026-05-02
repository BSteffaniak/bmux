use super::input::{TerminalGeometry, TerminalInputEvent};
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

pub type AttachAdapterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub trait AttachClock {
    fn now(&self) -> Instant;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemAttachClock;

impl AttachClock for SystemAttachClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FixedAttachClock {
    now: Instant,
}

impl FixedAttachClock {
    #[must_use]
    pub const fn new(now: Instant) -> Self {
        Self { now }
    }

    pub const fn set_now(&mut self, now: Instant) {
        self.now = now;
    }

    pub fn advance(&mut self, duration: Duration) {
        self.now += duration;
    }
}

impl AttachClock for FixedAttachClock {
    fn now(&self) -> Instant {
        self.now
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachScheduledWake {
    pub id: String,
    pub after: Duration,
}

pub trait AttachScheduler {
    fn schedule_wake(&mut self, wake: AttachScheduledWake);
}

pub trait AttachTerminalInputSource {
    /// Return the terminal geometry observed by this input source.
    ///
    /// # Errors
    /// Returns an error when the backing terminal/input adapter cannot provide
    /// its current dimensions.
    fn geometry(&self) -> Result<TerminalGeometry>;

    /// Return the next normalized attach input event, if one is currently available.
    ///
    /// # Errors
    /// Returns an error when the backing terminal/input adapter fails while
    /// polling or decoding input.
    fn next_input(&mut self) -> Result<Option<TerminalInputEvent>>;
}

pub trait AttachRenderSink {
    fn write_frame<'a>(&'a mut self, bytes: &'a [u8]) -> AttachAdapterFuture<'a, ()>;

    fn flush(&mut self) -> AttachAdapterFuture<'_, ()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachServiceCall {
    pub service: String,
    pub operation: String,
    pub payload: Vec<u8>,
}

pub trait AttachServiceHost {
    fn call_service(&mut self, call: AttachServiceCall) -> AttachAdapterFuture<'_, Vec<u8>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachStorageKey {
    pub namespace: String,
    pub key: String,
}

pub trait AttachStorage {
    fn get(&mut self, key: AttachStorageKey) -> AttachAdapterFuture<'_, Option<Vec<u8>>>;

    fn set(&mut self, key: AttachStorageKey, value: Vec<u8>) -> AttachAdapterFuture<'_, ()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachEventEnvelope {
    pub topic: String,
    pub payload: Vec<u8>,
}

pub trait AttachEventBus {
    fn publish(&mut self, event: AttachEventEnvelope) -> AttachAdapterFuture<'_, ()>;
}

#[cfg(test)]
mod tests {
    use super::{AttachClock, FixedAttachClock};
    use std::time::{Duration, Instant};

    #[test]
    fn fixed_attach_clock_can_advance_deterministically() {
        let start = Instant::now();
        let mut clock = FixedAttachClock::new(start);

        assert_eq!(clock.now(), start);
        clock.advance(Duration::from_millis(25));
        assert_eq!(clock.now(), start + Duration::from_millis(25));
    }
}
