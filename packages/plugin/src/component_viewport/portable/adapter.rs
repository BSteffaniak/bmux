//! Selected frontend adapter: producer mutations and host commit authority are
//! separate handles. Both are scoped to one frontend/owner activation.
use std::sync::{Arc, Mutex};

use super::{
    FrontendPresentationRequest, FrontendPresentationResources, FrontendPresentationResponse,
    FrontendResourceId, PORTABLE_PRESENTATION_CONTRACT_ID, PortablePresentationError,
    PortablePresentationLimits, PreparedFrontendPresentation,
};

/// A factory-owned producer capability. It cannot acknowledge output or enable
/// input, and cannot select another activation's store.
#[derive(Clone)]
pub struct RetainedFrontendPort {
    state: Arc<Mutex<Option<FrontendPresentationResources>>>,
}

impl RetainedFrontendPort {
    /// Apply a typed static request without serialization.
    ///
    /// # Errors
    /// Rejects retired scopes, invalid data, exhausted budgets and stale revisions.
    pub fn apply(
        &self,
        request: FrontendPresentationRequest,
    ) -> Result<FrontendPresentationResponse, PortablePresentationError> {
        self.state
            .lock()
            .map_err(|_| PortablePresentationError::Unavailable)?
            .as_mut()
            .ok_or(PortablePresentationError::Unavailable)?
            .apply(request)
    }

    /// Decode a selected dynamic extension request. The ABI adapter retains the
    /// returned result across response-buffer retries; it must not invoke twice.
    ///
    /// # Errors
    /// Rejects unsupported contracts, retired scopes, malformed or stale requests.
    pub fn extension(
        &self,
        contract: &str,
        bytes: &[u8],
    ) -> Result<FrontendPresentationResponse, PortablePresentationError> {
        if contract != PORTABLE_PRESENTATION_CONTRACT_ID {
            return Err(PortablePresentationError::Version);
        }
        let mut guard = self
            .state
            .lock()
            .map_err(|_| PortablePresentationError::Unavailable)?;
        let state = guard
            .as_mut()
            .ok_or(PortablePresentationError::Unavailable)?;
        let request = state.decode_request(bytes)?;
        let result = state.apply(request);
        drop(guard);
        result
    }
}

/// Frontend-owned display capability. Drop retires the scope and revokes even
/// outstanding prepared replays; producer-port clones do not prolong authority.
pub struct RetainedFrontendHost {
    state: Arc<Mutex<Option<FrontendPresentationResources>>>,
}

impl RetainedFrontendHost {
    /// Select a rich adapter explicitly; ABI support alone does not imply it.
    ///
    /// `owner` binds a trusted frontend/activation namespace; `scene_epoch` is
    /// only a local namespace epoch, not runtime activation authority. Callers
    /// hold the actual activation/invocation admission through each effect and
    /// retain admitted cleanup until settlement. A constructor is not admission.
    ///
    /// # Errors
    /// Rejects invalid owner identity.
    pub fn new(
        owner: String,
        scene_epoch: u64,
        limits: PortablePresentationLimits,
    ) -> Result<Self, PortablePresentationError> {
        Ok(Self {
            state: Arc::new(Mutex::new(Some(FrontendPresentationResources::new(
                owner,
                scene_epoch,
                limits,
            )?))),
        })
    }

    #[must_use]
    pub fn producer(&self) -> RetainedFrontendPort {
        RetainedFrontendPort {
            state: self.state.clone(),
        }
    }

    /// Resolve a retained full scene outside the terminal frame loop.
    ///
    /// # Errors
    /// Rejects unknown resources, revisions or retired scopes.
    pub fn prepare(
        &self,
        resource: FrontendResourceId,
        revision: u64,
    ) -> Result<PreparedFrontendPresentation, PortablePresentationError> {
        self.state
            .lock()
            .map_err(|_| PortablePresentationError::Unavailable)?
            .as_ref()
            .ok_or(PortablePresentationError::Unavailable)?
            .prepare(resource, revision)
    }

    /// Acknowledge only after the authoritative compositor's successful output.
    ///
    /// # Errors
    /// Rejects unknown resources, stale generations/revisions or retired scopes.
    pub fn acknowledge(
        &self,
        resource: FrontendResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<(), PortablePresentationError> {
        self.state
            .lock()
            .map_err(|_| PortablePresentationError::Unavailable)?
            .as_mut()
            .ok_or(PortablePresentationError::Unavailable)?
            .acknowledge(resource, generation, revision)
    }

    /// Validate input against displayed output before routing through its scene.
    ///
    /// # Errors
    /// Rejects stale, uncommitted, destroyed or retired targets.
    pub fn validate_input(
        &self,
        resource: FrontendResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<(), PortablePresentationError> {
        self.state
            .lock()
            .map_err(|_| PortablePresentationError::Unavailable)?
            .as_ref()
            .ok_or(PortablePresentationError::Unavailable)?
            .validate_input(resource, generation, revision)
    }

    /// Resolve displayed output and validate its input version under one scope
    /// lock. Use this instead of pairing `validate_input` with a pending prepare.
    /// Snapshots revoke on a newer commit or display failure; use them only for
    /// immediate synchronous dispatch, never as an application input queue.
    ///
    /// # Errors
    /// Rejects unknown, stale, uncommitted, retired or suspended targets.
    pub fn committed_for_input(
        &self,
        resource: FrontendResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<PreparedFrontendPresentation, PortablePresentationError> {
        self.state
            .lock()
            .map_err(|_| PortablePresentationError::Unavailable)?
            .as_ref()
            .ok_or(PortablePresentationError::Unavailable)?
            .committed_for_input(resource, generation, revision)
    }

    /// Suspend all input after failed display or compositor reset.
    ///
    /// # Errors
    /// Rejects retired or poisoned scopes.
    pub fn display_failed(&self) -> Result<(), PortablePresentationError> {
        self.state
            .lock()
            .map_err(|_| PortablePresentationError::Unavailable)?
            .as_mut()
            .ok_or(PortablePresentationError::Unavailable)?
            .display_failed();
        Ok(())
    }
}

impl Drop for RetainedFrontendHost {
    fn drop(&mut self) {
        // Poison does not authorize retaining producer access after host teardown.
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.take();
    }
}
