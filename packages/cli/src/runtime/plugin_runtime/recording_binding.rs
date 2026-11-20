//! Explicit recording activation composition; recording behavior stays plugin-owned.
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_plugin_sdk::{
    PluginCommandError,
    definition::{PluginHostServices, PluginLifecycleContext, PluginResult},
};
use bmux_recording_plugin_api::{RecordingPluginConfig, recording_events};
use std::sync::{Arc, Mutex, RwLock};

struct OwnedSink(Mutex<Option<bmux_recording_runtime::RecordingSinkHandle>>);
impl bmux_recording_runtime::RecordingSink for OwnedSink {
    fn record(
        &self,
        kind: bmux_recording_protocol::RecordingEventKind,
        payload: bmux_recording_protocol::RecordingPayload<bmux_ipc::Event, bmux_ipc::ErrorCode>,
        meta: bmux_recording_runtime::RecordMeta,
    ) {
        let sink = self.0.lock().ok().and_then(|slot| slot.clone());
        if let Some(sink) = sink {
            sink.0.record(kind, payload, meta);
        }
    }
    fn accepts(
        &self,
        kind: bmux_recording_protocol::RecordingEventKind,
        meta: bmux_recording_runtime::RecordMeta,
    ) -> bool {
        let sink = self.0.lock().ok().and_then(|slot| slot.clone());
        sink.is_some_and(|sink| sink.0.accepts(kind, meta))
    }
    fn record_checked(
        &self,
        kind: bmux_recording_protocol::RecordingEventKind,
        payload: bmux_recording_protocol::RecordingPayload<bmux_ipc::Event, bmux_ipc::ErrorCode>,
        meta: bmux_recording_runtime::RecordMeta,
    ) -> Result<(), String> {
        let sink = self
            .0
            .lock()
            .map_err(|_| "recording sink state poisoned".to_string())?
            .clone()
            .ok_or_else(|| "recording sink activation withdrawn".to_string())?;
        sink.0.record_checked(kind, payload, meta)
    }
}
struct RecordingPorts {
    state: Arc<bmux_plugin::PluginStateRegistry>,
    events: Arc<bmux_plugin::EventBus>,
    sink: Arc<OwnedSink>,
}
impl bmux_recording_plugin::RecordingHost for RecordingPorts {
    fn install_sink(
        &self,
        sink: bmux_recording_runtime::RecordingSinkHandle,
    ) -> Result<(), String> {
        self.events
            .register_channel::<recording_events::RecordingEvent>(recording_events::EVENT_KIND);
        let mut slot = self
            .sink
            .0
            .lock()
            .map_err(|_| "recording sink state poisoned".to_string())?;
        if slot.is_some() {
            return Err("recording sink already installed".into());
        }
        *slot = Some(sink);
        drop(slot);
        self.state.register(&Arc::new(RwLock::new(
            bmux_recording_runtime::RecordingSinkHandle::from_arc(self.sink.clone()),
        )));
        Ok(())
    }
    fn remove_sink(&self) -> Result<(), String> {
        self.sink
            .0
            .lock()
            .map_err(|_| "recording sink state poisoned".to_string())?
            .take();
        Ok(())
    }
    fn publish(&self, event: recording_events::RecordingEvent) -> Result<(), String> {
        self.events
            .emit(&recording_events::EVENT_KIND, event)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

pub(super) fn configuration(config: &BmuxConfig, paths: &ConfigPaths) -> RecordingPluginConfig {
    bmux_recording_bootstrap::recording_plugin_config(
        config,
        paths,
        false,
        &bmux_server::rolling_recording_settings_from_config(config),
        None,
    )
}

/// Discovery publishes the same author implementation without inventing runtime
/// configuration. Production command/server composition replaces this binding
/// with `definition` using the selected snapshot before loading an instance.
pub(super) fn unbound_definition() -> bmux_plugin_sdk::definition::PluginDefinition {
    bmux_recording_plugin::plugin_definition_with_activation_configuration(|_, _| {
        Err(PluginCommandError::unavailable(
            "recording activation requires an explicitly selected host configuration",
        ))
    })
}

pub(in crate::runtime) fn definition(
    config: RecordingPluginConfig,
    state: Arc<bmux_plugin::PluginStateRegistry>,
    events: Arc<bmux_plugin::EventBus>,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    bmux_recording_plugin::plugin_definition_with_activation_configuration(
        move |context, services| {
            bind_configuration(
                context,
                services,
                config.clone(),
                state.clone(),
                events.clone(),
            )
        },
    )
}
fn bind_configuration(
    context: &PluginLifecycleContext,
    services: &PluginHostServices,
    config: RecordingPluginConfig,
    state: Arc<bmux_plugin::PluginStateRegistry>,
    events: Arc<bmux_plugin::EventBus>,
) -> PluginResult<bmux_recording_plugin::RecordingActivationConfiguration> {
    let native = &context.native;
    let service_context = bmux_plugin_sdk::NativeServiceContext {
        plugin_id: native.plugin_id.clone(),
        request: bmux_plugin_sdk::ServiceRequest {
            caller_plugin_id: native.plugin_id.clone(),
            service: bmux_plugin_sdk::RegisteredService {
                capability: bmux_plugin_sdk::HostScope::new("bmux.recording")
                    .map_err(|error| PluginCommandError::failed(error.to_string()))?,
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "recording-commands".into(),
                provider: bmux_plugin_sdk::ProviderId::Plugin(native.plugin_id.clone()),
            },
            operation: "activation".into(),
            payload: Vec::new(),
        },
        required_capabilities: native.required_capabilities.clone(),
        provided_capabilities: native.provided_capabilities.clone(),
        services: native.services.clone(),
        available_capabilities: native.available_capabilities.clone(),
        enabled_plugins: native.enabled_plugins.clone(),
        plugin_search_roots: native.plugin_search_roots.clone(),
        host: native.host.clone(),
        connection: native.connection.clone(),
        settings: native.settings.clone(),
        plugin_settings_map: native.plugin_settings_map.clone(),
        caller_client_id: None,
        cancellation: bmux_plugin_sdk::CancellationToken::none(),
        host_kernel_bridge: None,
        host_service_port: Some(services.clone()),
    };
    let caller = bmux_plugin::TypedServiceCaller::from_service_context(&service_context);
    Ok(bmux_recording_plugin::RecordingActivationConfiguration {
        config,
        host: Arc::new(RecordingPorts {
            state,
            events,
            sink: Arc::new(OwnedSink(Mutex::new(None))),
        }),
        process_caller: Some(Arc::new(caller)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_recording_plugin::RecordingHost;
    use bmux_recording_protocol::{RecordingEventKind, RecordingPayload};
    use bmux_recording_runtime::{RecordMeta, RecordingSink, RecordingSinkHandle};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSink(Arc<AtomicUsize>);

    impl RecordingSink for CountingSink {
        fn record(
            &self,
            _kind: RecordingEventKind,
            _payload: RecordingPayload<bmux_ipc::Event, bmux_ipc::ErrorCode>,
            _meta: RecordMeta,
        ) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn record_checked(
            &self,
            kind: RecordingEventKind,
            payload: RecordingPayload<bmux_ipc::Event, bmux_ipc::ErrorCode>,
            meta: RecordMeta,
        ) -> Result<(), String> {
            self.record(kind, payload, meta);
            Ok(())
        }
    }

    #[test]
    fn standalone_configuration_preserves_capture_defaults_without_server_startup_intent() {
        let mut config = BmuxConfig::default();
        config.recording.enabled = true;
        config.recording.rolling_window_secs = 120;
        let paths = ConfigPaths::new(
            "/selected/config".into(),
            "/selected/runtime".into(),
            "/selected/data".into(),
            "/selected/state".into(),
        );
        let selected = configuration(&config, &paths);
        let expected = bmux_server::rolling_recording_settings_from_config(&config);
        assert_eq!(selected.rolling_defaults.window_secs, expected.window_secs);
        assert_eq!(selected.rolling_defaults.event_kinds, expected.event_kinds);
        assert_eq!(selected.recordings_dir, config.recordings_dir(&paths));
        assert!(!selected.rolling_auto_start);
        assert!(selected.startup_recording.is_none());
    }

    #[test]
    fn sink_is_published_only_to_selected_host_and_withdraws_retained_handles() {
        let state = Arc::new(bmux_plugin::PluginStateRegistry::new());
        let other = bmux_plugin::PluginStateRegistry::new();
        let host = RecordingPorts {
            state: state.clone(),
            events: Arc::new(bmux_plugin::EventBus::new()),
            sink: Arc::new(OwnedSink(Mutex::new(None))),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        host.install_sink(RecordingSinkHandle::new(CountingSink(calls.clone())))
            .unwrap();
        assert!(other.get::<RecordingSinkHandle>().is_none());
        let retained = state.get::<RecordingSinkHandle>().unwrap();
        let sink = retained.read().unwrap().clone();
        let enqueue = || {
            sink.0.record_checked(
                RecordingEventKind::PaneOutputRaw,
                RecordingPayload::Bytes { data: vec![1] },
                RecordMeta::default(),
            )
        };
        enqueue().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(host.install_sink(RecordingSinkHandle::noop()).is_err());
        host.remove_sink().unwrap();
        assert!(enqueue().unwrap_err().contains("withdrawn"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
