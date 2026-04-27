//! bmux performance plugin — owns `PerformanceCaptureSettings` and
//! serves typed performance settings queries/mutations.
//!
//! The plugin implements `performance-commands::dispatch(PerformanceRequest)
//! -> PerformanceResponse` for the `bmux_performance_plugin_api`
//! surface. Server constructs the settings handle at `BmuxServer::new`
//! time (seeded from config) and registers it as a
//! `PerformanceSettingsHandle`; this plugin's handlers read/write that
//! handle and emit `PerformanceEvent::SettingsUpdated` on the plugin
//! event bus when settings change.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_performance_plugin_api::{
    EVENT_KIND, METRIC_EVENT_KIND, METRICS_STATE_KIND, MetricEvent, MetricTarget, MetricWatch,
    MetricsSnapshot, PERFORMANCE_COMMANDS_INTERFACE, PERFORMANCE_READ, PERFORMANCE_WRITE,
    PaneMetricsSnapshot, PerformanceEvent, PerformanceRequest, PerformanceResponse,
    ProcessMetricsSnapshot, SystemMetricsSnapshot,
};
use bmux_performance_state::{PerformanceCaptureSettings, PerformanceSettingsHandle};
use bmux_plugin::{global_event_bus, global_plugin_state_registry};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry, WireEventSinkHandle};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::System;

const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct MetricsState {
    watches: BTreeMap<String, MetricWatch>,
    snapshot: MetricsSnapshot,
    worker_started: bool,
}

impl Default for MetricsState {
    fn default() -> Self {
        let watch = MetricWatch::default().normalized();
        let watches = BTreeMap::from([(watch.id.clone(), watch.clone())]);
        Self {
            watches,
            snapshot: MetricsSnapshot {
                watches: vec![watch],
                ..MetricsSnapshot::default()
            },
            worker_started: false,
        }
    }
}

static METRICS_STATE: OnceLock<Mutex<MetricsState>> = OnceLock::new();

fn metrics_state() -> &'static Mutex<MetricsState> {
    METRICS_STATE.get_or_init(|| Mutex::new(MetricsState::default()))
}

/// Look up the server-registered `WireEventSinkHandle` from the plugin
/// state registry and publish the given wire event through it. Silent
/// no-op when no server is attached (tests / headless tooling).
fn publish_wire_event(event: bmux_ipc::Event) {
    let Some(handle) = global_plugin_state_registry().get::<WireEventSinkHandle>() else {
        return;
    };
    let Ok(guard) = handle.read() else {
        return;
    };
    let _ = guard.0.publish(event);
}

#[derive(Default)]
pub struct PerformancePlugin;

