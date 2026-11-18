//! Reference consumer for the plugin author API.
//!
//! The watcher reaches the counter only through `declaration::requires`,
//! which re-exports exactly the interfaces its BPDL world requires, and
//! declares those requirements in [`Plugin::declare`]: a call to an
//! undeclared dependency does not compile, and a `require` the world does not
//! state is rejected when the definition is built.
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
use std::sync::atomic::{AtomicU64, Ordering};

use bmux_plugin_sdk::author::{self, Activation, Call, Declarations, Plugin};
use bmux_plugin_sdk::{PluginCommandError, PluginHostServices, TypedDispatchClient};
use declaration::commands::{WATCH, WatchArgs};
use declaration::provides::watcher_state;
use declaration::requires::{counter_commands, counter_events, counter_state};

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

/// The generated clients over the host's services: a typed dispatch
/// transport in both packaging modes; inside a dylib, calls run as host
/// tasks awaited with real wakers.
fn clients(
    host: &PluginHostServices,
) -> (
    counter_commands::v1::Client<PluginHostServices>,
    counter_state::v1::Client<PluginHostServices>,
) {
    (
        counter_commands::v1::Client::remote(host.clone()),
        counter_state::v1::Client::remote(host.clone()),
    )
}

/// Subscribe to the counter's retained changes and record each one from a
/// task the activation owns: the host cancels it and releases the
/// subscription when the activation ends, in both packaging modes.
async fn observe_changes(cx: &Activation, observed: Arc<AtomicU64>) -> author::Result<()> {
    let mut changes = cx
        .events::<counter_events::v1::Client>()?
        .subscribe_changed()
        .await?;
    cx.spawn("observe-changes", async move {
        while let Some(change) = changes.next().await {
            match change {
                Ok(change) => {
                    observed.store(change.value, Ordering::SeqCst);
                    eprintln!("watcher: observed {}", change.value);
                }
                Err(error) => eprintln!("watcher: unreadable change: {}", error.message),
            }
        }
    })
}

/// The watcher plugin.
#[derive(Default)]
pub struct Watcher {
    observed: Arc<AtomicU64>,
}

impl watcher_state::v1::WatcherState for Watcher {
    async fn observed(&self, _cx: &Call) -> author::Result<u64> {
        Ok(self.observed.load(Ordering::SeqCst))
    }
}

async fn watch_command(cx: &Call, args: WatchArgs) -> author::Result<()> {
    let host = cx
        .host()
        .ok_or_else(|| PluginCommandError::unavailable("watch needs host services"))?;
    let (commands, state) = clients(host);
    let (bumped, read) = watch(&commands, &state, args.by.unwrap_or(1)).await?;
    println!("watcher: bumped to {bumped}, read back {read}");
    Ok(())
}

impl Plugin for Watcher {
    fn declare(d: &mut Declarations<Self>) {
        d.provide::<dyn watcher_state::v1::WatcherState>()
            .require::<counter_commands::v1::Client>()
            .require::<counter_state::v1::Client>()
            .subscribe::<counter_events::v1::Client>()
            .command(WATCH, watch_command);
    }

    /// Follow the counter's changes for the activation's lifetime, then
    /// exercise it once on the provider.
    async fn activate(&self, cx: &mut Activation) -> author::Result<()> {
        if let Err(error) = observe_changes(cx, self.observed.clone()).await {
            // The activation host may not provide an event bus.
            eprintln!("watcher: not observing changes: {}", error.message);
        }
        let (commands, state) = clients(&cx.host()?);
        for by in [2, 3] {
            match watch(&commands, &state, by).await {
                Ok((bumped, read)) => {
                    eprintln!("watcher: activation bumped to {bumped}, read back {read}");
                }
                Err(error) => eprintln!("watcher: activation call failed: {}", error.message),
            }
        }
        Ok(())
    }
}

bmux_plugin_sdk::plugin!(Watcher);
