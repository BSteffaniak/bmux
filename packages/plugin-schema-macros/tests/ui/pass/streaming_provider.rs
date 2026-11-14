bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin example version 1;
capability WRITE = example.write;
@capability(WRITE)
@interface-version(2)
interface stream {
    command run(value: u32) -> u32 emits u32;
    command empty() -> unit emits u32;
}
"#
);

struct Provider;
impl stream::StreamService for Provider {
    fn run<'a>(
        &'a self,
        value: u32,
        events: bmux_plugin_sdk::typed_dispatch::TypedServiceEventSink<u32>,
        cancellation: bmux_plugin_sdk::ServiceCancellation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = u32> + Send + 'a>> {
        Box::pin(async move {
            if !cancellation.is_cancelled() {
                events.emit(value).expect("event accepted");
            }
            value + 1
        })
    }

    fn empty<'a>(
        &'a self,
        _events: bmux_plugin_sdk::typed_dispatch::TypedServiceEventSink<u32>,
        _cancellation: bmux_plugin_sdk::ServiceCancellation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

fn main() {
    use bmux_plugin_sdk::typed_dispatch::PortableServiceProvider;
    let declarations = Contract::service_contract_descriptors();
    assert_eq!(declarations.len(), 1);
    assert_eq!(declarations[0].interface_id, "stream/v2");
    assert_eq!(declarations[0].capability, "example.write");
    assert_eq!(declarations[0].contract, stream::SERVICE_CONTRACT);
    declarations[0].validate().unwrap();
    assert_eq!(
        <Contract as bmux_plugin_sdk::PluginContract>::service_contract_descriptors(),
        declarations,
    );
    let provider = std::sync::Arc::new(Provider);
    let mut registry = bmux_plugin_sdk::TypedServiceRegistry::new();
    stream::register_provider(&mut registry, provider.clone()).unwrap();
    let capability = bmux_plugin_sdk::HostScope::new("example.write").unwrap();
    let handle = registry
        .get(
            &capability,
            bmux_plugin_sdk::ServiceKind::Command,
            "stream/v2",
        )
        .unwrap();
    assert_eq!(
        registry.service_contract_for("stream/v2"),
        Some(stream::SERVICE_CONTRACT)
    );
    let direct = stream::StreamClient::new(provider.clone());
    let portable = stream::StreamProviderAdapter::new(provider);
    assert_eq!(portable.descriptor().contract, stream::SERVICE_CONTRACT);
    let sink = bmux_plugin_sdk::typed_dispatch::TypedServiceEventSink::new(|_: u32| Ok(()));
    let cancellation = bmux_plugin_sdk::ServiceCancellation::new();
    drop(direct.run(1, sink.clone(), cancellation.clone()));
    drop(direct.empty(sink, cancellation));
    let request =
        bmux_plugin_sdk::encode_service_message(&stream::client::RunRequest { value: 1 }).unwrap();
    let checked =
        bmux_plugin_sdk::service_contract::encode("stream/v2", stream::SERVICE_CONTRACT, &request)
            .unwrap();
    assert!(portable.supports_streaming("run"));
    let stream_context = || bmux_plugin_sdk::ServiceStreamContext {
        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        events: bmux_plugin_sdk::ServiceEventSinkHandle::new(
            bmux_plugin_sdk::BoundedServiceEventSink::new(2),
        ),
    };
    drop(stream::client::run_dispatch(
        bmux_plugin_sdk::typed_dispatch::ServiceDispatchTarget::Static(handle),
        stream::client::RunRequest { value: 1 },
        stream_context(),
    ));
    drop(stream::client::empty_dispatch(
        bmux_plugin_sdk::typed_dispatch::ServiceDispatchTarget::Portable(&portable),
        (),
        stream_context(),
    ));
    let request = bmux_plugin_sdk::ServiceRequest {
        caller_plugin_id: "consumer".to_string(),
        service: bmux_plugin_sdk::RegisteredService {
            capability,
            kind: bmux_plugin_sdk::ServiceKind::Command,
            interface_id: "stream/v2".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Plugin("example".to_string()),
        },
        operation: "run".to_string(),
        payload: checked.clone(),
    };
    drop(registry.dispatch_checked(&request, Some(stream_context())));
    drop(portable.dispatch_checked("run", &checked, None));
}
