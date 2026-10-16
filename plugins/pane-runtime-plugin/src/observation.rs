//! Plugin-owned non-sizing observation for the legacy local attach backend.
use bmux_client::{
    AttachProvider, AttachProviderBackend, AttachProviderError, AttachProviderFuture,
    AttachProviderRegistration, AttachProviderSession, AttachTarget, ResolvedAttachTarget,
};
use std::sync::{Arc, OnceLock};

#[derive(Debug)]
struct ObservationProvider;
#[derive(Debug)]
struct Target(String);
impl ResolvedAttachTarget for Target {
    fn provider_id(&self) -> &'static str {
        "bmux.pane-runtime.observation"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl AttachProvider for ObservationProvider {
    fn id(&self) -> &'static str {
        "bmux.pane-runtime.observation"
    }
    fn priority(&self) -> i32 {
        1
    }
    fn supports(&self, target: &AttachTarget) -> bool {
        target.scheme().is_none() || target.scheme() == Some("local")
    }
    fn requires_fallback_client(&self) -> bool {
        true
    }
    fn resolve(
        &self,
        target: &AttachTarget,
    ) -> Result<Arc<dyn ResolvedAttachTarget>, AttachProviderError> {
        Ok(Arc::new(Target(
            if target.scheme().is_some() {
                target.reference()
            } else {
                target.raw()
            }
            .to_string(),
        )))
    }
    fn open(
        &self,
        target: Arc<dyn ResolvedAttachTarget>,
        _: Option<bmux_client::AttachResumeState>,
        context: bmux_client::AttachProviderOpenContext,
    ) -> AttachProviderFuture<'_, AttachProviderSession> {
        Box::pin(async move {
            let target = target
                .as_any()
                .downcast_ref::<Target>()
                .ok_or_else(|| self.failed("foreign resolved target"))?;
            Ok(AttachProviderSession {
                backend: AttachProviderBackend::Legacy(
                    context
                        .fallback_client
                        .ok_or_else(|| self.failed("missing fallback connection"))?,
                ),
                target: (!target.0.is_empty()).then(|| target.0.clone()),
            })
        })
    }
    fn allows_retarget(&self) -> bool {
        false
    }
    fn observe_viewport<'a>(
        &'a self,
        client: &'a mut bmux_client::StreamingBmuxClient,
        target: &'a str,
        fallback: bmux_client::AttachProviderViewport,
    ) -> AttachProviderFuture<'a, bmux_client::AttachProviderViewport> {
        Box::pin(async move {
            let id = target
                .parse()
                .map_err(|_| self.failed("observation requires a resolved UUID"))?;
            let value =
                bmux_pane_runtime_plugin_api::attach_runtime_commands::client::observe_viewport(
                    client,
                    id,
                    fallback.columns,
                    fallback.rows,
                )
                .await
                .map_err(|_| self.failed("viewport observation transport failed"))?
                .map_err(|error| {
                    self.failed(&format!("viewport observation rejected: {error:?}"))
                })?;
            Ok(bmux_client::AttachProviderViewport {
                command_sequence: 0,
                columns: value.cols,
                rows: value.rows,
                pixel_width: None,
                pixel_height: None,
            })
        })
    }
}
impl ObservationProvider {
    fn failed(&self, reason: &str) -> AttachProviderError {
        AttachProviderError::OpenFailed {
            provider_id: self.id().into(),
            reason: reason.into(),
        }
    }
}
/// Install the local provider adapter. Domain selection stays out of the host.
///
/// # Panics
/// Panics if another adapter registered the same provider identity.
pub fn install() {
    static REGISTRATION: OnceLock<AttachProviderRegistration> = OnceLock::new();
    REGISTRATION.get_or_init(|| {
        bmux_client::global_attach_provider_registry()
            .register(Arc::new(ObservationProvider))
            .expect("unique local observation adapter")
    });
}
