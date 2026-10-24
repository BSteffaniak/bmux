use super::*;
use bmux_plugin::test_support::{TestServiceRouter, install_test_service_router};
use bmux_plugin_sdk::{decode_service_message, encode_service_message};

type Preferences = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

#[allow(clippy::result_large_err)] // TestServiceRouter fixes this external error type.
fn router(preferences: &Preferences, writes: &Arc<Mutex<usize>>) -> TestServiceRouter {
    let preferences = Arc::clone(preferences);
    let writes = Arc::clone(writes);
    crate::tests::checked_test_router(Arc::new(
        move |_, _, capability, _, _, operation, payload| match (capability, operation) {
            ("bmux.storage", "get") => {
                let request: bmux_plugin_sdk::StorageGetRequest = decode_service_message(&payload)?;
                encode_service_message(&bmux_plugin_sdk::StorageGetResponse {
                    value: preferences
                        .lock()
                        .expect("preferences")
                        .get(request.key.as_str())
                        .cloned(),
                })
            }
            ("bmux.storage", "set") => {
                let request: bmux_plugin_sdk::StorageSetRequest = decode_service_message(&payload)?;
                preferences
                    .lock()
                    .expect("preferences")
                    .insert(request.key.as_str().into(), request.value);
                *writes.lock().expect("writes") += 1;
                encode_service_message(&())
            }
            ("bmux.decoration.write", "apply-view-theme-extension") => {
                encode_service_message(&Ok::<
                    (),
                    bmux_decoration_plugin_api::decoration_state::ValidationResult,
                >(()))
            }
            (
                "bmux.decoration.write",
                "checkpoint-view-script-state"
                | "restore-view-script-state"
                | "discard-view-script-checkpoint",
            ) => encode_service_message(&Ok::<(), String>(())),
            _ => panic!("unexpected {capability}/{operation}"),
        },
    ))
}

fn context(owner: u8) -> NativeServiceContext {
    let mut context =
        crate::tests::service_context(Some(toml::from_str("theme = 'minimal'").expect("settings")));
    context.caller_client_id = Some(
        format!("00000000-0000-0000-0000-{owner:012}")
            .parse()
            .expect("uuid"),
    );
    context
}

fn invoke<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    plugin: &ThemePlugin,
    context: &NativeServiceContext,
    interface: &str,
    operation: &str,
    request: &T,
) -> R {
    let mut context = context.clone();
    context.request.service.interface_id = interface.into();
    context.request.operation = operation.into();
    context.request.payload = encode_service_message(request).expect("request");
    let response = plugin.invoke_service(context);
    assert!(response.error.is_none(), "{:?}", response.error);
    decode_service_message(&response.payload).expect("response")
}

fn current(
    plugin: &ThemePlugin,
    context: &NativeServiceContext,
) -> control_contract::theme_control_v1::Snapshot {
    invoke(plugin, context, "theme-control-v1", "current", &())
}

fn select(
    plugin: &ThemePlugin,
    context: &NativeServiceContext,
    revision: u64,
    name: &str,
) -> control_contract::theme_control_v1::Snapshot {
    let result: Result<control_contract::theme_control_v1::Snapshot, String> = invoke(
        plugin,
        context,
        "theme-control-v1",
        "select",
        &control_contract::theme_control_v1::client::SelectRequest {
            expected_revision: revision,
            selection: ThemeSelection::Preset { name: name.into() },
        },
    );
    result.expect("selection")
}

#[test]
fn owners_preview_cancel_and_close_are_isolated() {
    let preferences = Preferences::default();
    let writes = Arc::new(Mutex::new(0));
    let _router = install_test_service_router(router(&preferences, &writes));
    let plugin = ThemePlugin::default();
    let a = context(1);
    let b = context(2);
    current(&plugin, &a);
    current(&plugin, &b);
    select(&plugin, &a, 0, "hacker");
    select(&plugin, &b, 0, "minimal");
    let av = plugin.view(&a).expect("a");
    let bv = plugin.view(&b).expect("b");
    let at = av.state.begin_preview(&a, 1).expect("a preview");
    let bt = bv.state.begin_preview(&b, 1).expect("b preview");
    assert_ne!(at, bt);
    av.state
        .preview_theme(
            &a,
            at,
            &ThemeSelection::Preset {
                name: "minimal".into(),
            },
        )
        .expect("preview");
    bv.state
        .preview_theme(
            &b,
            bt,
            &ThemeSelection::Preset {
                name: "hacker".into(),
            },
        )
        .expect("preview");
    assert!(bv.state.cancel_preview(&b, at).is_err());
    av.state.cancel_preview(&a, at).expect("cancel");
    assert_eq!(
        av.effective().expect("a effective").appearance.foreground,
        "#39ff14"
    );
    av.close();
    assert!(av.closed.load(std::sync::atomic::Ordering::Acquire));
    assert!(!bv.closed.load(std::sync::atomic::Ordering::Acquire));
    assert!(bv.state.preview.lock().expect("b preview").is_some());
    bv.state.cancel_preview(&b, bt).expect("cancel b");
    assert_eq!(current(&plugin, &b).revision, 1);
    assert_eq!(*writes.lock().expect("writes"), 0);
}

