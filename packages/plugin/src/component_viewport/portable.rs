//! Host-owned retained frontend resources and local replay adapter.
//!
//! The single inert wire/capture contract is neutrally owned by `bmux_tui`.
//! Re-exports preserve existing host callers; author SDKs import TUI directly.

use bmux_tui::{
    focus::FocusScopeId,
    hit::{HitId, HitRegion},
    image::{ImageContribution, ImageKey, ImageLifecycle},
    paint::{LocalRect, PaintCx},
    selection::{SelectionContentId, SelectionFragmentId, SelectionScopeId},
};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub use bmux_tui::presentation::portable::{
    FrontendPresentationRequest, FrontendPresentationResponse, FrontendResourceId,
    PORTABLE_PRESENTATION_CONTRACT_ID, PORTABLE_PRESENTATION_VERSION, PortableMetadataAvailability,
    PortablePresentation, PortablePresentationError, PortablePresentationLimits,
};

mod adapter;
pub use adapter::{RetainedFrontendHost, RetainedFrontendPort};

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}

struct Resource {
    alive: Arc<AtomicBool>,
    generation: u64,
    input_enabled: bool,
    input_lease: Arc<AtomicBool>,
    latest_revision: Option<u64>,
    pending: BTreeMap<u64, Arc<PortablePresentation>>,
    committed: Option<(u64, Arc<PortablePresentation>)>,
}

/// Instance-owned retention and stale-input gate, never global or compositor authority.
///
/// One store belongs to one trusted owner activation and one frontend instance.
/// The frontend must destroy it on detach/activation teardown.
pub struct FrontendPresentationResources {
    owner: String,
    scene_epoch: u64,
    limits: PortablePresentationLimits,
    next_id: u64,
    resources: BTreeMap<FrontendResourceId, Resource>,
}

impl FrontendPresentationResources {
    /// Typed local dispatch and the dynamic adapter share the same operations.
    /// Hosts must retain a mutating request's result for response-buffer retries;
    /// they must not execute create/publish/destroy again merely to grow output.
    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn apply(
        &mut self,
        request: FrontendPresentationRequest,
    ) -> Result<FrontendPresentationResponse, PortablePresentationError> {
        match request {
            FrontendPresentationRequest::Create { generation } => {
                Ok(FrontendPresentationResponse::Created {
                    resource: self.create(generation)?,
                })
            }
            FrontendPresentationRequest::Publish {
                resource,
                generation,
                revision,
                presentation,
            } => {
                self.publish(resource, generation, revision, *presentation)?;
                Ok(FrontendPresentationResponse::Published)
            }
            FrontendPresentationRequest::Destroy { resource } => {
                self.destroy(resource)?;
                Ok(FrontendPresentationResponse::Destroyed)
            }
        }
    }

    /// Bounded dynamic request decoding before any resource mutation. The host
    /// provides caller identity and serializes the typed result through its
    /// ordinary ABI response-retention mechanism.
    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn decode_request(
        &self,
        bytes: &[u8],
    ) -> Result<FrontendPresentationRequest, PortablePresentationError> {
        if bytes.len() > self.limits.max_bytes {
            return Err(PortablePresentationError::Limit);
        }
        serde_json::from_slice(bytes).map_err(|_| PortablePresentationError::Encoding)
    }

    /// Construct a local scene store. `owner` is a trusted host-selected unique
    /// frontend/activation namespace; `scene_epoch` separates recreations within
    /// it. The epoch is not a runtime activation ID, credential or admission.
    /// Runtime scopes must fence and retain each effect at the calling boundary.
    ///
    /// # Errors
    /// Rejects invalid owner namespace syntax.
    pub fn new(
        owner: String,
        scene_epoch: u64,
        limits: PortablePresentationLimits,
    ) -> Result<Self, PortablePresentationError> {
        if !valid_id(&owner) {
            return Err(PortablePresentationError::Invalid);
        }
        Ok(Self {
            owner,
            scene_epoch,
            limits,
            next_id: 1,
            resources: BTreeMap::new(),
        })
    }

    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn create(
        &mut self,
        generation: u64,
    ) -> Result<FrontendResourceId, PortablePresentationError> {
        if self.resources.len() >= self.limits.max_resources {
            return Err(PortablePresentationError::Limit);
        }
        let id = FrontendResourceId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(PortablePresentationError::Limit)?;
        self.resources.insert(
            id,
            Resource {
                alive: Arc::new(AtomicBool::new(true)),
                generation,
                input_enabled: false,
                input_lease: Arc::new(AtomicBool::new(false)),
                latest_revision: None,
                pending: BTreeMap::new(),
                committed: None,
            },
        );
        Ok(id)
    }

