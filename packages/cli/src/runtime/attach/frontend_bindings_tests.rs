use super::*;
use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};

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
        FrontendEvent::Prompt(_) | FrontendEvent::OwnerClosed(_) => panic!("expected owned action"),
    }
    activation.close().unwrap();
    assert!(actions.dispatch("focus_next_pane").is_err());
    assert!(
        matches!(bindings.next().await.unwrap(), FrontendEvent::OwnerClosed(id) if id == instance.id())
    );
    activation.drain().await.unwrap();
    activation.finish().unwrap();
}