#[test]
fn explicit_default_overrides_legacy_policy_only_for_new_and_reset_views() {
    let preferences = Preferences::default();
    let writes = Arc::new(Mutex::new(0));
    let _router = install_test_service_router(router(&preferences, &writes));
    let plugin = ThemePlugin::default();
    let a = context(1);
    current(&plugin, &a);
    select(&plugin, &a, 0, "minimal");
    let default = plugin.default_snapshot(&a).expect("default");
    assert_eq!(default.selection, ThemeSelection::Configured);
    plugin
        .select_default(
            &a,
            0,
            ThemeSelection::Preset {
                name: "hacker".into(),
            },
        )
        .expect("save default");
    assert_eq!(
        current(&plugin, &a).selection,
        ThemeSelection::Preset {
            name: "minimal".into()
        }
    );
    assert_eq!(
        current(&plugin, &context(2)).selection,
        plugin.default_snapshot(&a).expect("default").selection
    );
    let reset: Result<control_contract::theme_control_v1::Snapshot, String> = invoke(
        &plugin,
        &a,
        "theme-control-v1",
        "reset-view",
        &control_contract::theme_control_v1::client::ResetViewRequest {
            expected_revision: 1,
        },
    );
    assert_eq!(
        reset.expect("reset").selection,
        ThemeSelection::Preset {
            name: "hacker".into()
        }
    );
    assert_eq!(*writes.lock().expect("writes"), 1);
    // Durable explicit defaults survive a fresh owner even under declared_on_connect.
    assert_eq!(
        current(&ThemePlugin::default(), &context(3)).selection,
        ThemeSelection::Preset {
            name: "hacker".into()
        }
    );
}

#[test]
fn reset_preserves_configured_split_composition() {
    let preferences = Preferences::default();
    let writes = Arc::new(Mutex::new(0));
    let _router = install_test_service_router(router(&preferences, &writes));
    let plugin = ThemePlugin::default();
    let mut a = context(1);
    a.settings = Some(
        toml::from_str("appearance_themes=['hacker']\ncomponent_themes=['pulse-border']")
            .expect("settings"),
    );
    current(&plugin, &a);
    let expected = plugin
        .view(&a)
        .expect("view")
        .effective()
        .expect("baseline");
    select(&plugin, &a, 0, "minimal");
    let reset: Result<control_contract::theme_control_v1::Snapshot, String> = invoke(
        &plugin,
        &a,
        "theme-control-v1",
        "reset-view",
        &control_contract::theme_control_v1::client::ResetViewRequest {
            expected_revision: 1,
        },
    );
    assert_eq!(reset.expect("reset").selection, ThemeSelection::Configured);
    let actual = plugin
        .view(&a)
        .expect("view")
        .effective()
        .expect("effective");
    assert_eq!(actual.appearance, expected.appearance);
    assert_eq!(actual.plugins, expected.plugins);
    assert_eq!(*writes.lock().expect("writes"), 0);
}

