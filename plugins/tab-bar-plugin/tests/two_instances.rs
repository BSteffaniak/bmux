//! Two instances of one plugin in one host share nothing but the host itself.
//!
//! Both instances are created from the plugin's real definition through the
//! scoped loader, each activation receives its own view of the host's single
//! event bus, and every mutation goes through the plugin's service boundary.
//! A process-wide store or bus would leak one instance's state into the other.
use bmux_plugin::{DefinitionArtifact, EventBus, LoadedExecution};
use bmux_plugin_runtime::{PluginConcurrencyConfig, PluginRuntimeScopeId};
use bmux_plugin_sdk::abi::{NativeOperation, NativeOutcome};
use bmux_plugin_sdk::definition::ComponentRole;
use bmux_plugin_sdk::{
    HostScope, NativeLifecycleContext, NativeServiceContext, PluginInvocationId, ProviderId,
    RegisteredService, ServiceKind, ServiceRequest,
};
use bmux_tab_bar_plugin_api::status_items::client::SetStatusItemRequest;
use bmux_tab_bar_plugin_api::status_items_list::{self, Severity, StatusItem, StatusItemSnapshot};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

const PLUGIN_ID: &str = "bmux.tab_bar";

fn host_json() -> serde_json::Value {
    serde_json::json!({
        "product_name": "two-instances", "product_version": "1",
        "plugin_api_version": {"major": 1, "minor": 0},
        "plugin_abi_version": {"major": 2, "minor": 0}
    })
}

fn connection_json() -> serde_json::Value {
    serde_json::json!({"config_dir": "", "runtime_dir": "", "data_dir": "", "state_dir": ""})
}

fn lifecycle_context() -> NativeLifecycleContext {
    serde_json::from_value(serde_json::json!({
        "plugin_id": PLUGIN_ID,
        "host": host_json(),
        "connection": connection_json(),
    }))
    .unwrap()
}

fn service_context(operation: &str, interface: &str, payload: Vec<u8>) -> NativeServiceContext {
    let request = ServiceRequest {
        caller_plugin_id: "svc.writer".into(),
        service: RegisteredService {
            capability: HostScope::new("bmux.tab_bar.status.write").unwrap(),
            kind: ServiceKind::Command,
            interface_id: interface.into(),
            provider: ProviderId::Plugin(PLUGIN_ID.into()),
        },
        operation: operation.into(),
        payload,
    };
    serde_json::from_value(serde_json::json!({
        "plugin_id": PLUGIN_ID,
        "request": request,
        "host": host_json(),
        "connection": connection_json(),
    }))
    .unwrap()
}