    /// Admit a complete immutable revision without changing displayed interaction.
    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn publish(
        &mut self,
        id: FrontendResourceId,
        generation: u64,
        revision: u64,
        value: PortablePresentation,
    ) -> Result<(), PortablePresentationError> {
        let resource = self
            .resources
            .get_mut(&id)
            .ok_or(PortablePresentationError::UnknownResource)?;
        if resource.generation != generation
            || resource.latest_revision.is_some_and(|old| revision <= old)
        {
            return Err(PortablePresentationError::Stale);
        }
        if resource.pending.len() >= self.limits.max_pending {
            return Err(PortablePresentationError::Limit);
        }
        value.validate(self.limits)?;
        resource.latest_revision = Some(revision);
        resource.pending.insert(revision, Arc::new(value));
        Ok(())
    }

    /// Prepare replay outside the terminal frame loop. Holding this value cannot
    /// enable input; the output backend must acknowledge successful display.
    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn prepare(
        &self,
        id: FrontendResourceId,
        revision: u64,
    ) -> Result<PreparedFrontendPresentation, PortablePresentationError> {
        let resource = self
            .resources
            .get(&id)
            .ok_or(PortablePresentationError::UnknownResource)?;
        let value = resource
            .pending
            .get(&revision)
            .or_else(|| {
                resource
                    .committed
                    .as_ref()
                    .filter(|(rev, _)| *rev == revision)
                    .map(|(_, value)| value)
            })
            .ok_or(PortablePresentationError::Stale)?;
        Ok(PreparedFrontendPresentation {
            alive: resource.alive.clone(),
            input_lease: None,
            namespace: format!(
                "{}:{}:{}:{}:{}",
                self.owner.len(),
                self.owner,
                self.scene_epoch,
                id.0,
                resource.generation
            ),
            value: value.clone(),
        })
    }

