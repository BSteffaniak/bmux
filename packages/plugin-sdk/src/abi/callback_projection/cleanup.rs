//! Retryable cleanup stays in the bounded instance owner, never forgotten pins.
use super::{Authority, Executor, WakeState};
use crate::abi::{AbiStatus, BUSY, NativeInvocationPorts, OK, PANICKED, ports::InvocationPorts};
use std::sync::{Arc, atomic::Ordering};

#[derive(Clone)]
pub(super) struct EntryRecord {
    pub lease: u64,
    pub ports: Option<Arc<InvocationPorts>>,
    pub transferred: Option<NativeInvocationPorts>,
    pub raw_output:
        Option<Arc<std::mem::MaybeUninit<crate::abi::NativeInvocationPortsV3CallbackAuthorityV1>>>,
    pub retiring: bool,
    pub unknown: bool,
}
impl EntryRecord {
    pub(super) const fn pending() -> Self {
        Self {
            lease: 0,
            ports: None,
            transferred: None,
            raw_output: None,
            retiring: false,
            unknown: false,
        }
    }
}
#[derive(Clone)]
pub(super) struct SubscriptionRecord {
    pub id: u64,
    pub state: Arc<WakeState>,
    pub retiring: bool,
    pub unknown: bool,
}
impl Authority {
    fn drain_subscriptions(&self) -> Result<(), AbiStatus> {
        let records = self.subscriptions.lock().map_err(|_| PANICKED)?.clone();
        for record in records {
            if record.unknown {
                return Err(PANICKED);
            }
            if !record.retiring {
                return Err(BUSY);
            }
            let status = unsafe { (self.table.unsubscribe_cancel)(self.table.handle, record.id) };
            if status != OK {
                return Err(status);
            }
            // OK is the only proof allowing userdata reclamation. This removes
            // exactly the fenced record; callbacks cannot access it afterward.
            self.subscriptions
                .lock()
                .map_err(|_| PANICKED)?
                .retain(|item| !Arc::ptr_eq(&item.state, &record.state));
        }
        Ok(())
    }
    fn drain_entry(&self) -> Result<(), AbiStatus> {
        let record = self.entry.lock().map_err(|_| PANICKED)?.clone();
        let Some(record) = record else {
            return Ok(());
        };
        if !record.retiring {
            return Err(BUSY);
        }
        if let Some(table) = record.transferred {
            let status = unsafe { (table.release)(table.handle) };
            if status != OK {
                return Err(status);
            }
            let mut slot = self.entry.lock().map_err(|_| PANICKED)?;
            slot.as_mut().ok_or(PANICKED)?.transferred = None;
        }
        if record.unknown {
            return Err(PANICKED);
        }
        if let Some(ports) = &record.ports {
            ports.close_and_drain()?;
        }
        let status = unsafe { (self.table.leave)(self.table.handle, record.lease) };
        if status != OK {
            return Err(status);
        }
        *self.entry.lock().map_err(|_| PANICKED)? = None;
        Ok(())
    }
    fn drain(&self) -> Result<(), AbiStatus> {
        self.drain_subscriptions()?;
        self.drain_entry()?;
        if self.uncertain.load(Ordering::Acquire) {
            return Err(PANICKED);
        }
        Ok(())
    }
}
impl Executor {
    /// Called by native operation reads, final drains and instance destruction.
    /// Each pass retries only recorded cleanup identities. BUSY consumes none.
    /// Reentrant drain rejects without holding locks over ABI callbacks.
    pub(in crate::abi) fn drain(&self) -> Result<(), AbiStatus> {
        if self
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(BUSY);
        }
        let result = self.drain_owners();
        self.draining.store(false, Ordering::Release);
        result
    }
    fn drain_owners(&self) -> Result<(), AbiStatus> {
        if !self.open_failures.lock().map_err(|_| PANICKED)?.is_empty() {
            return Err(PANICKED);
        }
        let owners = self.owners.lock().map_err(|_| PANICKED)?.clone();
        let mut failure = None;
        for owner in owners {
            // Registry+snapshot only means no observer/job/guard retains owner.
            let drained = owner.drain();
            if let Err(status) = drained {
                failure = Some(status);
                continue;
            }
            if Arc::strong_count(&owner) != 2 {
                continue;
            }
            let status = unsafe { (owner.table.release)(owner.table.handle) };
            if status != OK {
                failure = Some(status);
                continue;
            }
            self.owners
                .lock()
                .map_err(|_| PANICKED)?
                .retain(|item| !Arc::ptr_eq(item, &owner));
            self.owner_slots.fetch_sub(1, Ordering::AcqRel);
        }
        failure.map_or(Ok(()), Err)
    }
    pub(in crate::abi) fn retired(&self) -> Result<(), AbiStatus> {
        self.drain()?;
        if self.jobs.load(Ordering::Acquire) != 0 || self.owner_slots.load(Ordering::Acquire) != 0 {
            return Err(BUSY);
        }
        Ok(())
    }
}
