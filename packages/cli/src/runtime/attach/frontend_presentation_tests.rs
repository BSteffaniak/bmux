use super::*;
use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};
use bmux_tui::{damage::Damage, presentation::PresentationState};

#[test]
fn frontend_input_requires_display_and_failure_revokes_old_lease() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let mut owner = FrontendPresentationOwner::bind(activation, instance.id(), 7).unwrap();
    let scene = PresentationState::new(Rect::new(0, 0, 2, 1));
    let prepared = scene.prepare(Damage::Full, |_| {}).unwrap();
    let portable = PortablePresentation::from_prepared(&prepared);
    let bytes = portable
        .encode(PortablePresentationLimits::default())
        .unwrap();
    owner.publish_encoded(1, &bytes).unwrap();
    assert!(owner.committed_for_input().is_err());
    owner.displayed().unwrap();
    let committed = owner.committed_for_input().unwrap();
    owner.failed();
    assert!(owner.committed_for_input().is_err());
    assert_eq!(
        committed.modal_scopes(),
        Vec::<bmux_tui::focus::FocusScopeId>::new()
    );
}

#[test]
fn another_frontend_instance_cannot_bind_owner_scope() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let other = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    assert!(FrontendPresentationOwner::bind(activation, other.id(), 0).is_err());
}
