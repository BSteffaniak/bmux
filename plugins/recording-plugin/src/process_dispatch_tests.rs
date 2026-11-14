use super::*;
use bmux_plugin_sdk::{NativeCommandInvocationSource, NativeServiceContext, ServiceRequest};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn caller(calls: Arc<AtomicUsize>, worker: std::thread::ThreadId, fail: bool) -> ProcessCaller {
    let command = crate::tests::command_context(
        "recording-process-start",
        &[],
        NativeCommandInvocationSource::Cli,
    );
    let services =
        bmux_plugin_sdk::PluginHostServices::new(|_| panic!("synchronous fallback forbidden"))
            .with_async(move |request| {
                let calls = calls.clone();
                Box::pin(async move {
                    // Only the trusted host may grant worker admission. This test
                    // port owns its scheduler; recording only awaits the capability.
                    tokio::task::spawn_blocking(move || {
                        assert_ne!(std::thread::current().id(), worker);
                        assert_eq!(request.capability, "test.selected");
                        assert_eq!(request.payload, [1, 2, 3]);
                        calls.fetch_add(1, Ordering::SeqCst);
                        if fail {
                            Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                                details: "uncertain outcome; no replay".into(),
                            })
                        } else {
                            Ok(vec![4, 5])
                        }
                    })
                    .await
                    .expect("test host worker")
                })
            });
    ProcessCaller::Invocation(Box::new(NativeServiceContext {
        plugin_id: command.plugin_id,
        request: ServiceRequest {
            caller_plugin_id: "test".into(),
            service: bmux_plugin_sdk::RegisteredService {
                capability: bmux_plugin_sdk::HostScope::new("test.selected").unwrap(),
                kind: ServiceKind::Command,
                interface_id: "test-interface".into(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            },
            operation: "effect".into(),
            payload: Vec::new(),
        },
        required_capabilities: command.required_capabilities,
        provided_capabilities: command.provided_capabilities,
        services: command.services,
        available_capabilities: command.available_capabilities,
        enabled_plugins: command.enabled_plugins,
        plugin_search_roots: command.plugin_search_roots,
        host: command.host,
        connection: command.connection,
        settings: command.settings,
        plugin_settings_map: command.plugin_settings_map,
        caller_client_id: command.caller_client_id,
        cancellation: bmux_plugin_sdk::CancellationToken::default(),
        host_kernel_bridge: None,
        host_service_port: Some(services),
    }))
}

#[tokio::test(flavor = "current_thread")]
async fn missing_async_capability_never_uses_synchronous_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut selected = caller(calls.clone(), std::thread::current().id(), false);
    let ProcessCaller::Invocation(context) = &mut selected else {
        panic!("test invocation");
    };
    context.host_service_port = Some(bmux_plugin_sdk::PluginHostServices::new(|_| {
        panic!("no sync fallback")
    }));
    let mut client = ProcessDispatchClient::new(&selected);
    assert!(
        client
            .invoke_service_raw(
                "test.selected",
                bmux_ipc::InvokeServiceKind::Command,
                "test-interface",
                "effect",
                vec![1, 2, 3]
            )
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn selected_async_capability_is_awaited_once_without_sync_fallback() {
    for fail in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let caller = caller(calls.clone(), std::thread::current().id(), fail);
        let mut client = ProcessDispatchClient::new(&caller);
        let result = client
            .invoke_service_raw(
                "test.selected",
                bmux_ipc::InvokeServiceKind::Command,
                "test-interface",
                "effect",
                vec![1, 2, 3],
            )
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        if fail {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), vec![4, 5]);
        }
    }
}
