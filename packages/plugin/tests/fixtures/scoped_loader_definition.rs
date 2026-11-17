//! Shared implementation for loader tests and the explicit native fixture artifact.
use bmux_plugin_sdk::definition::{
    ComponentRole, PluginCallContext, PluginCreateContext, PluginDefinition, PluginFuture,
    PluginInstance, PluginLifecycleContext, PluginMetadata, PluginResult,
};
use bmux_plugin_sdk::rt;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

/// Channel the host owns and publishes on.
pub const HOST_EVENTS: &str = "acceptance.events/host";
/// Channel this plugin's activation registers and publishes on.
pub const PLUGIN_EVENTS: &str = "acceptance.events/plugin";

type JsonEvents = tokio::sync::broadcast::Receiver<Arc<bmux_plugin::JsonPluginEvent>>;

/// The activation's event view (in-process or remote, same API) and a
/// subscription kept across calls.
#[derive(Default)]
struct Events {
    bus: Option<Arc<bmux_plugin::EventBus>>,
    host: Option<JsonEvents>,
}

fn unavailable(error: impl std::fmt::Display) -> bmux_plugin_sdk::PluginCommandError {
    bmux_plugin_sdk::PluginCommandError::unavailable(error.to_string())
}

/// Records that a plugin future was dropped (completed, aborted or torn down).
struct DropGuard(Arc<AtomicBool>);
impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// plug-rt behaviors shared by the bundled and dynamic fixture builds. The
/// source is identical in both modes; only the packaging cfg differs.
async fn runtime_behavior(contract: &str) -> Option<Vec<u8>> {
    let reply = match contract {
        "rt-timer" => {
            let started = rt::time::Instant::now();
            rt::time::sleep(Duration::from_millis(30)).await;
            let timed_out =
                rt::time::timeout(Duration::from_millis(5), std::future::pending::<()>())
                    .await
                    .is_err();
            let mut ticks = rt::time::interval(Duration::from_millis(5));
            ticks.tick().await;
            ticks.tick().await;
            let waited = started.elapsed() >= Duration::from_millis(35);
            format!("{waited}:{timed_out}")
        }
        "rt-spawn" => {
            let producer = rt::spawn_named("producer", async {
                rt::time::sleep(Duration::from_millis(20)).await;
                41
            });
            let consumer =
                rt::spawn_named(
                    "consumer",
                    async move { producer.await.map(|value| value + 1) },
                );
            let spawned = consumer.await.ok().and_then(Result::ok).unwrap_or_default();
            let blocking = rt::spawn_blocking(|| 6 * 7).await.unwrap_or_default();
            format!("{spawned}:{blocking}")
        }
        "rt-abort" => {
            let dropped = Arc::new(AtomicBool::new(false));
            let guard = DropGuard(dropped.clone());
            let task = rt::spawn_named("aborted", async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            });
            rt::time::sleep(Duration::from_millis(5)).await;
            task.abort();
            let cancelled = task.await.err().is_some_and(|error| error.is_cancelled());
            format!("{cancelled}:{}", dropped.load(Ordering::SeqCst))
        }
        "rt-panic" => {
            let task = rt::spawn_named("panics", async { panic!("fixture task panic") });
            task.await
                .err()
                .is_some_and(|error| error.is_panic())
                .to_string()
        }
        _ => return None,
    };
    Some(reply.into_bytes())
}

struct Counter {
    scope: PluginCreateContext,
    mutations: AtomicUsize,
    orphan_dropped: Arc<AtomicBool>,
    events: Mutex<Events>,
}
impl Counter {
    fn bus(&self) -> PluginResult<Arc<bmux_plugin::EventBus>> {
        self.events
            .lock()
            .map_err(unavailable)?
            .bus
            .clone()
            .ok_or_else(|| unavailable("activation has no event bus"))
    }