    /// Call only after the ordinary retained compositor successfully flushes the
    /// exact revision. Failed output does not call this method.
    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn acknowledge(
        &mut self,
        id: FrontendResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<(), PortablePresentationError> {
        let resource = self
            .resources
            .get_mut(&id)
            .ok_or(PortablePresentationError::UnknownResource)?;
        if resource.generation != generation
            || resource
                .committed
                .as_ref()
                .is_some_and(|(old, _)| revision < *old)
        {
            return Err(PortablePresentationError::Stale);
        }
        if resource
            .committed
            .as_ref()
            .is_some_and(|(old, _)| revision == *old)
        {
            if !resource.input_enabled {
                resource.input_lease = Arc::new(AtomicBool::new(true));
            }
            resource.input_enabled = true;
            return Ok(());
        }
        let value = resource
            .pending
            .remove(&revision)
            .ok_or(PortablePresentationError::Stale)?;
        resource.pending.retain(|rev, _| *rev > revision);
        resource.input_lease.store(false, Ordering::Release);
        resource.input_lease = Arc::new(AtomicBool::new(true));
        resource.committed = Some((revision, value));
        resource.input_enabled = true;
        Ok(())
    }

    /// Validate delayed input/selection against the actual displayed revision.
    /// Routing and selection remain owned by the existing committed scene.
    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn validate_input(
        &self,
        id: FrontendResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<(), PortablePresentationError> {
        let resource = self
            .resources
            .get(&id)
            .ok_or(PortablePresentationError::UnknownResource)?;
        if resource.generation != generation {
            return Err(PortablePresentationError::Stale);
        }
        if !resource.input_enabled {
            return Err(PortablePresentationError::NotCommitted);
        }
        match &resource.committed {
            Some((current, _)) if *current == revision => Ok(()),
            Some(_) => Err(PortablePresentationError::Stale),
            None => Err(PortablePresentationError::NotCommitted),
        }
    }

    /// Resolve the exact displayed snapshot for input or logical-source lookup.
    /// Pending revisions are never eligible. Callers still use the compositor's
    /// committed geometry for routing; this does not perform a second hit test.
    ///
    /// # Errors
    /// Rejects unknown, stale, destroyed or display-suspended resources. The
    /// returned input snapshot is revoked by a newer commit or display failure;
    /// a later acknowledgment never revives that old input snapshot.
    pub fn committed_for_input(
        &self,
        id: FrontendResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<PreparedFrontendPresentation, PortablePresentationError> {
        self.validate_input(id, generation, revision)?;
        let mut prepared = self.prepare(id, revision)?;
        prepared.input_lease = self
            .resources
            .get(&id)
            .map(|resource| resource.input_lease.clone());
        Ok(prepared)
    }

    /// Suspend input after a compositor reset or failed display. Retained output
    /// can be prepared again, but requires a fresh successful acknowledgment.
    pub fn display_failed(&mut self) {
        for resource in self.resources.values_mut() {
            resource.input_enabled = false;
            resource.input_lease.store(false, Ordering::Release);
        }
    }

    /// Release retained images, text and prepared snapshots for this resource.
    /// Old handles are never reused in this activation.
    ///
    /// # Errors
    /// Returns an explicit admission, encoding, resource, or revision error.
    pub fn destroy(&mut self, id: FrontendResourceId) -> Result<(), PortablePresentationError> {
        let resource = self
            .resources
            .remove(&id)
            .ok_or(PortablePresentationError::UnknownResource)?;
        resource.alive.store(false, Ordering::Release);
        Ok(())
    }
}

impl Drop for FrontendPresentationResources {
    fn drop(&mut self) {
        for resource in self.resources.values() {
            resource.alive.store(false, Ordering::Release);
        }
    }
}

/// Immutable typed replay; it cannot install or mutate presentation authority.
pub struct PreparedFrontendPresentation {
    alive: Arc<AtomicBool>,
    input_lease: Option<Arc<AtomicBool>>,
    namespace: String,
    value: Arc<PortablePresentation>,
}

impl PreparedFrontendPresentation {
    fn available(&self) -> bool {
        self.alive.load(Ordering::Acquire)
            && self
                .input_lease
                .as_ref()
                .is_none_or(|lease| lease.load(Ordering::Acquire))
    }

    fn scoped(&self, id: &str) -> String {
        format!("{}:{}:{}", self.namespace, id.len(), id)
    }

    /// Replay through `PaintCx`: its transform, clipping and wide-cell rules are
    /// shared by cells, semantic geometry, selection, images, hits and cursor.
    /// The caller selects placement with `PaintCx::with_child`, not a new layout.
    pub fn paint(&self, cx: &mut PaintCx<'_, '_>) {
        if !self.available() {
            return;
        }
        let value = &self.value;
        let area = value.cells.area();
        cx.with_child(0, 0, LocalRect::terminal(area), |cx| {
            // Occlusions have already shaped the captured final cells and
            // metadata. Replay their image ordering first, then install final
            // metadata so occlusion cannot erase the producer's later hits.
            let mut occlusions = value.occlusions.iter().peekable();
            for (index, image) in value.images.iter().enumerate() {
                while occlusions.peek().is_some_and(|(at, _)| *at == index) {
                    if let Some((_, area)) = occlusions.next() {
                        cx.occlude(LocalRect::terminal(*area));
                    }
                }
                if let ImageContribution::Present(image) = image {
                    let mut image = image.clone();
                    image.key = ImageKey::new(self.scoped(image.key.as_str()));
                    image.lifecycle = ImageLifecycle::Frame;
                    cx.push_image(ImageContribution::Present(image));
                }
            }
            for (_, area) in occlusions {
                cx.occlude(LocalRect::terminal(*area));
            }
            cx.declare_metadata_availability(
                LocalRect::terminal(area),
                value.selection.is_some(),
                value.semantics.is_some(),
            );
            for (index, cell) in value.cells.cells().iter().enumerate() {
                if cell.is_wide_continuation() {
                    continue;
                }
                let x = u16::try_from(index % usize::from(area.width)).unwrap_or(0);
                let y = u16::try_from(index / usize::from(area.width)).unwrap_or(0);
                cx.set_cell(i32::from(x), i64::from(y), &cell.symbol, cell.style);
            }
            for entry in &value.metadata_availability {
                cx.declare_metadata_availability(
                    LocalRect::terminal(entry.area),
                    entry.selection,
                    entry.semantics,
                );
            }
            if let Some(scope) = &value.focus_scope {
                cx.set_focus_scope(Some(HitId::new(self.scoped(scope.as_str()))));
            }
            for hit in &value.hits {
                let mut hit = hit.clone();
                hit.id = HitId::new(self.scoped(hit.id.as_str()));
                hit.focus_scope = hit
                    .focus_scope
                    .map(|scope| HitId::new(self.scoped(scope.as_str())));
                cx.push_hit(hit);
            }
            if let Some(scene) = &value.semantics {
                for region in scene.regions() {
                    let mut region = region.clone();
                    region.id = self.scoped(&region.id);
                    cx.push_semantic(region);
                }
            }
            if let Some(scene) = &value.selection {
                for scope in scene.scopes() {
                    let mut scope = scope.clone();
                    scope.id = SelectionScopeId::new(self.scoped(scope.id.as_str()));
                    scope.parent = scope
                        .parent
                        .map(|id| SelectionScopeId::new(self.scoped(id.as_str())));
                    cx.push_selection_scope(scope);
                }
                for fragment in scene.fragments() {
                    let mut fragment = fragment.clone();
                    fragment.id = SelectionFragmentId::new(self.scoped(fragment.id.as_str()));
                    fragment.scope_id =
                        SelectionScopeId::new(self.scoped(fragment.scope_id.as_str()));
                    fragment.content_id =
                        SelectionContentId::new(self.scoped(fragment.content_id.as_str()));
                    cx.push_selection_fragment(fragment);
                }
            }
            if let Some(cursor) = value.cursor {
                cx.set_cursor_logical(
                    u64::from(cursor.position.x),
                    u64::from(cursor.position.y),
                    cursor.visible,
                );
            }
        });
    }

    /// Owner-scoped modal declarations for the host's existing prepared scene.
    /// Merge in compositor order and call `PreparedPresentation::set_modal_scopes`
    /// before output. Only that compositor commits and restores focus history.
    #[must_use]
    pub fn modal_scopes(&self) -> Vec<FocusScopeId> {
        if !self.available() {
            return Vec::new();
        }
        self.value
            .modal_scopes
            .iter()
            .map(|scope| HitId::new(self.scoped(scope.as_str())))
            .collect()
    }

    /// Snapshot caller-owned logical source text into an ordinary prepared frame.
    /// Frontends merge contributions from all producers before calling
    /// `PreparedPresentation::set_selection_text`; this never writes core state.
    #[must_use]
    pub fn selection_text(&self) -> BTreeMap<SelectionContentId, String> {
        if !self.available() {
            return BTreeMap::new();
        }
        self.value
            .text
            .iter()
            .map(|(id, text)| {
                (
                    SelectionContentId::new(self.scoped(id.as_str())),
                    text.clone(),
                )
            })
            .collect()
    }

    /// Resolve only an interaction actually declared by this resource. The
    /// caller first validates the displayed generation/revision with the store;
    /// this strips no arbitrary prefixes and never forwards another owner's ID.
    #[must_use]
    pub fn local_hit(&self, id: &HitId) -> Option<&HitRegion> {
        if !self.available() {
            return None;
        }
        self.value
            .hits
            .iter()
            .find(|hit| self.scoped(hit.id.as_str()) == id.as_str())
    }

    /// Explicit logical source lookup; effects such as clipboard/export remain
    /// application-owned. IDs are the same owner-scoped IDs emitted by `paint`.
    #[must_use]
    pub fn text(&self, id: &SelectionContentId) -> Option<&str> {
        if !self.available() {
            return None;
        }
        self.value.text.iter().find_map(|(local, text)| {
            (self.scoped(local.as_str()) == id.as_str()).then_some(text.as_str())
        })
    }
}

#[cfg(test)]
mod tests;
