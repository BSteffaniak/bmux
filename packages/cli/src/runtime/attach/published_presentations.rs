//! Presentations published by plugins that do not run inside the attach client.
//!
//! A plugin publishes its whole presentation (layout requests and surfaces) as
//! retained JSON state on `<plugin id>/presentation`, declared in its manifest
//! with `forward_to_streaming_clients = true`. The server forwards it to every
//! streaming client; this companion lowers each snapshot into the same layout
//! and surface registries in-process presentations publish into, under the
//! publishing plugin's own id. Input on a published region becomes an
//! invocation of the region's endpoint carrying the event, which the host runs
//! asynchronously like any other surface action.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use bmux_plugin::layout::{PluginLayoutPublishError, PluginLayoutSnapshot};
use bmux_plugin::surface::{PluginSurfaceSnapshot, PublishedPresentation, presentation_event_kind};
use bmux_plugin::{
    AttachInputEndpoint, AttachInputEvent, AttachInputResult, AttachInputServiceInvocation,
    AttachPresentationResources, PluginEventKind,
};

type Endpoints = Arc<Mutex<BTreeMap<String, BTreeSet<AttachInputEndpoint>>>>;

/// The registries a published presentation is lowered into.
#[derive(Clone)]
pub struct Registries {
    pub layouts: Arc<bmux_plugin::layout::PluginLayoutRegistry>,
    pub surfaces: Arc<bmux_plugin::surface::PluginSurfaceRegistry>,
    pub input: Arc<bmux_plugin::AttachPresentationInputRegistry>,
}

impl Registries {
    #[must_use]
    pub fn of(resources: &AttachPresentationResources) -> Self {
        Self {
            layouts: resources.layouts.clone(),
            surfaces: resources.surfaces.clone(),
            input: resources.input.clone(),
        }
    }
}

/// Lowers every installed plugin's published presentation for one attach view.
pub struct PublishedPresentations {
    owners: Vec<String>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    registries: Registries,
    endpoints: Endpoints,
}

impl PublishedPresentations {
    /// Follow the presentation of each of `plugin_ids`.
    #[must_use]
    pub fn install(plugin_ids: &[String], resources: &AttachPresentationResources) -> Self {
        let endpoints: Endpoints = Arc::default();
        let mut tasks = Vec::new();
        for owner in plugin_ids {
            let kind = PluginEventKind::from(presentation_event_kind(owner));
            if let Err(error) = resources.events.register_json_state_channel(kind.clone()) {
                tracing::warn!(%owner, %error, "cannot follow published presentation");
                continue;
            }
            let Ok((_, mut updates)) = resources.events.subscribe_state::<serde_json::Value>(&kind)
            else {
                continue;
            };
            let owner = owner.clone();
            let registries = Registries::of(resources);
            let endpoints = endpoints.clone();
            tasks.push(tokio::spawn(async move {
                while updates.changed().await.is_ok() {
                    let value = updates.borrow_and_update().clone();
                    if value.is_null() {
                        continue;
                    }
                    match serde_json::from_value::<PublishedPresentation>(value.as_ref().clone()) {
                        Ok(presentation) => {
                            if let Err(error) = apply(&owner, presentation, &registries, &endpoints)
                            {
                                tracing::warn!(%owner, %error, "rejected published presentation");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%owner, %error, "malformed published presentation");
                        }
                    }
                }
            }));
        }
        Self {
            owners: plugin_ids.to_vec(),
            tasks,
            registries: Registries::of(resources),
            endpoints,
        }
    }
}

impl Drop for PublishedPresentations {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for owner in &self.owners {
            self.registries.layouts.remove_owner(owner);
            self.registries.surfaces.remove_owner(owner);
        }
        if let Ok(endpoints) = self.endpoints.lock() {
            for endpoint in endpoints.values().flatten() {
                self.registries.input.remove(endpoint);
            }
        }
    }
}

/// Lower one published snapshot into the registries under `owner`.
///
/// # Errors
/// Returns the registries' validation failures (foreign ownership, limits).
pub fn apply(
    owner: &str,
    presentation: PublishedPresentation,
    resources: &Registries,
    endpoints: &Endpoints,
) -> Result<(), String> {
    let revision = presentation.revision.max(1);
    let layouts = PluginLayoutSnapshot {
        revision,
        requests: presentation.layouts,
    };
    match resources.layouts.publish(owner, layouts.clone()) {
        Ok(_) => {}
        // A restarted plugin counts its revisions from the start again.
        Err(PluginLayoutPublishError::ConflictingRevision) => {
            resources.layouts.remove_owner(owner);
            resources
                .layouts
                .publish(owner, layouts)
                .map_err(|error| format!("layouts: {error:?}"))?;
        }
        Err(error) => return Err(format!("layouts: {error:?}")),
    }
    let mut surfaces = presentation.surfaces;
    for surface in &mut surfaces {
        surface.revision = revision;
    }
    let wanted = surfaces
        .iter()
        .flat_map(|surface| &surface.interactive_regions)
        .filter_map(|region| region.endpoint.clone())
        .collect::<BTreeSet<_>>();
    resources
        .surfaces
        .publish_advancing(owner, PluginSurfaceSnapshot { revision, surfaces })
        .map_err(|error| format!("surfaces: {error:?}"))?;
    let mut endpoints = endpoints.lock().map_err(|_| "endpoint lock poisoned")?;
    let held = endpoints.entry(owner.to_string()).or_default();
    for endpoint in held.difference(&wanted) {
        resources.input.remove(endpoint);
    }
    for endpoint in wanted.difference(held) {
        let target = endpoint.clone();
        resources.input.register(
            endpoint.clone(),
            Arc::new(move |event| forward(&target, event)),
        );
    }
    *held = wanted;
    drop(endpoints);
    Ok(())
}