async fn settle(execution: &LoadedExecution, id: u64) -> NativeOutcome {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Poll::Ready(outcome) = execution
                .poll(id, &mut Context::from_waker(Waker::noop()))
                .unwrap()
            {
                break outcome;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("operation settles within the fixture budget")
}

/// Create and activate one instance with the host's event bus binder.
async fn instance(
    artifact: &DefinitionArtifact,
    host: PluginRuntimeScopeId,
    bus: &Arc<EventBus>,
) -> Arc<LoadedExecution> {
    // The capabilities the plugin's manifest requires from its host.
    let available = ["bmux.tabs.read", "bmux.tabs.write"]
        .into_iter()
        .chain(["bmux.workspaces.read", "bmux.workspaces.write"])
        .map(String::from)
        .collect::<Vec<_>>();
    let execution = artifact
        .create(host, ComponentRole::Backend, &available)
        .unwrap();
    assert!(execution.is_in_process());
    execution
        .bind_callback_runtime(tokio::runtime::Handle::current())
        .unwrap();
    execution
        .bind_activation_binder(bmux_plugin::event_bus_activation_binder(Arc::clone(bus)))
        .unwrap();
    let id = execution
        .begin_activation(lifecycle_context(), PluginConcurrencyConfig::Concurrent)
        .unwrap();
    let outcome = settle(&execution, id).await;
    assert!(matches!(outcome, NativeOutcome::Unit), "{outcome:?}");
    execution.acknowledge(id).await.unwrap();
    execution
}

async fn invoke<T: serde::de::DeserializeOwned>(
    execution: &Arc<LoadedExecution>,
    interface: &str,
    operation: &str,
    request: &impl serde::Serialize,
) -> T {
    let payload = bmux_plugin_sdk::encode_service_message(request).unwrap();
    // The host authenticates each call as a root callback on a fresh
    // invocation of this instance's activation.
    let binding = execution
        .bind_root_callback(
            execution.begin_invocation_scope().unwrap(),
            bmux_plugin_sdk::ServiceCancellation::new(),
        )
        .unwrap();
    let id = execution
        .begin_with_callback_dispatch(
            NativeOperation::Invoke {
                invocation_id: PluginInvocationId::new(),
                context: service_context(operation, interface, payload),
            },
            binding,
            None,
            None,
        )
        .unwrap();
    let response = match settle(execution, id).await {
        NativeOutcome::Service(response) => response,
        other => panic!("unexpected outcome {other:?}"),
    };
    execution.acknowledge(id).await.unwrap();
    assert!(response.error.is_none(), "{:?}", response.error);
    bmux_plugin_sdk::decode_service_message(&response.payload).unwrap()
}

async fn set_item(execution: &Arc<LoadedExecution>, id: &str) {
    let result: Result<(), bmux_tab_bar_plugin_api::status_items::StatusItemError> = invoke(
        execution,
        bmux_tab_bar_plugin_api::status_items::INTERFACE_ID.as_str(),
        "set-status-item",
        &SetStatusItemRequest {
            id: id.into(),
            text: id.into(),
            severity: Severity::Normal,
            priority: 0,
            tooltip: None,
        },
    )
    .await;
    result.unwrap();
}

async fn listed(execution: &Arc<LoadedExecution>) -> Vec<String> {
    let items: Vec<StatusItem> = invoke(
        execution,
        bmux_tab_bar_plugin_api::status_items_state::INTERFACE_ID.as_str(),
        "list-status-items",
        &bmux_tab_bar_plugin_api::status_items_state::client::ListStatusItemsRequest {
            owner: None,
        },
    )
    .await;
    items.into_iter().map(|item| item.id).collect()
}

fn published(bus: &EventBus) -> Vec<String> {
    let (snapshot, _) = bus
        .subscribe_state::<StatusItemSnapshot>(&status_items_list::STATE_KIND)
        .expect("an activation registered the status-items channel on the host bus");
    snapshot.items.iter().map(|item| item.id.clone()).collect()
}

async fn retire(execution: &LoadedExecution) {
    let id = execution
        .begin_deactivation(lifecycle_context())
        .await
        .unwrap();
    assert!(matches!(settle(execution, id).await, NativeOutcome::Unit));
    execution.acknowledge(id).await.unwrap();
    execution.destroy().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_instances_in_one_host_share_no_state() {
    let artifact = DefinitionArtifact::from_definition(bmux_tab_bar_plugin::plugin_definition());
    let host = PluginRuntimeScopeId::new();
    let bus = Arc::new(EventBus::new());
    let first = instance(&artifact, host, &bus).await;
    let second = instance(&artifact, host, &bus).await;
    assert_ne!(
        first.create_context().instance,
        second.create_context().instance
    );

    // Each instance keeps its own store; the host bus carries each
    // instance's own snapshot, never a merged one.
    set_item(&first, "first-only").await;
    assert_eq!(listed(&first).await, ["first-only"]);
    assert!(listed(&second).await.is_empty());
    assert_eq!(published(&bus), ["first-only"]);

    set_item(&second, "second-only").await;
    assert_eq!(listed(&second).await, ["second-only"]);
    assert_eq!(listed(&first).await, ["first-only"]);
    assert_eq!(published(&bus), ["second-only"]);

    // Retiring the first instance closes only its bus view: the second
    // instance's registration survives and keeps publishing.
    retire(&first).await;
    set_item(&second, "after-first-retired").await;
    assert_eq!(published(&bus), ["after-first-retired", "second-only"]);
    retire(&second).await;
    assert!(
        bus.subscribe_state::<StatusItemSnapshot>(&status_items_list::STATE_KIND)
            .is_err(),
        "the last owner's channel is released with its activation"
    );
}
