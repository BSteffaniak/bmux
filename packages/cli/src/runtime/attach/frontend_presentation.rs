//! Attach-owned consumers of negotiated retained-v2 frontend output.
//! Producers publish off-frame; frame composition only replays retained state.
#[cfg(test)]
#[path = "frontend_presentation_tests.rs"]
mod tests;
use bmux_plugin::component_viewport::portable::{
    FrontendPresentationResources, FrontendResourceId, PortablePresentation,
    PortablePresentationLimits, PreparedFrontendPresentation,
};
use bmux_plugin_runtime::{ActivationScope, PluginInstanceId};
use bmux_tui::geometry::Rect;
use std::collections::BTreeMap;

/// The host passes the actual selected frontend instance and owning activation.
/// This does not create activation authority or detached backend lifetime.
pub struct FrontendPresentationOwner {
    scope: ActivationScope,
    resources: FrontendPresentationResources,
    resource: FrontendResourceId,
    generation: u64,
    pending: Option<u64>,
    committed_revision: Option<u64>,
}
impl FrontendPresentationOwner {
    /// Bind exactly one frontend surface to existing activation authority.
    ///
    /// # Errors
    /// Rejects another instance, closed admission or invalid resource limits.
    pub fn bind(
        scope: ActivationScope,
        frontend: PluginInstanceId,
        generation: u64,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            scope.instance_id() == frontend,
            "frontend scope belongs to another instance"
        );
        let admission = scope.try_admit()?;
        // Full UUID scope identity is retained in the namespace, never truncated
        // to the resource protocol's local integer discriminator.
        let mut resources = FrontendPresentationResources::new(
            format!("{frontend}:{}", scope.id()),
            0,
            PortablePresentationLimits::default(),
        )?;
        let resource = resources.create(generation)?;
        drop(admission);
        Ok(Self {
            scope,
            resources,
            resource,
            generation,
            pending: None,
            committed_revision: None,
        })
    }
    /// Accept complete rich output outside the frame loop. No metadata fallback.
    ///
    /// # Errors
    /// Rejects stale/oversized output, invalid authority and unsupported versions.
    pub fn publish_encoded(&mut self, revision: u64, bytes: &[u8]) -> anyhow::Result<()> {
        let admission = self.scope.try_admit()?;
        let value = PortablePresentation::decode(bytes, PortablePresentationLimits::default())?;
        self.resources
            .publish(self.resource, self.generation, revision, value)?;
        self.pending = Some(revision);
        drop(admission);
        Ok(())
    }
    fn prepared(&self) -> anyhow::Result<Option<PreparedFrontendPresentation>> {
        let _admission = self.scope.try_admit()?;
        self.pending
            .map(|revision| {
                self.resources
                    .prepare(self.resource, revision)
                    .map_err(anyhow::Error::from)
            })
            .transpose()
    }
    fn displayed(&mut self) -> anyhow::Result<()> {
        let _admission = self.scope.try_admit()?;
        if let Some(revision) = self.pending {
            self.resources
                .acknowledge(self.resource, self.generation, revision)?;
            self.committed_revision = Some(revision);
        }
        Ok(())
    }
    /// Input/selection consumers must obtain a current lease immediately before
    /// routing through the authoritative host scene's hit/focus decision.
    ///
    /// # Errors
    /// Rejects unpublished, stale, suspended, closed or destroyed resources.
    pub fn committed_for_input(&self) -> anyhow::Result<PreparedFrontendPresentation> {
        let _admission = self.scope.try_admit()?;
        let revision = self
            .committed_revision
            .ok_or_else(|| anyhow::anyhow!("frontend has no displayed revision"))?;
        Ok(self
            .resources
            .committed_for_input(self.resource, self.generation, revision)?)
    }
    fn failed(&mut self) {
        self.resources.display_failed();
    }
}
impl Drop for FrontendPresentationOwner {
    fn drop(&mut self) {
        let _ = self.resources.destroy(self.resource);
    }
}

/// Bounded attach-local presentation references, not another scene authority.
#[derive(Default)]
pub struct FrontendPresentations {
    owners: BTreeMap<PluginInstanceId, (i32, Rect, FrontendPresentationOwner)>,
}
impl FrontendPresentations {
    /// Install the selected owner's host-allocated geometry.
    ///
    /// # Errors
    /// Rejects duplicate owners and a full attachment resource budget.
    pub fn install(
        &mut self,
        owner: FrontendPresentationOwner,
        area: Rect,
        compositor_order: i32,
    ) -> anyhow::Result<()> {
        let id = owner.scope.instance_id();
        anyhow::ensure!(
            self.owners.len() < 64 && !self.owners.contains_key(&id),
            "frontend owner duplicate or capacity exhausted"
        );
        anyhow::ensure!(
            self.owners
                .values()
                .all(|(order, _, _)| *order != compositor_order),
            "frontend compositor order must be owner-unique"
        );
        self.owners.insert(id, (compositor_order, area, owner));
        Ok(())
    }
    /// Update owner-retained output off-frame.
    ///
    /// # Errors
    /// Rejects unknown owners or invalid output.
    pub fn publish_encoded(
        &mut self,
        id: PluginInstanceId,
        revision: u64,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        self.owners
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown frontend owner"))?
            .2
            .publish_encoded(revision, bytes)
    }
    /// Resolve input source metadata only through a current committed lease.
    ///
    /// # Errors
    /// Rejects unknown, closed or display-suspended owners.
    pub fn committed_for_input(
        &self,
        id: PluginInstanceId,
    ) -> anyhow::Result<PreparedFrontendPresentation> {
        self.owners
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown frontend owner"))?
            .2
            .committed_for_input()
    }
    /// Explicit detach withdraws references; it does not stop backend work.
    pub fn remove(&mut self, id: PluginInstanceId) {
        self.owners.remove(&id);
    }
    pub(super) fn reconcile_closed(&mut self) -> bool {
        let closed = self
            .owners
            .iter()
            .filter_map(|(id, (_, _, owner))| {
                owner.scope.is_cancelled().unwrap_or(true).then_some(*id)
            })
            .collect::<Vec<_>>();
        let changed = !closed.is_empty();
        for id in closed {
            self.remove(id);
        }
        changed
    }

    pub(super) fn input_is_current(&self) -> bool {
        self.owners
            .keys()
            .all(|id| self.committed_for_input(*id).is_ok())
    }

    pub(super) fn prepared(&self) -> anyhow::Result<Vec<(Rect, PreparedFrontendPresentation)>> {
        let mut owners = self.owners.values().collect::<Vec<_>>();
        owners.sort_by_key(|(order, _, _)| *order);
        owners
            .into_iter()
            .filter_map(|(_, area, owner)| match owner.prepared() {
                Ok(Some(value)) => Some(Ok((*area, value))),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }
    pub(super) fn displayed(&mut self) -> anyhow::Result<()> {
        for (_, _, owner) in self.owners.values_mut() {
            owner.displayed()?;
        }
        Ok(())
    }
    pub(super) fn failed(&mut self) {
        for (_, _, owner) in self.owners.values_mut() {
            owner.failed();
        }
    }
}
