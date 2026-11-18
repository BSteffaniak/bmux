//! Reference provider for the plugin author API.
//!
//! [`Counter`] implements the BPDL-generated provider traits with plain
//! `async fn` and declares them in [`Plugin::declare`]; `plugin!` turns it
//! into a plugin definition (and, when packaged, the dynamic-library entry).
//! See `docs/plugin-author-api.md`.
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

use std::sync::atomic::{AtomicU64, Ordering};

use bmux_example_counter_plugin_api::commands::{BUMP, BumpArgs};
use bmux_plugin_sdk::PluginCommandError;
use bmux_plugin_sdk::author::{self, Call, Declarations, Plugin};
use declaration::provides::{counter_commands, counter_events, counter_state};

/// The counter service.
#[derive(Default)]
pub struct Counter {
    value: AtomicU64,
}

impl counter_commands::v1::CounterCommands for Counter {
    async fn incr(&self, cx: &Call, by: u64) -> author::Result<u64> {
        // A failed increment leaves the counter unchanged.
        let value = self
            .value
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_add(by)
            })
            .map_err(|_| PluginCommandError::invalid_arguments("counter overflow"))?
            + by;
        cx.emit(counter_events::v1::Changed { value })?;
        Ok(value)
    }
}

impl counter_state::v1::CounterState for Counter {
    async fn get(&self, _cx: &Call) -> author::Result<u64> {
        Ok(self.value.load(Ordering::SeqCst))
    }
}

/// `bump`: commands run in the issuing CLI, so reach the server instance
/// through the host like any other consumer.
async fn bump(cx: &Call, args: BumpArgs) -> author::Result<()> {
    let host = cx
        .host()
        .cloned()
        .ok_or_else(|| PluginCommandError::unavailable("bump needs host services"))?;
    let value = counter_commands::v1::Client::remote(host)
        .incr(args.by.unwrap_or(1))
        .await?;
    println!("counter: {value}");
    Ok(())
}

impl Plugin for Counter {
    fn declare(d: &mut Declarations<Self>) {
        d.provide::<dyn counter_commands::v1::CounterCommands>()
            .provide::<dyn counter_state::v1::CounterState>()
            .emits::<counter_events::v1::Changed>()
            .command(BUMP, bump);
    }
}

bmux_plugin_sdk::plugin!(Counter);

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_example_counter_plugin_api::extension_points::counter_display_v1 as display;
    use bmux_plugin_sdk::PluginEventKind;
    use bmux_plugin_sdk::author::{
        DeclaredInstance, EventDelivery, EventEmitter, Extends, ExtensionPoint,
    };
    use counter_commands::v1::CounterCommands;
    use std::sync::{Arc, Mutex};

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
        let call = Call::detached().with_events(recorder.clone());
        assert_eq!(Counter::default().incr(&call, 4).await.unwrap(), 4);
        let (kind, delivery, payload) = recorder.0.lock().unwrap().remove(0);
        assert_eq!(kind, counter_events::STATE_KIND.as_str());
        assert_eq!(delivery, EventDelivery::State);
        let changed: counter_events::v1::Changed = serde_json::from_slice(&payload).unwrap();
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
    fn cli_bump_does_not_construct_the_counter() {
        use bmux_plugin_sdk::definition::{
            ComponentRole, LoadedInstanceId, PluginCreateContext, RuntimeScopeId,
        };

        let definition = author::definition::<Counter>(
            declaration::PLUGIN_MANIFEST,
            declaration::SERVICE_CONTRACTS,
            |_| Err(PluginCommandError::failed("provider factory called")),
        )
        .unwrap();
        let mut scope = PluginCreateContext {
            cli_command: Some("bump".to_owned()),
            runtime_scope: RuntimeScopeId::new(),
            instance: LoadedInstanceId::from_uuid(RuntimeScopeId::new().as_uuid()),
            role: ComponentRole::Backend,
        };
        assert!(definition.create(scope.clone()).is_ok());
        scope.cli_command = None;
        assert_eq!(
            definition
                .create(scope)
                .err()
                .expect("provider uses the factory")
                .message,
            "provider factory called"
        );
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
        assert_eq!(declarations.modeled_service_contracts.len(), 2);
        let manifest = definition.metadata.manifest_toml.as_deref().unwrap();
        assert!(manifest.contains("name = \"bump\""));
        let args = BUMP.parse(&["--by".to_owned(), "3".to_owned()]).unwrap();
        assert_eq!(args.by, Some(3));
        assert!(BUMP.parse(&["--nope".to_owned(), "1".to_owned()]).is_err());
    }

    /// Serves only one of the two interfaces its world provides.
    #[derive(Default)]
    struct HalfCounter(Counter);

    impl counter_state::v1::CounterState for HalfCounter {
        async fn get(&self, cx: &Call) -> author::Result<u64> {
            counter_state::v1::CounterState::get(&self.0, cx).await
        }
    }

    impl Plugin for HalfCounter {
        fn declare(d: &mut Declarations<Self>) {
            d.provide::<dyn counter_state::v1::CounterState>();
        }
    }

    #[test]
    fn declarations_must_match_the_world() {
        let error = DeclaredInstance::new(
            HalfCounter::default(),
            declaration::PLUGIN_MANIFEST,
            declaration::SERVICE_CONTRACTS,
        )
        .err()
        .expect("missing provide is rejected");
        assert!(
            error.message.contains("counter-commands/v1"),
            "{}",
            error.message
        );
        assert!(
            DeclaredInstance::new(
                Counter::default(),
                declaration::PLUGIN_MANIFEST,
                declaration::SERVICE_CONTRACTS,
            )
            .is_ok()
        );
    }
}
