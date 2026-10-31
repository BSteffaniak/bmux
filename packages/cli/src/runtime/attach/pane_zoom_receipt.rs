//! Per-request reducer settlement, deliberately distinct from presentation commit.
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

/// Settlement of one exact pane zoom request, not a displayed-frame acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePaneZoomRequestStatus {
    Pending,
    Applied,
    Rejected,
}

/// Read-only observation handle; unrelated frames cannot settle this request.
#[derive(Clone, Debug)]
pub struct NativePaneZoomRequestReceipt(Arc<AtomicU8>);

impl NativePaneZoomRequestReceipt {
    #[must_use]
    pub fn status(&self) -> NativePaneZoomRequestStatus {
        match self.0.load(Ordering::Acquire) {
            1 => NativePaneZoomRequestStatus::Applied,
            2 => NativePaneZoomRequestStatus::Rejected,
            _ => NativePaneZoomRequestStatus::Pending,
        }
    }
}

/// Sole settlement owner travels with the queued event; dropped work is rejected.
#[derive(Clone, Debug)]
pub struct NativePaneZoomRequestSettlement(Arc<SettlementInner>);

#[derive(Debug)]
struct SettlementInner(Arc<AtomicU8>);

impl NativePaneZoomRequestSettlement {
    pub(super) fn new() -> (NativePaneZoomRequestReceipt, Self) {
        let state = Arc::new(AtomicU8::new(0));
        (
            NativePaneZoomRequestReceipt(Arc::clone(&state)),
            Self(Arc::new(SettlementInner(state))),
        )
    }

    pub(super) fn settle(self, applied: bool) {
        let _ = self.0.0.compare_exchange(
            0,
            if applied { 1 } else { 2 },
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

impl Drop for SettlementInner {
    fn drop(&mut self) {
        let _ = self
            .0
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_owner_can_settle_receipt() {
        let (first, owner) = NativePaneZoomRequestSettlement::new();
        let (second, unrelated) = NativePaneZoomRequestSettlement::new();
        unrelated.settle(true);
        assert_eq!(first.status(), NativePaneZoomRequestStatus::Pending);
        assert_eq!(second.status(), NativePaneZoomRequestStatus::Applied);
        drop(owner);
        assert_eq!(first.status(), NativePaneZoomRequestStatus::Rejected);
    }

    #[test]
    fn applied_settlement_survives_owner_drop() {
        let (receipt, owner) = NativePaneZoomRequestSettlement::new();
        owner.settle(true);
        assert_eq!(receipt.status(), NativePaneZoomRequestStatus::Applied);
    }
}
