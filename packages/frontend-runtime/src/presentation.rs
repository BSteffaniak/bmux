//! Direct delivery of the retained compositor's owned prepared output.
//!
//! This module does not compose a second scene or encode a transport frame.
//! `stage` admits only the current attach identity and projection viewport;
//! staging a newer revision supersedes older work without changing interaction
//! authority. `present` invokes a synchronous backend acceptance callback only
//! for the latest staged delivery. Success means the *complete* output has been
//! accepted, not queued for later drawing. Failure preserves the last committed
//! delivery. The exclusive authority borrow serializes acceptance with rebinding
//! and supersession; callers must not acknowledge an asynchronous queue as output.
//!
//! The viewport is a presentation cell grid, not execution geometry or pixels.
//! Native adapters own logical/physical pixel conversion outside this contract.

use std::sync::Arc;

use bmux_tui::{geometry::Rect, presentation::PreparedPresentation};
use uuid::Uuid;

/// Provenance of one complete prepared output, scoped to an attach lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationIdentity {
    pub epoch: Uuid,
    pub revision: u64,
    pub attached_id: Uuid,
    pub viewport_generation: u64,
}

/// Owned output from the canonical retained preparation path, never ANSI/browser data.
#[derive(Debug, Clone)]
pub struct PreparedDelivery {
    pub identity: PresentationIdentity,
    pub prepared: Arc<PreparedPresentation>,
}

/// Failed provenance admission; no backend output or interaction commit occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryRejection {
    Identity,
    Viewport,
    Stale,
    Superseded,
}

/// Provenance rejection or unsuccessful complete backend output.
#[derive(Debug, PartialEq, Eq)]
pub enum DeliveryError<E> {
    Rejected(DeliveryRejection),
    Output(E),
}

impl std::fmt::Display for DeliveryRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "prepared delivery rejected: {self:?}")
    }
}
impl std::error::Error for DeliveryRejection {}
impl<E: std::fmt::Display> std::fmt::Display for DeliveryError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(error) => error.fmt(f),
            Self::Output(error) => error.fmt(f),
        }
    }
}
impl<E: std::error::Error + 'static> std::error::Error for DeliveryError<E> {}

/// Acceptance gate, not a resource manager, compositor, or independent hit map.
///
/// Consumers route input through `committed()` only. The retained prepared value
/// carries cells, hits, cursor, focus, images, selection, semantics and damage
/// together; none may be promoted independently of backend acceptance.
#[derive(Debug)]
pub struct PresentationAuthority {
    epoch: Uuid,
    attached_id: Uuid,
    viewport_generation: u64,
    area: Rect,
    latest: Option<PreparedDelivery>,
    highest_revision: u64,
    committed: Option<PreparedDelivery>,
}

impl PresentationAuthority {
    #[must_use]
    pub const fn new(epoch: Uuid, attached_id: Uuid, viewport_generation: u64, area: Rect) -> Self {
        Self {
            epoch,
            attached_id,
            viewport_generation,
            area,
            latest: None,
            highest_revision: 0,
            committed: None,
        }
    }

    /// Invalidate pending work when attach target or presentation viewport changes.
    /// The previous scene is discarded as input authority; displayed pixels are
    /// the backend's responsibility until replacement. Revisions remain monotonic
    /// across retarget/viewport changes within the same epoch.
    pub fn rebind(&mut self, epoch: Uuid, attached_id: Uuid, viewport_generation: u64, area: Rect) {
        if (
            self.epoch,
            self.attached_id,
            self.viewport_generation,
            self.area,
        ) != (epoch, attached_id, viewport_generation, area)
        {
            if self.epoch != epoch {
                self.highest_revision = 0;
            }
            self.epoch = epoch;
            self.attached_id = attached_id;
            self.viewport_generation = viewport_generation;
            self.area = area;
            self.latest = None;
            self.committed = None;
        }
    }

    fn validate(&self, delivery: &PreparedDelivery) -> Result<(), DeliveryRejection> {
        let id = delivery.identity;
        if id.epoch != self.epoch || id.attached_id != self.attached_id {
            return Err(DeliveryRejection::Identity);
        }
        if id.viewport_generation != self.viewport_generation
            || delivery.prepared.buffer().area() != self.area
        {
            return Err(DeliveryRejection::Viewport);
        }
        Ok(())
    }

    /// Advertise the newest complete candidate before scheduling backend work.
    ///
    /// # Errors
    /// Rejects mismatched identity/viewport and non-increasing revisions.
    pub fn stage(&mut self, delivery: &PreparedDelivery) -> Result<(), DeliveryRejection> {
        self.validate(delivery)?;
        if delivery.identity.revision <= self.highest_revision {
            return Err(DeliveryRejection::Stale);
        }
        self.highest_revision = delivery.identity.revision;
        self.latest = Some(delivery.clone());
        Ok(())
    }

