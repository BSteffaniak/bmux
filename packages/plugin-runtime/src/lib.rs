//! Host-owned plugin runtime primitives.
//!
//! This crate is intentionally domain-agnostic. It contains scheduling,
//! cancellation, deadline, and backpressure primitives owned by the BMUX host
//! runtime rather than plugin API contracts or product-domain plugins.

use serde::{Deserialize, Serialize};
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// Plugin or service concurrency policy.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum PluginConcurrencyConfig {
    /// No scheduler-imposed concurrency limit.
    #[default]
    Concurrent,
    /// One invocation at a time.
    Exclusive,
    /// At most `max` concurrent invocations.
    Limited { max: NonZeroUsize },
}

impl PluginConcurrencyConfig {
    /// Return the number of permits needed by this policy, if bounded.
    #[must_use]
    pub const fn permit_limit(self) -> Option<NonZeroUsize> {
        match self {
            Self::Concurrent => None,
            Self::Exclusive => NonZeroUsize::new(1),
            Self::Limited { max } => Some(max),
        }
    }
}

/// Effective runtime concurrency policy resolved from plugin/service metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveConcurrencyPolicy {
    pub plugin: PluginConcurrencyConfig,
    pub service: Option<PluginConcurrencyConfig>,
}

impl EffectiveConcurrencyPolicy {
    #[must_use]
    pub const fn new(plugin: PluginConcurrencyConfig) -> Self {
        Self {
            plugin,
            service: None,
        }
    }

    #[must_use]
    pub const fn with_service_override(
        plugin: PluginConcurrencyConfig,
        service: PluginConcurrencyConfig,
    ) -> Self {
        Self {
            plugin,
            service: Some(service),
        }
    }

    #[must_use]
    pub const fn effective(self) -> PluginConcurrencyConfig {
        match self.service {
            Some(service) => service,
            None => self.plugin,
        }
    }
}

/// Host scheduling errors.
#[derive(Debug, thiserror::Error)]
pub enum PluginRuntimeError {
    #[error("plugin scheduler lock poisoned")]
    SchedulerLockPoisoned,
}

/// Small host-owned blocking scheduler gate.
#[derive(Debug, Clone)]
pub struct ConcurrencyGate {
    policy: PluginConcurrencyConfig,
    state: Arc<GateState>,
}

#[derive(Debug)]
struct GateState {
    permits: Mutex<usize>,
    available: Condvar,
}

impl ConcurrencyGate {
    #[must_use]
    pub fn new(policy: PluginConcurrencyConfig) -> Self {
        let permits = policy.permit_limit().map_or(usize::MAX, NonZeroUsize::get);
        Self {
            policy,
            state: Arc::new(GateState {
                permits: Mutex::new(permits),
                available: Condvar::new(),
            }),
        }
    }

    #[must_use]
    pub const fn policy(&self) -> PluginConcurrencyConfig {
        self.policy
    }

    /// Acquire a scheduler permit, blocking until one is available.
    ///
    /// # Errors
    ///
    /// Returns [`PluginRuntimeError::SchedulerLockPoisoned`] if the scheduler
    /// state mutex or condition variable is poisoned.
    pub fn acquire(&self) -> Result<ConcurrencyPermit, PluginRuntimeError> {
        if self.policy.permit_limit().is_none() {
            return Ok(ConcurrencyPermit { gate: None });
        }

        let mut permits = self.lock_permits()?;
        while *permits == 0 {
            permits = self
                .state
                .available
                .wait(permits)
                .map_err(|_| PluginRuntimeError::SchedulerLockPoisoned)?;
        }
        *permits = permits.saturating_sub(1);
        Ok(ConcurrencyPermit {
            gate: Some(self.clone()),
        })
    }

    fn release(&self) {
        if self.policy.permit_limit().is_none() {
            return;
        }
        if let Ok(mut permits) = self.state.permits.lock() {
            *permits = permits.saturating_add(1);
            drop(permits);
            self.state.available.notify_one();
        }
    }

    fn lock_permits(&self) -> Result<MutexGuard<'_, usize>, PluginRuntimeError> {
        self.state
            .permits
            .lock()
            .map_err(|_| PluginRuntimeError::SchedulerLockPoisoned)
    }
}

/// RAII scheduler permit.
#[derive(Debug)]
pub struct ConcurrencyPermit {
    gate: Option<ConcurrencyGate>,
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConcurrencyGate, EffectiveConcurrencyPolicy, PluginConcurrencyConfig};
    use std::num::NonZeroUsize;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    use std::thread;

    #[test]
    fn service_override_beats_plugin_default() {
        let policy = EffectiveConcurrencyPolicy::with_service_override(
            PluginConcurrencyConfig::Exclusive,
            PluginConcurrencyConfig::Concurrent,
        );
        assert_eq!(policy.effective(), PluginConcurrencyConfig::Concurrent);
    }

    #[test]
    fn exclusive_serializes_calls() {
        let gate = ConcurrencyGate::new(PluginConcurrencyConfig::Exclusive);
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let gate = gate.clone();
            let active = active.clone();
            let max_active = max_active.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                let _permit = gate.acquire().expect("permit should acquire");
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(current, Ordering::SeqCst);
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle.join().expect("worker should finish");
        }
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn limited_allows_exactly_max() {
        let gate = ConcurrencyGate::new(PluginConcurrencyConfig::Limited {
            max: NonZeroUsize::new(2).expect("nonzero"),
        });
        let first = gate.acquire().expect("first permit");
        let second = gate.acquire().expect("second permit");
        drop(first);
        let third = gate.acquire().expect("third permit after release");
        drop(second);
        drop(third);
    }
}