    /// Event behaviors with identical source in both packaging modes.
    async fn event_behavior(&self, contract: &str) -> PluginResult<Option<Vec<u8>>> {
        let reply: &[u8] = match contract {
            "ev-register" => {
                self.bus()?
                    .register_json_channel(PLUGIN_EVENTS.into())
                    .map_err(unavailable)?;
                b"registered"
            }
            "ev-publish" => {
                let published = self
                    .bus()?
                    .emit_json(
                        &PLUGIN_EVENTS.into(),
                        &serde_json::json!({"from": "plugin"}),
                    )
                    .map_err(unavailable)?;
                if published { b"published" } else { b"dropped" }
            }
            "ev-subscribe" => {
                let receiver = self
                    .bus()?
                    .subscribe_json(&HOST_EVENTS.into())
                    .map_err(unavailable)?;
                self.events.lock().map_err(unavailable)?.host = Some(receiver);
                b"subscribed"
            }
            "ev-receive" => {
                let mut receiver = self
                    .events
                    .lock()
                    .map_err(unavailable)?
                    .host
                    .take()
                    .ok_or_else(|| unavailable("not subscribed"))?;
                let event = receiver.recv().await.map_err(unavailable)?;
                return Ok(Some(event.payload.to_string().into_bytes()));
            }
            _ => return Ok(None),
        };
        Ok(Some(reply.to_vec()))
    }
}
impl PluginInstance for Counter {
    fn activate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            // Optional: hosts that bind no bus leave events disabled.
            self.events.lock().map_err(unavailable)?.bus =
                bmux_plugin::take_activation_event_bus(&context.capabilities).ok();
            Ok(())
        })
    }
    fn deactivate(&self, _context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            // Release the view and its registrations with the activation.
            *self.events.lock().map_err(unavailable)? = Events::default();
            Ok(())
        })
    }
    fn invoke(
        &self,
        _call: PluginCallContext,
        context: bmux_plugin_sdk::NativeServiceContext,
    ) -> PluginFuture<'_, PluginResult<bmux_plugin_sdk::ServiceResponse>> {
        Box::pin(async move {
            bmux_plugin_sdk::service_contract::decode_for(
                "acceptance.counter/v1",
                &[7; 32],
                &context.request.payload,
            )
            .map_err(|error| bmux_plugin_sdk::PluginCommandError::unavailable(error.to_string()))?;
            self.mutations.fetch_add(1, Ordering::SeqCst);
            Ok(bmux_plugin_sdk::ServiceResponse::ok(b"committed".to_vec()))
        })
    }
    fn extension(
        &self,
        call: PluginCallContext,
        contract: String,
        _payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            if let Some(reply) = runtime_behavior(&contract).await {
                return Ok(reply);
            }
            if let Some(reply) = self.event_behavior(&contract).await? {
                return Ok(reply);
            }
            match contract.as_str() {
                // A raw task left running: teardown must abort it and drop
                // its future through the plugin.
                "rt-orphan" => {
                    let guard = DropGuard(self.orphan_dropped.clone());
                    drop(rt::spawn_named("orphan", async move {
                        let _guard = guard;
                        std::future::pending::<()>().await;
                    }));
                    Ok(b"spawned".to_vec())
                }
                "scope" => Ok(serde_json::to_vec(&self.scope).unwrap()),
                "mutate" => {
                    self.mutations.fetch_add(1, Ordering::SeqCst);
                    // Exceeds the loader's initial 4 KiB outcome buffer, below ABI bounds.
                    Ok(vec![42; 64 * 1024])
                }
                "count" => Ok(self
                    .mutations
                    .load(Ordering::SeqCst)
                    .to_string()
                    .into_bytes()),
                "pending" => {
                    std::future::poll_fn(move |_cx| {
                        if call.cancellation.is_cancelled() {
                            std::task::Poll::Ready(Ok(b"cancelled".to_vec()))
                        } else {
                            std::task::Poll::Pending
                        }
                    })
                    .await
                }
                _ => Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                    "unknown fixture extension",
                )),
            }
        })
    }
}
pub fn plugin_definition() -> PluginDefinition {
    PluginDefinition::new(
        PluginMetadata {
            plugin_id: "acceptance.scoped-loader".into(),
            version: "1.0.0".into(),
            roles: vec![ComponentRole::Backend, ComponentRole::Frontend],
            required_capabilities: vec!["acceptance.allowed".into()],
            contracts: vec![],
            manifest_toml: None,
        },
        |scope| {
            Ok(Arc::new(Counter {
                scope,
                mutations: AtomicUsize::new(0),
                orphan_dropped: Arc::new(AtomicBool::new(false)),
                events: Mutex::new(Events::default()),
            }) as Arc<dyn PluginInstance>)
        },
    )
}