impl RustPlugin for PerformancePlugin {
    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        global_event_bus().register_channel::<PerformanceEvent>(EVENT_KIND);
        global_event_bus().register_channel::<MetricEvent>(METRIC_EVENT_KIND);
        let initial_snapshot = metrics_state().lock().map_or_else(
            |_| MetricsSnapshot::default(),
            |state| state.snapshot.clone(),
        );
        global_event_bus()
            .register_state_channel::<MetricsSnapshot>(METRICS_STATE_KIND, initial_snapshot);
        ensure_metrics_worker();
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "performance-commands", "dispatch" => |req: PerformanceRequest, _ctx| {
                Ok::<PerformanceResponse, ServiceResponse>(handle_request(req))
            },
            "performance-commands", "list-watches" => |_req: (), _ctx| {
                Ok::<PerformanceResponse, ServiceResponse>(handle_request(PerformanceRequest::ListWatches))
            },
            "performance-commands", "start-watch" => |watch: MetricWatch, _ctx| {
                Ok::<PerformanceResponse, ServiceResponse>(handle_request(PerformanceRequest::StartWatch { watch }))
            },
            "performance-commands", "stop-watch" => |req: StopWatchRequest, _ctx| {
                Ok::<PerformanceResponse, ServiceResponse>(handle_request(PerformanceRequest::StopWatch { watch_id: req.watch_id }))
            },
            "performance-commands", "get-snapshot" => |_req: (), _ctx| {
                Ok::<PerformanceResponse, ServiceResponse>(handle_request(PerformanceRequest::GetSnapshot))
            },
        })
    }

    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        _registry: &mut TypedServiceRegistry,
    ) {
        // No typed Arc<dyn Trait> surface today — performance operations
        // dispatch exclusively through the byte-service path.
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StopWatchRequest {
    watch_id: String,
}

fn handle_request(req: PerformanceRequest) -> PerformanceResponse {
    match req {
        PerformanceRequest::GetSettings => handle_get_settings(),
        PerformanceRequest::SetSettings { settings } => handle_set_settings(&settings),
        PerformanceRequest::ListWatches => handle_list_watches(),
        PerformanceRequest::StartWatch { watch } => handle_start_watch(watch),
        PerformanceRequest::StopWatch { watch_id } => handle_stop_watch(&watch_id),
        PerformanceRequest::GetSnapshot => handle_get_snapshot(),
    }
}

fn handle_list_watches() -> PerformanceResponse {
    let watches = metrics_state().lock().map_or_else(
        |_| Vec::new(),
        |state| state.watches.values().cloned().collect(),
    );
    PerformanceResponse::Watches { watches }
}

fn handle_start_watch(watch: MetricWatch) -> PerformanceResponse {
    if watch.id.trim().is_empty() {
        return PerformanceResponse::Ack;
    }
    if let Ok(mut state) = metrics_state().lock() {
        let watch = watch.normalized();
        state.watches.insert(watch.id.clone(), watch);
        state.snapshot.watches = state.watches.values().cloned().collect();
    }
    ensure_metrics_worker();
    handle_list_watches()
}

fn handle_stop_watch(watch_id: &str) -> PerformanceResponse {
    if let Ok(mut state) = metrics_state().lock() {
        state.watches.remove(watch_id);
        state.snapshot.watches = state.watches.values().cloned().collect();
    }
    PerformanceResponse::Ack
}

fn handle_get_snapshot() -> PerformanceResponse {
    let snapshot = metrics_state().lock().map_or_else(
        |_| MetricsSnapshot::default(),
        |state| state.snapshot.clone(),
    );
    PerformanceResponse::Snapshot { snapshot }
}

fn handle_get_settings() -> PerformanceResponse {
    let Some(handle) = global_plugin_state_registry().get::<PerformanceSettingsHandle>() else {
        return PerformanceResponse::Settings {
            settings: PerformanceCaptureSettings::default().to_runtime_settings(),
        };
    };
    let Ok(guard) = handle.read() else {
        return PerformanceResponse::Settings {
            settings: PerformanceCaptureSettings::default().to_runtime_settings(),
        };
    };
    PerformanceResponse::Settings {
        settings: guard.0.current().to_runtime_settings(),
    }
}

fn handle_set_settings(requested: &bmux_ipc::PerformanceRuntimeSettings) -> PerformanceResponse {
    let normalized_capture = PerformanceCaptureSettings::from_runtime_settings(requested);
    let normalized = normalized_capture.to_runtime_settings();

    let Some(handle) = global_plugin_state_registry().get::<PerformanceSettingsHandle>() else {
        return PerformanceResponse::Settings {
            settings: normalized,
        };
    };
    let Ok(guard) = handle.read() else {
        return PerformanceResponse::Settings {
            settings: normalized,
        };
    };
    guard.0.set(normalized_capture);

    // Emit the typed event for plugin-local consumers, then publish
    // the wire-shape event directly through the registered
    // `WireEventSinkHandle` for cross-process subscribers.
    let _ = global_event_bus().emit(
        &EVENT_KIND,
        PerformanceEvent::SettingsUpdated {
            settings: normalized.clone(),
        },
    );
    publish_wire_event(bmux_ipc::Event::PerformanceSettingsUpdated {
        settings: normalized.clone(),
    });

    PerformanceResponse::Settings {
        settings: normalized,
    }
}

fn ensure_metrics_worker() {
    let should_start = metrics_state().lock().is_ok_and(|mut state| {
        if state.worker_started {
            false
        } else {
            state.worker_started = true;
            true
        }
    });
    if !should_start {
        return;
    }

    let _ = thread::Builder::new()
        .name("bmux-performance-metrics".to_string())
        .spawn(metrics_worker_loop);
}

fn metrics_worker_loop() {
    let mut system = System::new_all();
    loop {
        thread::sleep(DEFAULT_SAMPLE_INTERVAL);
        let watches = metrics_state().lock().map_or_else(
            |_| Vec::new(),
            |state| state.watches.values().cloned().collect::<Vec<_>>(),
        );
        if watches.is_empty() {
            continue;
        }

        let snapshot = sample_metrics(&mut system, watches);
        publish_metrics_snapshot(&snapshot);
    }
}

fn publish_metrics_snapshot(snapshot: &MetricsSnapshot) {
    if let Ok(mut state) = metrics_state().lock() {
        state.snapshot = snapshot.clone();
    }
    let _ = global_event_bus().publish_state(&METRICS_STATE_KIND, snapshot.clone());
    let _ = global_event_bus().emit(
        &METRIC_EVENT_KIND,
        MetricEvent::SnapshotUpdated {
            sampled_at_epoch_ms: snapshot.sampled_at_epoch_ms,
        },
    );
}

fn sample_metrics(system: &mut System, watches: Vec<MetricWatch>) -> MetricsSnapshot {
    system.refresh_all();

    let pane_identities = pane_process_identities();
    let mut process_roots = BTreeSet::new();
    for watch in &watches {
        if let MetricTarget::Process { pid } = watch.target {
            process_roots.insert(pid);
        }
    }
    for identity in &pane_identities {
        if let Some(pid) = identity.pid {
            process_roots.insert(pid);
        }
    }

    let process_tree = ProcessTree::from_system(system);
    let processes = process_roots
        .into_iter()
        .map(|pid| (pid, process_tree.snapshot_for_root(pid)))
        .collect::<BTreeMap<_, _>>();

    let panes = pane_identities
        .into_iter()
        .map(|identity| {
            let snapshot = identity
                .pid
                .and_then(|pid| processes.get(&pid))
                .map_or_else(
                    || PaneMetricsSnapshot {
                        pane_id: identity.pane_id,
                        session_id: Some(identity.session_id.0),
                        pid: identity.pid,
                        process_group_id: identity.process_group_id,
                        available: false,
                        ..PaneMetricsSnapshot::default()
                    },
                    |process| PaneMetricsSnapshot {
                        pane_id: identity.pane_id,
                        session_id: Some(identity.session_id.0),
                        pid: identity.pid,
                        process_group_id: identity.process_group_id,
                        cpu_percent: process.cpu_percent,
                        memory_bytes: process.memory_bytes,
                        process_count: process.process_count,
                        available: true,
                    },
                );
            (identity.pane_id, snapshot)
        })
        .collect();

    MetricsSnapshot {
        sampled_at_epoch_ms: epoch_millis_now(),
        watches,
        system: SystemMetricsSnapshot {
            cpu_percent: system.global_cpu_usage(),
            memory_used_bytes: system.used_memory(),
            memory_total_bytes: system.total_memory(),
        },
        processes,
        panes,
    }
}

fn pane_process_identities() -> Vec<bmux_pane_runtime_state::PaneProcessIdentity> {
    global_plugin_state_registry()
        .get::<bmux_pane_runtime_state::SessionRuntimeManagerHandle>()
        .and_then(|handle| {
            handle
                .read()
                .ok()
                .map(|guard| guard.0.list_pane_processes())
        })
        .unwrap_or_default()
}

#[allow(clippy::cast_possible_truncation)]
fn epoch_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct ProcessTree {
    processes: BTreeMap<u32, ProcessMetricsSnapshot>,
    children_by_parent: BTreeMap<u32, Vec<u32>>,
}

impl ProcessTree {
    fn from_system(system: &System) -> Self {
        let mut processes = BTreeMap::new();
        let mut children_by_parent: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (pid, process) in system.processes() {
            let pid = pid.as_u32();
            processes.insert(
                pid,
                ProcessMetricsSnapshot {
                    pid,
                    cpu_percent: process.cpu_usage(),
                    memory_bytes: process.memory(),
                    process_count: 1,
                },
            );
            if let Some(parent) = process.parent() {
                children_by_parent
                    .entry(parent.as_u32())
                    .or_default()
                    .push(pid);
            }
        }
        Self {
            processes,
            children_by_parent,
        }
    }

    fn snapshot_for_root(&self, root_pid: u32) -> ProcessMetricsSnapshot {
        let mut aggregate = ProcessMetricsSnapshot {
            pid: root_pid,
            ..ProcessMetricsSnapshot::default()
        };
        let mut stack = vec![root_pid];
        let mut seen = BTreeSet::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some(process) = self.processes.get(&pid) {
                aggregate.cpu_percent += process.cpu_percent;
                aggregate.memory_bytes =
                    aggregate.memory_bytes.saturating_add(process.memory_bytes);
                aggregate.process_count = aggregate.process_count.saturating_add(1);
            }
            if let Some(children) = self.children_by_parent.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        aggregate
    }
}

// Keep the capability/interface constants alive for consumers of the
// exported plugin binary (the symbols are referenced in the plugin's
// BPDL-free service registration via `plugin.toml`, but Rust doesn't
// see that wiring, so we touch them once in a const tuple).
const _KEEPS_CONSTS_ALIVE: (
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::InterfaceId,
) = (
    PERFORMANCE_READ,
    PERFORMANCE_WRITE,
    PERFORMANCE_COMMANDS_INTERFACE,
);

bmux_plugin_sdk::export_plugin!(PerformancePlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tree_aggregates_descendants() {
        let tree = ProcessTree {
            processes: BTreeMap::from([
                (
                    10,
                    ProcessMetricsSnapshot {
                        pid: 10,
                        cpu_percent: 5.0,
                        memory_bytes: 100,
                        process_count: 1,
                    },
                ),
                (
                    11,
                    ProcessMetricsSnapshot {
                        pid: 11,
                        cpu_percent: 7.0,
                        memory_bytes: 200,
                        process_count: 1,
                    },
                ),
                (
                    12,
                    ProcessMetricsSnapshot {
                        pid: 12,
                        cpu_percent: 9.0,
                        memory_bytes: 300,
                        process_count: 1,
                    },
                ),
            ]),
            children_by_parent: BTreeMap::from([(10, vec![11]), (11, vec![12])]),
        };

        let snapshot = tree.snapshot_for_root(10);
        assert_eq!(snapshot.pid, 10);
        assert!((snapshot.cpu_percent - 21.0).abs() < f32::EPSILON);
        assert_eq!(snapshot.memory_bytes, 600);
        assert_eq!(snapshot.process_count, 3);
    }

    #[test]
    fn start_watch_clamps_and_stores_watch() {
        let response = handle_start_watch(MetricWatch {
            id: "test-watch".to_string(),
            target: MetricTarget::System,
            metrics: Vec::new(),
            interval_ms: 1,
        });

        let PerformanceResponse::Watches { watches } = response else {
            panic!("expected watches response");
        };
        let watch = watches
            .into_iter()
            .find(|watch| watch.id == "test-watch")
            .expect("watch stored");
        assert_eq!(
            watch.interval_ms,
            bmux_performance_plugin_api::MIN_METRICS_INTERVAL_MS
        );

        let _ = handle_stop_watch("test-watch");
    }
}
