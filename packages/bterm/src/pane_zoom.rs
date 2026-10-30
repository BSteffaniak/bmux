//! Caller-owned local pane-view zoom. No layout, PTY, arrangement, or input authority.
//!
//! The caller supplies identities; this module never derives them from focus,
//! labels, indexes, geometry, connection IDs, or presentation epochs.
use std::{collections::BTreeMap, time::Instant};

use anyhow::{Result, ensure};
use sha2::{Digest as _, Sha256};

use crate::zoom::{ZoomAction, ZoomConfiguration, ZoomController};

const MAX_VIEWS: usize = 256;
const MAX_ID_BYTES: usize = 512;

/// Opaque caller-owned identity of one local view of one pane/source.
/// Include authority in each binding where needed. Two local views of the same
/// source MUST have distinct local-view bindings. A binding is not proof of liveness.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PaneViewBinding {
    local_view: String,
    pane: String,
}

impl PaneViewBinding {
    /// Supplied identities must be nonempty, bounded, and free of control characters.
    /// This validates syntax only, not authority or durability.
    pub fn new(local_view: &str, pane: &str) -> Result<Self> {
        for identity in [local_view, pane] {
            ensure!(
                !identity.is_empty()
                    && identity.len() <= MAX_ID_BYTES
                    && !identity.chars().any(char::is_control),
                "invalid pane-view binding"
            );
        }
        Ok(Self {
            local_view: local_view.into(),
            pane: pane.into(),
        })
    }

    /// Compare a requested durable binding with the actual verified binding.
    /// The caller MUST obtain `observed` from its authoritative local-view/pane
    /// binding, not echo persisted data. Neither argument may be inferred from
    /// presentation placement. Without that evidence, do not issue a restore token.
    pub fn verify_restore(&self, observed: &Self) -> Result<PaneViewRestoreToken> {
        ensure!(self == observed, "pane-view restore binding mismatch");
        Ok(PaneViewRestoreToken(self.clone()))
    }

    pub(crate) fn preference_key(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"bterm-pane-view-zoom-v1\0");
        // NUL-delimited encoding is unambiguous: bindings reject control characters.
        for value in [&self.local_view, &self.pane] {
            digest.update(value.as_bytes());
            digest.update(b"\0");
        }
        digest
            .finalize()
            .iter()
            .flat_map(|byte| {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                [
                    char::from(HEX[usize::from(byte >> 4)]),
                    char::from(HEX[usize::from(byte & 0x0f)]),
                ]
            })
            .collect()
    }
}

/// Explicit caller assertion of an exact verified durable local-view/pane pair.
/// This permits preference restoration, not input dispatch or execution resize.
/// The caller must discard the token when the verified binding is invalidated.
#[derive(Clone, Debug)]
pub struct PaneViewRestoreToken(PaneViewBinding);

impl PaneViewRestoreToken {
    /// Expose a store lookup only after actual CLI committed target observation.
    /// The namespace is presentation intent, not a durable resource identity.
    pub(crate) fn for_committed_pane(namespace: &str, pane: &str) -> Result<Self> {
        Ok(Self(PaneViewBinding::new(namespace, pane)?))
    }

    #[must_use]
    pub fn binding(&self) -> &PaneViewBinding {
        &self.0
    }
}

/// Target and animation state only. Samples become geometry only through the
/// caller's retained preparation and complete-output commit path.
pub struct PaneZoomState {
    configuration: ZoomConfiguration,
    views: BTreeMap<PaneViewBinding, ZoomController>,
}

impl PaneZoomState {
    pub fn new(configuration: ZoomConfiguration) -> Result<Self> {
        configuration.validate()?;
        Ok(Self {
            configuration,
            views: BTreeMap::new(),
        })
    }

    /// Bind an ephemeral view at scale one, clamped to current bounds.
    /// Existing local intent is preserved; at most 256 live bindings are retained.
    pub fn bind(&mut self, binding: PaneViewBinding) -> Result<bool> {
        self.insert(binding, None)
    }

    /// Seed a newly observed view from its committed local scale, not preferences.
    /// This is independent of `remember`; existing local intent is preserved.
    pub fn bind_current(&mut self, binding: PaneViewBinding, scale: f64) -> Result<bool> {
        ensure!(
            scale.is_finite() && scale > 0.0,
            "invalid committed pane zoom scale"
        );
        if self.views.contains_key(&binding) {
            return Ok(false);
        }
        ensure!(
            self.views.len() < MAX_VIEWS,
            "too many local pane zoom views"
        );
        let mut policy = self.configuration.clone();
        policy.remember = true;
        self.views
            .insert(binding, ZoomController::new(policy, Some(scale))?);
        Ok(true)
    }

    /// Restore only before the view is bound; late IO must not overwrite local
    /// intent. The current remember policy and bounds apply to the saved target.
    pub fn restore(&mut self, token: &PaneViewRestoreToken, target_scale: f64) -> Result<bool> {
        ensure!(
            target_scale.is_finite() && target_scale > 0.0,
            "invalid pane zoom target"
        );
        self.insert(token.binding().clone(), Some(target_scale))
    }

    fn insert(&mut self, binding: PaneViewBinding, remembered: Option<f64>) -> Result<bool> {
        if self.views.contains_key(&binding) {
            return Ok(false);
        }
        ensure!(
            self.views.len() < MAX_VIEWS,
            "too many local pane zoom views"
        );
        let controller = ZoomController::new(self.configuration.clone(), remembered)?;
        self.views.insert(binding, controller);
        Ok(true)
    }

    /// Caller must resolve and validate this target against committed retained
    /// state. Keyboard targets focus; modifier-wheel targets hover without focus.
    /// For SetSize supply the logical content base (including window zoom), not DPI.
    pub fn apply(
        &mut self,
        binding: &PaneViewBinding,
        action: ZoomAction,
        base_size: f64,
        now: Instant,
    ) -> Result<bool> {
        self.views
            .get_mut(binding)
            .ok_or_else(|| anyhow::anyhow!("unbound pane zoom view"))?
            .apply(action, base_size, now)
    }

    #[must_use]
    pub fn sample(&self, binding: &PaneViewBinding, now: Instant) -> Option<f64> {
        self.views
            .get(binding)
            .map(|controller| controller.sample(now))
    }

    #[must_use]
    pub fn target_scale(&self, binding: &PaneViewBinding) -> Option<f64> {
        self.views.get(binding).map(ZoomController::target_scale)
    }

    #[must_use]
    pub fn needs_frame(&self, now: Instant) -> bool {
        self.views
            .values()
            .any(|controller| controller.needs_frame(now))
    }

    /// Remove attachment-local state only; never delete resources or preferences.
    pub fn remove(&mut self, binding: &PaneViewBinding) -> bool {
        self.views.remove(binding).is_some()
    }

    /// Invalid policy leaves every view unchanged. Remembered state is not reloaded.
    pub fn reconfigure(&mut self, configuration: ZoomConfiguration, now: Instant) -> Result<()> {
        configuration.validate()?;
        for controller in self.views.values_mut() {
            controller.reconfigure(configuration.clone(), now)?;
        }
        self.configuration = configuration;
        Ok(())
    }
}

pub(crate) mod session;

#[cfg(test)]
mod tests;
