use super::*;
use crate::{DecorationPlugin, State};
use bmux_attach_layout_protocol::attach_layout_protocol::AttachSurfaceSummary;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    ApiVersion, CancellationToken, HostConnectionInfo, HostMetadata, HostScope, ProviderId,
    RegisteredService, ServiceRequest,
};

#[test]
fn transport_liveness_reclaims_disconnected_owner_without_clients_plugin() {
    let registration = std::cell::Cell::new(Some(true));
    let registry = ViewRegistry::default();
    let caller = id(4001);
    let shared = SharedState::default();
    let entry = registry.entry(caller, &shared).unwrap();
    // No lifecycle facade, enabled-plugin list, or idle-time renewal exists.
    reap_disconnected(&registry.entries, |_| registration.get());
    assert!(registry.entries.lock().unwrap().contains_key(&caller));
    assert!(!entry.lock().unwrap().closed);
    registration.set(None);
    reap_disconnected(&registry.entries, |_| registration.get());
    assert!(registry.entries.lock().unwrap().contains_key(&caller));
    assert!(!entry.lock().unwrap().closed);
    registration.set(Some(false));
    reap_disconnected(&registry.entries, |_| registration.get());
    assert!(!registry.entries.lock().unwrap().contains_key(&caller));
    let entry = entry.lock().unwrap();
    assert!(entry.closed);
    assert!(*entry.state.runtime.stopped.borrow());
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn context(
    caller: Option<Uuid>,
    interface: &str,
    operation: &str,
    payload: Vec<u8>,
) -> NativeServiceContext {
    NativeServiceContext {
        plugin_id: "bmux.decoration".into(),
        request: ServiceRequest {
            caller_plugin_id: "bmux.theme".into(),
            service: RegisteredService {
                capability: HostScope::new("bmux.decoration.write").unwrap(),
                kind: ServiceKind::Command,
                interface_id: interface.into(),
                provider: ProviderId::Plugin("bmux.decoration".into()),
            },
            operation: operation.into(),
            payload,
        },
        required_capabilities: vec![],
        provided_capabilities: vec![],
        services: vec![],
        available_capabilities: vec![],
        enabled_plugins: vec![],
        plugin_search_roots: vec![],
        host: HostMetadata {
            product_name: "test".into(),
            product_version: "test".into(),
            plugin_api_version: ApiVersion::new(1, 0),
            plugin_abi_version: ApiVersion::new(1, 0),
        },
        connection: HostConnectionInfo {
            config_dir: String::new(),
            config_dir_candidates: vec![],
            runtime_dir: String::new(),
            data_dir: String::new(),
            state_dir: String::new(),
        },
        settings: None,
        plugin_settings_map: BTreeMap::new(),
        caller_client_id: caller,
        cancellation: CancellationToken::default(),
        host_kernel_bridge: None,
    }
}
fn call<T: serde::Serialize>(
    plugin: &DecorationPlugin,
    caller: Uuid,
    interface: &str,
    operation: &str,
    payload: &T,
) -> ServiceResponse {
    plugin.invoke_service(context(
        Some(caller),
        interface,
        operation,
        encode_service_message(payload).unwrap(),
    ))
}
fn layout(lifetime: Uuid, pane: Uuid, width: u16, revision: u64) -> view::UpdateLayoutRequest {
    let rect = crate::Rect {
        x: 0,
        y: 0,
        w: width,
        h: 8,
    };
    view::UpdateLayoutRequest {
        lifetime,
        revision,
        focused_pane_id: Some(pane),
        surfaces: vec![AttachSurfaceSummary {
            surface_id: pane,
            pane_id: Some(pane),
            rect: rect.clone(),
            content_rect: rect,
            visible: true,
        }],
    }
}
fn scene(plugin: &DecorationPlugin, caller: Uuid) -> crate::DecorationScene {
    let response = call(plugin, caller, "decoration-view-v1", "scene", &());
    decode_service_message(&response.payload).unwrap()
}

#[test]
fn caller_layout_focus_theme_checkpoint_and_lifetime_are_isolated() {
    let plugin = DecorationPlugin::new();
    let a = id(801);
    let b = id(802);
    let pane = id(803);
    let life_a = id(804);
    let life_b = id(805);
    let missing = plugin.invoke_service(context(
        None,
        "decoration-view-v1",
        "scene",
        encode_service_message(&()).unwrap(),
    ));
    assert!(missing.error.is_some());
    for (caller, life, width) in [(a, life_a, 20), (b, life_b, 40)] {
        let response = call(
            &plugin,
            caller,
            "decoration-view-v1",
            "update-layout",
            &layout(life, pane, width, 1),
        );
        assert_eq!(
            decode_service_message::<Result<(), String>>(&response.payload).unwrap(),
            Ok(())
        );
    }
    let before_b = scene(&plugin, b);
    assert_eq!(scene(&plugin, a).surfaces[&pane].rect.w, 20);
    assert_eq!(before_b.surfaces[&pane].rect.w, 40);
    let apply = commands::ApplyViewThemeExtensionRequest {
        toml: "border = 'double'".into(),
        config_dir_candidates: vec![],
    };
    let _ = call(
        &plugin,
        a,
        "decoration-commands",
        "apply-view-theme-extension",
        &apply,
    );
    let response = call(
        &plugin,
        a,
        "decoration-commands",
        "checkpoint-view-script-state",
        &(),
    );
    assert!(response.error.is_none());
    assert_eq!(scene(&plugin, b), before_b);
    let old_revision = scene(&plugin, a).revision;
    let close = call(
        &plugin,
        a,
        "decoration-view-v1",
        "close",
        &view::CloseRequest { lifetime: life_a },
    );
    assert_eq!(
        decode_service_message::<Result<(), String>>(&close.payload).unwrap(),
        Ok(())
    );
    let old = call(
        &plugin,
        a,
        "decoration-view-v1",
        "update-layout",
        &layout(life_a, pane, 22, 2),
    );
    assert!(
        decode_service_message::<Result<(), String>>(&old.payload)
            .unwrap()
            .is_err()
    );
    let _ = call(
        &plugin,
        a,
        "decoration-view-v1",
        "update-layout",
        &layout(id(806), pane, 24, 1),
    );
    assert!(scene(&plugin, a).revision > old_revision);
    assert_eq!(scene(&plugin, b), before_b);
}

#[test]
fn hiding_surface_preserves_explicit_pane_override() {
    let mut state = State::default();
    let pane = id(901);
    crate::set_pane_border_direct(&mut state, pane, crate::BorderStyle::Double);
    let request = layout(id(902), pane, 20, 1);
    crate::apply_attach_layout_snapshot(
        &mut state,
        &bmux_attach_layout_protocol::attach_layout_protocol::AttachLayoutSnapshot {
            surfaces: request.surfaces,
            revision: 1,
        },
    );
    crate::apply_attach_layout_snapshot(
        &mut state,
        &bmux_attach_layout_protocol::attach_layout_protocol::AttachLayoutSnapshot {
            surfaces: vec![],
            revision: 2,
        },
    );
    assert_eq!(state.panes[&pane].border, crate::BorderStyle::Double);
    assert!(state.geometry.is_empty());
}

#[test]
fn generated_versioned_input_dispatch_never_reaches_global_vm() {
    let plugin = DecorationPlugin::new();
    let interface = bmux_decoration_plugin_api::decoration_input_hooks::INTERFACE_ID;
    assert_eq!(interface.as_str(), "decoration-input-hooks/v2");
    // A malformed private request must fail decoding in the private dispatcher,
    // not be treated as an unknown interface and fall through to legacy state.
    let response = plugin.views.dispatch(
        &context(Some(id(950)), interface.as_str(), "handle-input", vec![]),
        &plugin.state,
    );
    assert!(response.is_some());
    assert!(response.unwrap().error.is_some());
    assert!(plugin.build_scene().surfaces.is_empty());
}

#[test]
fn view_limit_is_explicit_not_live_state_eviction() {
    let registry = ViewRegistry::default();
    let shared = SharedState::default();
    let first = registry.entry(id(1000), &shared).unwrap();
    for n in 1..MAX_VIEWS {
        registry.entry(id(1000 + n as u128), &shared).unwrap();
    }
    assert!(registry.entry(id(2000), &shared).is_err());
    assert!(Arc::ptr_eq(
        &first,
        &registry.entry(id(1000), &shared).unwrap()
    ));
}

#[test]
fn two_valid_versioned_input_scripts_have_independent_vm_and_hooks() {
    use bmux_decoration_plugin_api::decoration_input_hooks::{self as wire, client};
    let path = std::env::temp_dir().join(format!("bmux-view-input-{}.lua", std::process::id()));
    std::fs::write(
        &path,
        r"
        local count = 0
        function decorate(message)
            if message.kind == 'input' then
                count = count + 1
                return { consumed = count % 2 == 1, surfaces = {} }
            end
            return { surfaces = {} }
        end
    ",
    )
    .unwrap();
    let plugin = DecorationPlugin::new();
    let a = id(3001);
    let b = id(3002);
    let toml = format!(
        "script = {:?}\n[input]\nkeys = ['x']\nscope = 'always'\n",
        path.to_string_lossy()
    );
    for caller in [a, b] {
        let response = call(
            &plugin,
            caller,
            "decoration-commands",
            "apply-view-theme-extension",
            &commands::ApplyViewThemeExtensionRequest {
                toml: toml.clone(),
                config_dir_candidates: vec![],
            },
        );
        let result: Result<(), crate::ValidationResult> =
            decode_service_message(&response.payload).unwrap();
        assert_eq!(result, Ok(()));
    }
    let hook_a = scene(&plugin, a).input_hooks[0].id.clone();
    let hook_b = scene(&plugin, b).input_hooks[0].id.clone();
    assert_ne!(hook_a, hook_b);
    let request = |hook_id: String| client::HandleInputRequest {
        event: wire::InputEvent {
            hook_id,
            event_kind: "key".into(),
            phase: "press".into(),
            button: None,
            key: Some("x".into()),
            col: None,
            row: None,
            wheel_delta: 0,
            modifiers: wire::InputModifiers {
                shift: false,
                alt: false,
                control: false,
                super_key: false,
                hyper: false,
                meta: false,
            },
            focused_pane: None,
            hovered_pane: None,
        },
    };
    let interface = wire::INTERFACE_ID;
    let foreign = call(
        &plugin,
        b,
        interface.as_str(),
        "handle-input",
        &request(hook_a.clone()),
    );
    assert!(foreign.error.is_some());
    for (caller, hook, expected) in [
        (a, hook_a.clone(), true),
        (a, hook_a, false),
        (b, hook_b, true),
    ] {
        let response = call(
            &plugin,
            caller,
            interface.as_str(),
            "handle-input",
            &request(hook),
        );
        assert!(response.error.is_none());
        let result: wire::InputResult = decode_service_message(&response.payload).unwrap();
        assert_eq!(result.consumed, expected);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn subscriptions_cancel_on_reapply_empty_clear_and_checkpoint_without_events() {
    use std::sync::atomic::Ordering;
    let shared = SharedState::default();
    let kind = bmux_plugin_sdk::PluginEventKind::from_owned(format!(
        "test/view-cancel-{}",
        shared.runtime.hook_scope
    ));
    let sender = bmux_plugin::global_event_bus()
        .register_channel_with_json_projection::<String>(kind.clone());
    let access = crate::decoration_state::ScriptAccessSpec {
        state_channels: vec![],
        event_channels: vec![kind.as_str().into()],
        services: vec![],
    };
    let wait = || {
        for _ in 0..1000 {
            if shared.runtime.active_subscriptions.load(Ordering::SeqCst) == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("obsolete subscriptions remain live");
    };
    for generation in 1..=12 {
        let access = access.clone();
        let shared_for_install = shared.clone();
        shared.with_state(move |state| {
            state.script_subscription_generation = generation;
            crate::install_script_event_subscriptions(
                &shared_for_install,
                state,
                Some(access),
                generation,
            );
        });
        crate::apply_theme_extension_toml(&shared, "", &[], &crate::ScriptHostAccess::default())
            .unwrap();
        wait();
        assert_eq!(sender.receiver_count(), 0);
    }
    let access = access.clone();
    let shared_for_install = shared.clone();
    shared.with_state(move |state| {
        state.script_subscription_generation = 100;
        crate::install_script_event_subscriptions(&shared_for_install, state, Some(access), 100);
    });
    send_engine_command_blocking(&shared, |reply| DecorationEngineCommand::ScriptCheckpoint {
        restore: false,
        reply,
    })
    .unwrap()
    .unwrap();
    wait();
    assert_eq!(sender.receiver_count(), 0);
}
