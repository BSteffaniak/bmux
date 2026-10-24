//! Caller-driven local presentation. No application state or transport is owned here.
use bmux_tui::{
    damage::Damage,
    event::Event,
    geometry::Rect,
    interaction::{InteractionRoute, InteractionRouter},
    paint::PaintCx,
    presentation::{
        PreparedPresentation, PresentationError, PresentationLimits, PresentationState,
    },
};

/// Reconstructible presentation and interaction caches for one local viewport.
/// Paint callbacks consume caller-owned state and complete retained plugin snapshots;
/// they must never invoke plugins or perform product updates.
///
/// Dropping this value releases only its derived Rust caches. The host owns DOM
/// listeners, queued input, CanvasRenderer and application state: detach listeners
/// and settle an in-flight presentation before dropping the surface. Pointer
/// cancellation never dispatches a synthetic release or activates a control.
pub struct LocalSurface {
    presentation: PresentationState,
    router: InteractionRouter,
    limits: PresentationLimits,
    scope: Option<bmux_tui::focus::FocusScopeId>,
}

impl LocalSurface {
    /// Create presentation caches with caller-selected viewport and admission budgets.
    pub fn new(area: Rect, limits: PresentationLimits) -> Self {
        Self {
            presentation: PresentationState::new(area),
            router: InteractionRouter::new(),
            limits,
            scope: None,
        }
    }

    /// Inspect the last successfully committed presentation metadata.
    pub const fn presentation(&self) -> &PresentationState {
        &self.presentation
    }

    /// Resize invalidates paint history, not the last displayed interaction scene.
    pub fn resize(&mut self, area: Rect) {
        self.presentation.resize(area);
    }

    /// Route exactly once through the last successfully displayed scene.
    pub fn route(&mut self, event: Event) -> InteractionRoute {
        if matches!(event, Event::Focus(bmux_tui::event::FocusEvent::Lost)) {
            self.cancel_pointer();
        }
        self.router.route(event)
    }

    /// Route a bounded normalized browser batch, including cancellation cleanup.
    pub fn route_input(&mut self, input: crate::input::NormalizedInput) -> Vec<InteractionRoute> {
        if input.cancel_pointer {
            self.cancel_pointer();
        }
        input
            .events
            .into_iter()
            .map(|event| self.route(event))
            .collect()
    }

    /// Drop transient pointer state on browser cancellation or focus loss.
    pub fn cancel_pointer(&mut self) {
        let focused = self.router.focused().cloned();
        let scope = self.scope.clone();
        // Emptying the scene clears pressed/hovered identities while retaining
        // the router's nested modal restoration stack.
        self.router
            .commit_scene(bmux_tui::hit::HitMap::new(), scope.clone());
        self.router
            .commit_scene(self.presentation.hits().clone(), scope);
        if let Some(id) = focused {
            self.router.set_focused(&id);
        }
    }

    /// Prepare, admit, output and commit atomically. A failed output keeps metadata.
    /// The output callback must itself publish pixels transactionally.
    pub fn present<E>(
        &mut self,
        damage: Damage,
        paint: impl FnOnce(&mut PaintCx<'_, '_>),
        output: impl FnOnce(&PreparedPresentation) -> Result<(), E>,
    ) -> Result<bool, PresentationError<E>> {
        self.present_with_metadata(damage, paint, |_| {}, output)
    }

    /// Stage caller-owned modal chains or logical selection text before admission.
    /// The staging callback cannot change pixel/layout geometry. Limits are checked
    /// again after staging, before any output or committed routing changes.
    pub fn present_with_metadata<E>(
        &mut self,
        damage: Damage,
        paint: impl FnOnce(&mut PaintCx<'_, '_>),
        stage: impl FnOnce(&mut PreparedPresentation),
        output: impl FnOnce(&PreparedPresentation) -> Result<(), E>,
    ) -> Result<bool, PresentationError<E>> {
        let mut prepared = self
            .presentation
            .prepare_bounded(self.limits, damage, paint)
            .map_err(PresentationError::Limit)?;
        let mut next_router = self.router.clone();
        if let Some(frame) = &mut prepared {
            stage(frame);
            frame
                .validate_limits(self.limits)
                .map_err(PresentationError::Limit)?;
            next_router.commit_scene(frame.hits().clone(), frame.focus_scope().cloned());
            frame.project_active_focus(next_router.focused());
        }
        let next_scope = prepared
            .as_ref()
            .and_then(|frame| frame.focus_scope().cloned());
        let changed = self
            .presentation
            .present_prepared(prepared, |frame, _| output(frame))
            .map_err(PresentationError::Output)?
            .is_some();
        if changed {
            self.router = next_router;
            self.scope = next_scope;
        }
        Ok(changed)
    }

    /// Browser-local Canvas presentation, with no JSON/binary transport roundtrip.
    #[cfg(all(feature = "wasm", target_arch = "wasm32"))]
    pub async fn present_canvas(
        &mut self,
        canvas: &mut crate::CanvasRenderer,
        damage: Damage,
        paint: impl FnOnce(&mut PaintCx<'_, '_>),
    ) -> Result<bool, wasm_bindgen::JsValue> {
        self.present_canvas_with_metadata(canvas, damage, paint, |_| {})
            .await
    }

    /// Canvas transaction with caller-owned modal/selection metadata staging.
    #[cfg(all(feature = "wasm", target_arch = "wasm32"))]
    pub async fn present_canvas_with_metadata(
        &mut self,
        canvas: &mut crate::CanvasRenderer,
        damage: Damage,
        paint: impl FnOnce(&mut PaintCx<'_, '_>),
        stage: impl FnOnce(&mut PreparedPresentation),
    ) -> Result<bool, wasm_bindgen::JsValue> {
        let mut prepared = self
            .presentation
            .prepare_bounded(self.limits, damage, paint)
            .map_err(|error| wasm_bindgen::JsValue::from_str(&error.to_string()))?;
        let Some(frame) = &mut prepared else {
            return Ok(false);
        };
        stage(frame);
        frame
            .validate_limits(self.limits)
            .map_err(|error| wasm_bindgen::JsValue::from_str(&error.to_string()))?;
        let mut next_router = self.router.clone();
        next_router.commit_scene(frame.hits().clone(), frame.focus_scope().cloned());
        frame.project_active_focus(next_router.focused());
        let scope = frame.focus_scope().cloned();
        if let Err(error) = canvas.present_prepared(frame).await {
            self.presentation.reset();
            return Err(error);
        }
        let _: Result<_, std::convert::Infallible> =
            self.presentation.present_prepared(prepared, |_, _| Ok(()));
        self.router = next_router;
        self.scope = scope;
        Ok(true)
    }
}
