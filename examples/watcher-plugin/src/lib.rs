//! Reference consumer for the generated plugin author API.
//!
//! The watcher reaches the counter only through `declaration::requires`,
//! which re-exports exactly the interfaces its BPDL world requires: a call to
//! an undeclared dependency does not compile. Its `watch` command uses the
//! generated typed arguments and clients.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

/// Bindings and manifest generated from `bpdl/watcher.bpdl` and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/watcher.bpdl",
        manifest: "plugin.toml",
        imports: {
            counter: {
                schema: ::bmux_example_counter_plugin_api::export_schema,
                crate_path: ::bmux_example_counter_plugin_api,
            },
        },
    }
}

use std::sync::Arc;

use bmux_plugin_sdk::author;
use bmux_plugin_sdk::definition::{
    CommandOutcome, PluginCallContext, PluginCreateContext, PluginFuture, PluginInstance,
    PluginLifecycleContext, PluginResult,
};
use bmux_plugin_sdk::{EXIT_OK, NativeCommandContext, PluginCommandError, TypedDispatchClient};
use declaration::commands::WATCH;
use declaration::requires::{counter_commands, counter_state};

/// Bump the counter by `by` and read it back through the generated clients.
///
/// Works over any transport: a bundled direct client or the host's codec.
///
/// # Errors
/// Returns the counter's failure or a transport failure.
pub async fn watch<T: TypedDispatchClient + Clone + Sync>(
    commands: &counter_commands::v1::Client<T>,
    state: &counter_state::v1::Client<T>,
    by: u64,
) -> author::Result<(u64, u64)> {
    let bumped = commands.incr(by).await?;
    let read = state.get().await?;
    Ok((bumped, read))
}

/// Scoped plugin instance.
#[derive(Default)]
pub struct WatcherPlugin;

impl PluginInstance for WatcherPlugin {
    /// Exercise the counter once at activation; lifecycle diagnostics go to
    /// stderr (in a command-local activation, stdout is command output).
    fn activate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            let Some(host) = context.native.host_service_port.clone() else {
                eprintln!("watcher: activated without host services");
                return Ok(());
            };
            // The host's services are a typed dispatch transport in both
            // packaging modes; inside a dylib, calls run as host tasks
            // awaited with real wakers.
            let commands = counter_commands::v1::Client::remote(host.clone());
            let state = counter_state::v1::Client::remote(host);
            for by in [2, 3] {
                match watch(&commands, &state, by).await {
                    Ok((bumped, read)) => {
                        eprintln!("watcher: activation bumped to {bumped}, read back {read}");
                    }
                    Err(error) => eprintln!("watcher: activation call failed: {}", error.message),
                }
            }
            Ok(())
        })
    }

    fn command(
        &self,
        call: PluginCallContext,
        context: NativeCommandContext,
    ) -> PluginFuture<'_, CommandOutcome> {
        Box::pin(async move {
            if context.command != WATCH.name() {
                return Err(PluginCommandError::unknown_command(&context.command)).into();
            }
            let result = async {
                let args = WATCH.parse(&context.arguments)?;
                let host = call
                    .host_services
                    .clone()
                    .ok_or_else(|| PluginCommandError::unavailable("watch needs host services"))?;
                let (bumped, read) = watch(
                    &counter_commands::v1::Client::remote(host.clone()),
                    &counter_state::v1::Client::remote(host),
                    args.by.unwrap_or(1),
                )
                .await?;
                println!("watcher: bumped to {bumped}, read back {read}");
                Ok(EXIT_OK)
            };
            let result: PluginResult<i32> = result.await;
            if let Err(error) = &result {
                eprintln!("watch failed: {}", error.message);
            }
            result.into()
        })
    }
}

fn create(_context: PluginCreateContext) -> PluginResult<Arc<dyn PluginInstance>> {
    Ok(Arc::new(WatcherPlugin))
}

bmux_plugin_sdk::define_plugin!(
    factory = create,
    metadata = author::metadata(declaration::PLUGIN_MANIFEST, declaration::SERVICE_CONTRACTS)
        .expect("generated manifest declares identity and roles"),
);