#[test]
fn scope_tracks_latest_baseline_beneath_selection_and_preview() {
    let preferences = Preferences::default();
    let writes = Arc::new(Mutex::new(0));
    let _router = install_test_service_router(router(&preferences, &writes));
    let root = std::env::temp_dir().join(format!("bmux-theme-scope-{}", std::process::id()));
    let a_dir = root.join("a");
    let b_dir = root.join("b");
    std::fs::create_dir_all(&a_dir).expect("a");
    std::fs::create_dir_all(&b_dir).expect("b");
    std::fs::write(root.join("bmux.toml"), "").expect("global");
    std::fs::write(
        a_dir.join("bmux.toml"),
        "[plugins.settings.\"bmux.theme\"]\ntheme='hacker'\n",
    )
    .expect("a config");
    std::fs::write(
        b_dir.join("bmux.toml"),
        "[plugins.settings.\"bmux.theme\"]\ntheme='tetris'\n",
    )
    .expect("b config");
    let plugin = ThemePlugin::default();
    let mut c = context(4);
    c.connection.config_dir = root.to_string_lossy().into_owned();
    c.connection.config_dir_candidates = vec![c.connection.config_dir.clone()];
    let _: control_contract::theme_state::Appearance =
        invoke(&plugin, &c, "theme-state", "active-appearance", &());
    let cwd_query = |path: &std::path::Path| -> control_contract::theme_state::Appearance {
        invoke(
            &plugin,
            &c,
            "theme-state",
            "active-appearance-for-cwd",
            &control_contract::theme_state::client::ActiveAppearanceForCwdRequest {
                cwd: path.to_string_lossy().into_owned(),
            },
        )
    };
    assert_eq!(cwd_query(&a_dir).foreground, "#39ff14");
    assert_eq!(cwd_query(&b_dir).foreground, "#d8e8ff");
    assert_eq!(cwd_query(&a_dir).foreground, "#39ff14");
    select(&plugin, &c, 0, "minimal");
    let selected = cwd_query(&a_dir);
    assert_eq!(cwd_query(&b_dir), selected);
    let reset: Result<control_contract::theme_control_v1::Snapshot, String> = invoke(
        &plugin,
        &c,
        "theme-control-v1",
        "reset-view",
        &control_contract::theme_control_v1::client::ResetViewRequest {
            expected_revision: 1,
        },
    );
    assert_eq!(reset.expect("reset").selection, ThemeSelection::Configured);
    assert_eq!(cwd_query(&b_dir).foreground, "#d8e8ff");
    let v = plugin.view(&c).expect("view");
    let token = v.state.begin_preview(&c, 2).expect("preview");
    v.state
        .preview_theme(
            &c,
            token,
            &ThemeSelection::Preset {
                name: "minimal".into(),
            },
        )
        .expect("preview");
    cwd_query(&a_dir);
    assert!(v.state.preview.lock().expect("preview").is_some());
    v.state.cancel_preview(&c, token).expect("cancel");
    let reset: Result<control_contract::theme_control_v1::Snapshot, String> = invoke(
        &plugin,
        &c,
        "theme-control-v1",
        "reset-view",
        &control_contract::theme_control_v1::client::ResetViewRequest {
            expected_revision: 2,
        },
    );
    reset.expect("reset preview baseline");
    assert_eq!(cwd_query(&a_dir).foreground, "#39ff14");
    std::fs::remove_dir_all(root).expect("cleanup owned test directory");
}

