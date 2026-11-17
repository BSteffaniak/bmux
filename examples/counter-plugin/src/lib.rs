//! Reference provider for the generated plugin author API.
//!
//! [`Counter`] implements the BPDL-generated provider traits with plain
//! `async fn`. The [`PluginInstance`] glue below is what the Wave 2
//! `Plugin`/`Declarations`/`plugin!` layer generates; it routes only through
//! `declaration::provides`, so serving an interface the world does not
//! declare does not compile. See `docs/plugin-author-api.md`.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

/// Manifest generated from the counter package's world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_example_counter_plugin_api::export_schema,
            crate_path: ::bmux_example_counter_plugin_api,
        },
        settings: "plugin.toml",
    }
}

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bmux_example_counter_plugin_api::commands::BUMP;
use bmux_plugin_sdk::author::{self, Call, EventDelivery, EventEmitter};
use bmux_plugin_sdk::definition::{
    CommandOutcome, PluginCallContext, PluginCreateContext, PluginFuture, PluginInstance,
    PluginResult,
};
use bmux_plugin_sdk::{
    EXIT_OK, NativeCommandContext, NativeServiceContext, PluginCommandError, PluginEventKind,
    ServiceResponse, TypedServiceRegistry,
};
use declaration::provides::{counter_commands, counter_events, counter_state};

/// The counter service.
pub struct Counter {
    value: AtomicU64,
    events: Arc<dyn EventEmitter>,
}

impl Default for Counter {
    fn default() -> Self {
        Self::with_events(Arc::new(Unsubscribed))
    }
}

impl Counter {
    /// A counter publishing changes through `events`.
    #[must_use]
    pub fn with_events(events: Arc<dyn EventEmitter>) -> Self {
        Self {
            value: AtomicU64::new(0),
            events,
        }
    }

    /// Serve one host invocation of a provided interface.
    async fn invoke(
        &self,
        call: Call,
        context: NativeServiceContext,
    ) -> author::Result<ServiceResponse> {
        let call = call.with_events(self.events.clone());
        let interface = context.request.service.interface_id.clone();
        if interface == counter_commands::INTERFACE_ID.as_str() {
            counter_commands::v1::dispatch(self, call, context).await
        } else if interface == counter_state::INTERFACE_ID.as_str() {
            counter_state::v1::dispatch(self, call, context).await
        } else {
            Ok(ServiceResponse::error(
                "unsupported_service_interface",
                format!("example.counter does not provide '{interface}'"),
            ))
        }
    }
}

impl counter_commands::v1::CounterCommands for Counter {
    async fn incr(&self, cx: &Call, by: u64) -> author::Result<u64> {
        let value = self
            .value
            .fetch_add(by, Ordering::SeqCst)
            .checked_add(by)
            .ok_or_else(|| PluginCommandError::invalid_arguments("counter overflow"))?;
        cx.emit(counter_events::v1::Changed { value })?;
        Ok(value)
    }
}

impl counter_state::v1::CounterState for Counter {
    async fn get(&self, _cx: &Call) -> author::Result<u64> {
        Ok(self.value.load(Ordering::SeqCst))
    }
}

/// Drops changes until the host event bus is attached (Wave 2 wires it
/// into every `Call`).
struct Unsubscribed;

impl EventEmitter for Unsubscribed {
    fn emit(&self, _: &PluginEventKind, _: EventDelivery, _: Vec<u8>) -> author::Result<()> {
        Ok(())
    }
}

/// Scoped plugin instance: a fresh [`Counter`] per host scope.
#[derive(Default)]
pub struct CounterPlugin(Arc<Counter>);