    /// Deliver the latest staged value and atomically promote its interaction data.
    /// Retries of a failed output may reuse the staged candidate until superseded.
    ///
    /// # Errors
    /// Rejects stale/superseded provenance before output, or returns backend failure.
    pub fn present<T, E>(
        &mut self,
        delivery: PreparedDelivery,
        output: impl FnOnce(&PreparedDelivery) -> Result<T, E>,
    ) -> Result<T, DeliveryError<E>> {
        self.validate(&delivery).map_err(DeliveryError::Rejected)?;
        if !self.latest.as_ref().is_some_and(|latest| {
            latest.identity == delivery.identity
                && Arc::ptr_eq(&latest.prepared, &delivery.prepared)
        }) {
            return Err(DeliveryError::Rejected(DeliveryRejection::Superseded));
        }
        if self
            .committed
            .as_ref()
            .is_some_and(|committed| committed.identity == delivery.identity)
        {
            return Err(DeliveryError::Rejected(DeliveryRejection::Stale));
        }
        let result = output(&delivery).map_err(DeliveryError::Output)?;
        self.committed = Some(delivery);
        Ok(result)
    }

    /// Last accepted complete scene for the current binding; no pending metadata.
    #[must_use]
    pub fn committed(&self) -> Option<&PreparedDelivery> {
        self.committed
            .as_ref()
            .filter(|delivery| self.validate(delivery).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::{damage::Damage, presentation::PresentationState};

    fn candidate(epoch: Uuid, target: Uuid, revision: u64, generation: u64) -> PreparedDelivery {
        let prepared = PresentationState::new(Rect::new(0, 0, 2, 1))
            .prepare(Damage::Full, |_| {})
            .unwrap();
        PreparedDelivery {
            identity: PresentationIdentity {
                epoch,
                attached_id: target,
                revision,
                viewport_generation: generation,
            },
            prepared: Arc::new(prepared),
        }
    }

    #[test]
    fn failure_preserves_commit_and_retry_accepts_complete_owned_value() {
        let epoch = Uuid::nil();
        let target = Uuid::from_u128(1);
        let mut authority = PresentationAuthority::new(epoch, target, 1, Rect::new(0, 0, 2, 1));
        let first = candidate(epoch, target, 1, 1);
        authority.stage(&first).unwrap();
        authority.present(first, |_| Ok::<_, ()>(())).unwrap();
        let next = candidate(epoch, target, 2, 1);
        authority.stage(&next).unwrap();
        assert_eq!(
            authority.present(next.clone(), |_| Err::<(), _>("lost surface")),
            Err(DeliveryError::Output("lost surface"))
        );
        assert_eq!(authority.committed().unwrap().identity.revision, 1);
        let prepared = next.prepared.clone();
        authority.present(next, |_| Ok::<_, ()>(())).unwrap();
        assert!(Arc::ptr_eq(
            &authority.committed().unwrap().prepared,
            &prepared
        ));
    }

    #[test]
    fn supersession_never_calls_backend_or_advances_authority() {
        let epoch = Uuid::nil();
        let target = Uuid::from_u128(1);
        let mut authority = PresentationAuthority::new(epoch, target, 1, Rect::new(0, 0, 2, 1));
        let old = candidate(epoch, target, 1, 1);
        let new = candidate(epoch, target, 2, 1);
        authority.stage(&old).unwrap();
        authority.stage(&new).unwrap();
        assert_eq!(
            authority.present(old, |_| -> Result<(), ()> { panic!("stale output") }),
            Err(DeliveryError::Rejected(DeliveryRejection::Superseded))
        );
        assert!(authority.committed().is_none());
        authority.present(new.clone(), |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(authority.stage(&new), Err(DeliveryRejection::Stale));
        assert_eq!(
            authority.present(new, |_| -> Result<(), ()> { panic!("duplicate output") }),
            Err(DeliveryError::Rejected(DeliveryRejection::Stale))
        );
    }

    #[test]
    fn matching_identity_cannot_substitute_a_different_prepared_scene() {
        let epoch = Uuid::nil();
        let target = Uuid::from_u128(1);
        let mut authority = PresentationAuthority::new(epoch, target, 1, Rect::new(0, 0, 2, 1));
        let original = candidate(epoch, target, 1, 1);
        authority.stage(&original).unwrap();
        let substitute = candidate(epoch, target, 1, 1);
        assert_eq!(
            authority.present(substitute, |_| -> Result<(), ()> {
                panic!("substituted output")
            }),
            Err(DeliveryError::Rejected(DeliveryRejection::Superseded))
        );
        assert!(authority.committed().is_none());
        authority.present(original, |_| Ok::<_, ()>(())).unwrap();
    }

    #[test]
    fn viewport_and_retarget_invalidate_old_input_and_pending_work() {
        let epoch = Uuid::nil();
        let target = Uuid::from_u128(1);
        let area = Rect::new(0, 0, 2, 1);
        let mut authority = PresentationAuthority::new(epoch, target, 1, area);
        let first = candidate(epoch, target, 1, 1);
        authority.stage(&first).unwrap();
        authority
            .present(first.clone(), |_| Ok::<_, ()>(()))
            .unwrap();
        authority.rebind(epoch, target, 2, area);
        assert!(authority.committed().is_none());
        assert_eq!(authority.stage(&first), Err(DeliveryRejection::Viewport));
        let resized = candidate(epoch, target, 2, 2);
        authority.stage(&resized).unwrap();
        authority.rebind(epoch, Uuid::from_u128(2), 2, area);
        assert_eq!(authority.stage(&resized), Err(DeliveryRejection::Identity));
        authority.rebind(epoch, target, 2, Rect::new(0, 0, 3, 1));
        assert_eq!(authority.stage(&resized), Err(DeliveryRejection::Viewport));
    }
}
