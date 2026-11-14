use super::*;
use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};

#[tokio::test]
async fn presentation_publication_reaches_retained_owner_and_commit_gate() {
    use crate::runtime::attach::frontend_presentation::{
        FrontendPresentationOwner, FrontendPresentations,
    };
    use bmux_plugin::component_viewport::portable::{
        PortablePresentation, PortablePresentationLimits,
    };
    use bmux_tui::{damage::Damage, geometry::Rect, presentation::PresentationState};
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let (registrar, mut bindings) = FrontendBindings::channel();
    let producer = registrar
        .presentation(
            activation.clone(),
            instance.id(),
            3,
            Rect::new(0, 0, 2, 1),
            0,
        )
        .unwrap();
    let mut retained = FrontendPresentations::default();
    match bindings.next().await.unwrap() {
        FrontendEvent::Presentation(PresentationUpdate::Install {
            scope,
            instance,
            generation,
            area,
            order,
        }) => {
            retained
                .install(
                    FrontendPresentationOwner::bind(scope, instance, generation).unwrap(),
                    area,
                    order,
                )
                .unwrap();
        }
        _ => panic!("expected installation"),
    }
    let scene = PresentationState::new(Rect::new(0, 0, 2, 1));
    let frame = scene.prepare(Damage::Full, |_| {}).unwrap();
    producer
        .publish_encoded(
            1,
            &PortablePresentation::from_prepared(&frame)
                .encode(PortablePresentationLimits::default())
                .unwrap(),
        )
        .unwrap();
    match bindings.next().await.unwrap() {
        FrontendEvent::Presentation(PresentationUpdate::Publish {
            instance,
            revision,
            bytes,
            ..
        }) => retained
            .publish_encoded(instance, revision, &bytes)
            .unwrap(),
        _ => panic!("expected publication"),
    }
    assert!(!retained.input_is_current());
    retained.displayed().unwrap();
    assert!(retained.input_is_current());
    retained.failed();
    assert!(!retained.input_is_current());
    retained.remove(instance.id());
    activation.close().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
}

#[tokio::test]
async fn queued_presentation_keeps_admission_until_receiver_releases_it() {
    use bmux_plugin::component_viewport::portable::{
        PortablePresentation, PortablePresentationLimits,
    };
    use bmux_tui::{damage::Damage, geometry::Rect, presentation::PresentationState};
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let (registrar, mut bindings) = FrontendBindings::channel();
    let producer = registrar
        .presentation(
            activation.clone(),
            instance.id(),
            1,
            Rect::new(0, 0, 1, 1),
            0,
        )
        .unwrap();
    assert!(matches!(
        bindings.next().await.unwrap(),
        FrontendEvent::Presentation(PresentationUpdate::Install { .. })
    ));
    let scene = PresentationState::new(Rect::new(0, 0, 1, 1));
    let prepared = scene.prepare(Damage::Full, |_| {}).unwrap();
    producer
        .publish_encoded(
            1,
            &PortablePresentation::from_prepared(&prepared)
                .encode(PortablePresentationLimits::default())
                .unwrap(),
        )
        .unwrap();
    let queued = bindings.next().await.unwrap();
    activation.close().unwrap();
    assert!(producer.publish_encoded(2, &[]).is_err());
    drop(queued);
    activation.drain().await.unwrap();
    activation.finish().unwrap();
}

#[tokio::test]
async fn scoped_inboxes_deliver_and_reject_cross_instance_binding() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let other = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let (registrar, mut bindings) = FrontendBindings::channel();
    assert!(registrar.bind(activation.clone(), other.id()).is_err());
    let (_prompt, actions) = registrar.bind(activation.clone(), instance.id()).unwrap();
    actions.dispatch("focus_next_pane").unwrap();
    match bindings.next().await.unwrap() {
        FrontendEvent::Action(action) => assert_eq!(action.action, "focus_next_pane"),
        FrontendEvent::Presentation(_)
        | FrontendEvent::Prompt(_)
        | FrontendEvent::OwnerClosed(_) => panic!("expected owned action"),
    }
    activation.close().unwrap();
    assert!(actions.dispatch("focus_next_pane").is_err());
    assert!(
        matches!(bindings.next().await.unwrap(), FrontendEvent::OwnerClosed(id) if id == instance.id())
    );
    activation.drain().await.unwrap();
    activation.finish().unwrap();
}