impl PluginInstance for CounterPlugin {
    fn invoke(
        &self,
        call: PluginCallContext,
        context: NativeServiceContext,
    ) -> PluginFuture<'_, PluginResult<ServiceResponse>> {
        Box::pin(async move {
            let call = Call::from_service(call, &context);
            self.0.invoke(call, context).await
        })
    }

    fn register_typed_services(&self, registry: &mut TypedServiceRegistry) -> PluginResult<()> {
        let failed =
            |error: bmux_plugin_sdk::PluginError| PluginCommandError::failed(error.to_string());
        // Direct bundled calls carry no host invocation; they run with the
        // counter's own event channel bound.
        let base = Call::detached().with_events(self.0.events.clone());
        counter_commands::v1::register(self.0.clone(), base.clone(), registry).map_err(failed)?;
        counter_state::v1::register(self.0.clone(), base, registry).map_err(failed)
    }

    fn command(
        &self,
        call: PluginCallContext,
        context: NativeCommandContext,
    ) -> PluginFuture<'_, CommandOutcome> {
        Box::pin(async move {
            if context.command != BUMP.name() {
                return Err(PluginCommandError::unknown_command(&context.command)).into();
            }
            // Commands run in the issuing CLI; reach the server instance
            // through the host like any other consumer.
            let result = async {
                let args = BUMP.parse(&context.arguments)?;
                let host = call
                    .host_services
                    .clone()
                    .ok_or_else(|| PluginCommandError::unavailable("bump needs host services"))?;
                let value = counter_commands::v1::Client::remote(host)
                    .incr(args.by.unwrap_or(1))
                    .await?;
                println!("counter: {value}");
                Ok(EXIT_OK)
            };
            let result: PluginResult<i32> = result.await;
            if let Err(error) = &result {
                eprintln!("bump failed: {}", error.message);
            }
            result.into()
        })
    }
}

fn create(_context: PluginCreateContext) -> PluginResult<Arc<dyn PluginInstance>> {
    Ok(Arc::new(CounterPlugin::default()))
}

bmux_plugin_sdk::define_plugin!(
    factory = create,
    metadata = author::metadata(declaration::PLUGIN_MANIFEST, declaration::SERVICE_CONTRACTS)
        .expect("generated manifest declares identity and roles"),
);

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_example_counter_plugin_api::extension_points::counter_display_v1 as display;
    use bmux_plugin_sdk::author::{Extends, ExtensionPoint};
    use counter_commands::v1::CounterCommands;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, EventDelivery, Vec<u8>)>>);

    impl EventEmitter for Recorder {
        fn emit(
            &self,
            kind: &PluginEventKind,
            delivery: EventDelivery,
            payload: Vec<u8>,
        ) -> author::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push((kind.as_str().to_owned(), delivery, payload));
            Ok(())
        }
    }

    #[tokio::test]
    async fn increments_publish_retained_changes_on_the_generated_channel() {
        let recorder = Arc::new(Recorder::default());
        let counter = Counter::with_events(recorder.clone());
        let call = Call::detached().with_events(recorder.clone());
        assert_eq!(counter.incr(&call, 4).await.unwrap(), 4);
        let events = recorder.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, counter_events::STATE_KIND.as_str());
        assert_eq!(events[0].1, EventDelivery::State);
        let changed: counter_events::v1::Changed =
            bmux_plugin_sdk::decode_service_message(&events[0].2).unwrap();
        assert_eq!(changed.value, 4);
    }

    struct Plain;

    impl display::Behavior for Plain {
        async fn render(&self, _cx: &Call, value: u64) -> author::Result<String> {
            Ok(format!("count={value}"))
        }
    }

    #[tokio::test]
    async fn extension_point_behavior_round_trips_through_its_wire_adapter() {
        assert_eq!(display::Point::ID, "example.counter.counter-display/v1");
        let data: <display::Point as ExtensionPoint>::Data = display::Data {
            label: "plain".into(),
            order: 1,
        };
        assert_eq!(data.order, 1);
        let request =
            bmux_plugin_sdk::encode_service_message(&display::Request::Render { value: 9 })
                .unwrap();
        let response =
            <display::Point as Extends<Plain>>::dispatch(&Plain, &Call::detached(), &request)
                .await
                .unwrap();
        let rendered: String = bmux_plugin_sdk::decode_service_message(&response).unwrap();
        assert_eq!(rendered, "count=9");
    }

    #[test]
    fn definition_embeds_the_generated_manifest() {
        let definition = plugin_definition();
        definition
            .validate()
            .expect("embedded manifest matches metadata");
        let declarations = definition.declarations().unwrap();
        let interfaces = declarations
            .services
            .iter()
            .map(|service| service.interface_id.as_str())
            .collect::<Vec<_>>();
        assert!(
            interfaces.contains(&"counter-commands/v1"),
            "{interfaces:?}"
        );
        assert!(interfaces.contains(&"counter-state/v1"), "{interfaces:?}");
        let manifest = definition.metadata.manifest_toml.as_deref().unwrap();
        assert!(manifest.contains("name = \"bump\""));
        let args = BUMP.parse(&["--by".to_owned(), "3".to_owned()]).unwrap();
        assert_eq!(args.by, Some(3));
        assert!(BUMP.parse(&["--nope".to_owned(), "1".to_owned()]).is_err());
    }
}