#[derive(Default)]
struct Capture {
    values: Mutex<BTreeMap<uuid::Uuid, Vec<u8>>>,
    publishes: std::sync::atomic::AtomicUsize,
    liveness: Mutex<BTreeMap<uuid::Uuid, bool>>,
}
impl bmux_plugin_sdk::WireEventSink for Capture {
    fn is_connected(&self, recipient: uuid::Uuid) -> Option<bool> {
        self.liveness
            .lock()
            .expect("liveness")
            .get(&recipient)
            .copied()
    }
    fn publish(&self, _event: bmux_ipc::Event) -> Result<(), bmux_plugin_sdk::WireEventSinkError> {
        panic!("personal appearance must never broadcast")
    }
    fn publish_retained_to(
        &self,
        recipient: uuid::Uuid,
        kind: String,
        payload: Vec<u8>,
    ) -> Result<(), bmux_plugin_sdk::WireEventSinkError> {
        assert_eq!(kind, crate::RUNTIME_APPEARANCE_STATE_KIND.as_str());
        self.values
            .lock()
            .expect("values")
            .insert(recipient, payload);
        self.publishes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    fn clear_retained_to(
        &self,
        recipient: uuid::Uuid,
        _kind: &str,
    ) -> Result<(), bmux_plugin_sdk::WireEventSinkError> {
        self.values.lock().expect("values").remove(&recipient);
        Ok(())
    }
}

#[test]
fn publication_is_private_cached_and_close_cannot_reretain() {
    let preferences = Preferences::default();
    let writes = Arc::new(Mutex::new(0));
    let _router = install_test_service_router(router(&preferences, &writes));
    let capture = Arc::new(Capture::default());
    let handle = Arc::new(std::sync::RwLock::new(
        bmux_plugin_sdk::WireEventSinkHandle::from_arc(capture.clone()),
    ));
    let registry = bmux_plugin::global_plugin_state_registry();
    let previous = registry.register(&handle);
    let sentinel = crate::RuntimeAppearance {
        foreground: "#123456".into(),
        ..crate::RuntimeAppearance::default()
    };
    let appearance_sender = bmux_plugin::global_event_bus()
        .register_state_channel::<crate::RuntimeAppearance>(
            crate::RUNTIME_APPEARANCE_STATE_KIND,
            sentinel.clone(),
        );
    appearance_sender.send_replace(Arc::new(sentinel.clone()));
    drop(appearance_sender);
    let plugin = ThemePlugin::default();
    let a = context(5);
    current(&plugin, &a);
    let first = capture.publishes.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(first, 1, "initial hydration retained once");
    for _ in 0..10 {
        let _: control_contract::theme_state::Appearance =
            invoke(&plugin, &a, "theme-state", "active-appearance", &());
        current(&plugin, &a);
    }
    assert_eq!(
        capture.publishes.load(std::sync::atomic::Ordering::Relaxed),
        first
    );
    select(&plugin, &a, 0, "hacker");
    assert_eq!(
        capture.publishes.load(std::sync::atomic::Ordering::Relaxed),
        first + 1
    );
    let global = bmux_plugin::global_event_bus()
        .subscribe_state::<crate::RuntimeAppearance>(&crate::RUNTIME_APPEARANCE_STATE_KIND)
        .expect("global")
        .0;
    assert_eq!(*global, sentinel);
    let view = plugin.view(&a).expect("view");
    std::thread::scope(|scope| {
        let operation = view.operation.lock().expect("operation");
        let closing = Arc::clone(&view);
        let close = scope.spawn(move || closing.close());
        view.publish();
        drop(operation);
        close.join().expect("close");
    });
    let operation = view.operation.lock().expect("operation");
    view.publish();
    drop(operation);
    assert!(
        !capture
            .values
            .lock()
            .expect("values")
            .contains_key(&a.caller_client_id.expect("caller"))
    );
    drop(plugin);
    if let Some(previous) = previous {
        registry.register(&previous);
    } else {
        registry.register(&Arc::new(std::sync::RwLock::new(
            bmux_plugin_sdk::WireEventSinkHandle::noop(),
        )));
    }
}

#[test]
fn neutral_liveness_retires_without_clients_plugin_and_preserves_unknown() {
    // No clients service/event provider or test router is installed. The sink is
    // the transport authority; an unsupported/no-op observation is not death.
    let plugin = ThemePlugin::default();
    let a = context(7);
    let b = context(8);
    let c = context(9);
    let av = plugin.view(&a).expect("a");
    let bv = plugin.view(&b).expect("b");
    let cv = plugin.view(&c).expect("c");
    let capture = Capture::default();
    capture.liveness.lock().expect("liveness").extend([
        (a.caller_client_id.expect("a"), false),
        (b.caller_client_id.expect("b"), true),
    ]);
    assert!(retire_disconnected(&plugin.views, &bmux_plugin_sdk::NoopWireEventSink).is_empty());
    let retired = retire_disconnected(&plugin.views, &capture);
    assert_eq!(retired.len(), 1);
    assert!(Arc::ptr_eq(&retired[0], &av));
    // Retirement returns handles for close outside the global registry lock.
    assert!(plugin.views.try_lock().is_ok());
    for view in retired {
        view.close();
    }
    assert!(av.closed.load(std::sync::atomic::Ordering::Acquire));
    assert!(!bv.closed.load(std::sync::atomic::Ordering::Acquire));
    assert!(!cv.closed.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(plugin.views.lock().expect("views").len(), 2);
    capture
        .liveness
        .lock()
        .expect("liveness")
        .insert(c.caller_client_id.expect("c"), false);
    let retired = retire_disconnected(&plugin.views, &capture);
    assert_eq!(retired.len(), 1);
    assert!(Arc::ptr_eq(&retired[0], &cv));
    for view in retired {
        view.close();
    }
    assert_eq!(plugin.views.lock().expect("views").len(), 1);
}