/// Deliver `event` to the plugin by invoking `endpoint`. Pointer moves are not
/// forwarded: a remote round trip per motion event would flood the plugin.
fn forward(endpoint: &AttachInputEndpoint, event: &AttachInputEvent) -> Option<AttachInputResult> {
    if event.event_kind == "pointer" && matches!(event.phase.as_str(), "move" | "drag") {
        return None;
    }
    let payload = bmux_plugin_sdk::encode_service_message(event).ok()?;
    Some(AttachInputResult {
        consumed: true,
        service_invocation: Some(AttachInputServiceInvocation {
            endpoint: endpoint.clone(),
            response_endpoint: None,
            payload,
        }),
        ..AttachInputResult::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::RenderOp;
    use bmux_plugin::layout::{LayoutEdge, LayoutExtent, PluginLayoutId, PluginLayoutRequest};
    use bmux_plugin::surface::{PluginSurface, PluginSurfaceId, PluginSurfaceRegion};
    use bmux_plugin::{ExtensionRect, RenderStyle};
    use uuid::Uuid;

    const OWNER: &str = "example.remote";

    fn endpoint() -> AttachInputEndpoint {
        AttachInputEndpoint {
            capability: "example.remote.input".into(),
            interface_id: "presentation-input".into(),
            operation: "handle-input".into(),
        }
    }

    fn presentation(revision: u64, label: &str) -> PublishedPresentation {
        let layout = PluginLayoutId::new(OWNER, "panel");
        PublishedPresentation {
            revision,
            layouts: vec![PluginLayoutRequest::split(
                layout.clone(),
                0,
                LayoutEdge::Right,
                LayoutExtent::Cells(10),
            )],
            surfaces: vec![
                PluginSurface::layout(
                    PluginSurfaceId::new(OWNER, "panel", Uuid::from_u128(1)),
                    0,
                    layout,
                    vec![RenderOp::text_run(0, 0, label, RenderStyle::new())],
                )
                .interactive_region(
                    PluginSurfaceRegion::new("button", ExtensionRect::new(0, 0, 10, 1))
                        .endpoint(endpoint()),
                ),
            ],
        }
    }

    fn event(phase: &str) -> AttachInputEvent {
        AttachInputEvent {
            target: None,
            hook_id: String::new(),
            event_kind: "pointer".into(),
            phase: phase.into(),
            button: Some("left".into()),
            key: None,
            col: Some(1),
            row: Some(0),
            wheel_delta: 0,
            modifiers: bmux_plugin::AttachInputModifiers::default(),
            focused_pane: None,
            hovered_pane: None,
        }
    }

    #[tokio::test]
    async fn published_snapshots_reach_the_registries_and_input_becomes_invocations() {
        let resources = AttachPresentationResources::isolated(64);
        let installed = PublishedPresentations::install(&[OWNER.to_string()], &resources);
        let kind = PluginEventKind::from(presentation_event_kind(OWNER));
        resources
            .events
            .emit_json(
                &kind,
                &serde_json::to_value(presentation(1, "first")).unwrap(),
            )
            .unwrap();
        // The companion applies forwarded state asynchronously.
        for _ in 0..100 {
            if resources.surfaces.owner_snapshot(OWNER).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let surfaces = resources
            .surfaces
            .owner_snapshot(OWNER)
            .expect("surfaces lowered");
        assert_eq!(surfaces.surfaces.len(), 1);
        assert_eq!(resources.layouts.requests().len(), 1);

        let down = resources.input.invoke(&endpoint(), &event("down")).unwrap();
        assert!(down.consumed);
        let invocation = down.service_invocation.unwrap();
        assert_eq!(invocation.endpoint, endpoint());
        let decoded: AttachInputEvent =
            bmux_plugin_sdk::decode_service_message(&invocation.payload).unwrap();
        assert_eq!(decoded, event("down"));
        assert!(
            resources
                .input
                .invoke(&endpoint(), &event("move"))
                .is_none()
        );

        // A restarted plugin starting its revisions over still replaces the view.
        let endpoints = Endpoints::default();
        apply(
            OWNER,
            presentation(1, "again"),
            &Registries::of(&resources),
            &endpoints,
        )
        .unwrap();
        drop(installed);
        assert!(resources.surfaces.owner_snapshot(OWNER).is_none());
        assert_eq!(resources.layouts.requests().len(), 0);
    }

    #[test]
    fn foreign_surfaces_are_rejected() {
        let resources = AttachPresentationResources::isolated(64);
        let mut foreign = presentation(1, "x");
        foreign.surfaces[0].id = PluginSurfaceId::new("someone.else", "panel", Uuid::from_u128(1));
        assert!(
            apply(
                OWNER,
                foreign,
                &Registries::of(&resources),
                &Endpoints::default()
            )
            .is_err()
        );
    }
}
