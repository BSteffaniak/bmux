//! bmux performance plugin — owns `PerformanceCaptureSettings` and
//! serves typed performance settings queries/mutations.
//!
//! The plugin implements BPDL-generated `performance-state` and
//! `performance-commands` operations for the
//! `bmux_performance_plugin_api` surface. Server constructs the settings handle at `BmuxServer::new`
//! time (seeded from config) and registers it as a
//! `PerformanceSettingsHandle`; this plugin's handlers read/write that
//! handle and emit `PerformanceEvent::SettingsUpdated` on the plugin
//! event bus when settings change.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_performance_plugin_api::{
    CpuPercentMode, EVENT_KIND, METRIC_EVENT_KIND, METRICS_STATE_KIND, MetricAccuracy,
    MetricCapability, MetricEvent, MetricTarget, MetricTargetKind, MetricWatch, MetricsSnapshot,
    PaneMetricsSnapshot, PerformanceEvent, ProcessMetricsSnapshot, SystemMetricsSnapshot,
    ThemeHeaderMetric, ThemeHeaderSettings, performance_types,
};
use bmux_performance_state::{
    PerformanceCaptureSettings, PerformanceRuntimeSettings, PerformanceSettingsHandle,
};
use bmux_plugin::{EventBus, global_plugin_state_registry};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_presentation_state::{
    PresentationEntityRef, PresentationFact, PresentationFactRole, PresentationFactSnapshot,
    global_presentation_fact_host_service,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::System;

const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct MetricsState {
    watches: BTreeMap<String, MetricWatch>,
    snapshot: MetricsSnapshot,
    theme_header_settings: ThemeHeaderSettings,
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
            theme_header_settings: ThemeHeaderSettings::default(),
        }
    }
}

/// Host mechanisms are explicitly supplied by the selected host adapter. Errors
/// are not converted to a healthy default installation. This trait belongs to
/// the implementation, never to the plugin API or shared ABI.
pub trait PerformanceHost: Send + Sync {
    /// # Errors
    /// Rejects unavailable or conflicting host channel registration.
    fn activate_channels(&self, initial: MetricsSnapshot) -> Result<(), String>;
    /// # Errors
    /// Rejects unavailable or corrupt selected settings.
    fn settings(&self) -> Result<PerformanceRuntimeSettings, String>;
    /// # Errors
    /// Rejects invalid settings or failure to commit the configuration.
    fn set_settings(
        &self,
        settings: &PerformanceRuntimeSettings,
    ) -> Result<PerformanceRuntimeSettings, String>;
    /// # Errors
    /// Reports unavailable selected pane-process observations.
    fn pane_processes(&self) -> Result<Vec<bmux_pane_runtime_state::PaneProcessIdentity>, String>;
    /// # Errors
    /// Reports rejected or unavailable retained metrics publication.
    fn publish_metrics(&self, snapshot: &MetricsSnapshot) -> Result<(), String>;
    /// # Errors
    /// Reports rejected or unavailable presentation-fact publication.
    fn publish_facts(&self, snapshot: PresentationFactSnapshot) -> Result<(), String>;
}

pub struct PerformancePlugin {
    host: Arc<dyn PerformanceHost>,
    metrics: Arc<Mutex<MetricsState>>,
    fact_revision: Arc<std::sync::atomic::AtomicU64>,
    worker_error: Arc<Mutex<Option<String>>>,
    worker: Arc<Mutex<Option<MetricsWorker>>>,
    cleanup_pending: Arc<std::sync::atomic::AtomicBool>,
    /// Set for the default composition: it receives the activation's bus.
    activation_host: Option<Arc<ActivationPerformanceHost>>,
}
struct MetricsWorker {
    stop: Arc<std::sync::atomic::AtomicBool>,
    task: std::thread::JoinHandle<()>,
}
impl Default for PerformancePlugin {
    fn default() -> Self {
        let host = Arc::new(ActivationPerformanceHost::default());
        let mut plugin = Self::with_host(host.clone());
        plugin.activation_host = Some(host);
        plugin
    }
}
impl PerformancePlugin {
    pub fn with_host(host: Arc<dyn PerformanceHost>) -> Self {
        Self {
            host,
            metrics: Arc::new(Mutex::new(MetricsState::default())),
            fact_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            worker_error: Arc::new(Mutex::new(None)),
            worker: Arc::new(Mutex::new(None)),
            cleanup_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            activation_host: None,
        }
    }

    fn adopt_activation_bus(&self, take: impl FnOnce() -> Option<Arc<EventBus>>) {
        if let Some(host) = &self.activation_host
            && let Ok(mut events) = host.events.write()
        {
            *events = take();
        }
    }
    fn activate_owner(&self) -> Result<(), PluginCommandError> {
        if self
            .cleanup_pending
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(PluginCommandError::unavailable(
                "metrics cleanup pending or failed; recovery required",
            ));
        }
        let snapshot = self
            .metrics
            .lock()
            .map_err(|_| PluginCommandError::failed("metrics lock poisoned"))?
            .snapshot
            .clone();
        self.host
            .activate_channels(snapshot)
            .map_err(PluginCommandError::failed)?;
        self.ensure_metrics_worker()
    }
    fn stop_worker(&self) -> Result<(), PluginCommandError> {
        if self
            .cleanup_pending
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(PluginCommandError::unavailable(
                "metrics cleanup pending or failed; recovery required",
            ));
        }
        stop_metrics_worker(&self.worker)?;
        self.cleanup_pending
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

fn stop_metrics_worker(slot: &Mutex<Option<MetricsWorker>>) -> Result<(), PluginCommandError> {
    let worker = slot
        .lock()
        .map_err(|_| PluginCommandError::failed("metrics worker lock poisoned"))?
        .take();
    if let Some(worker) = worker {
        worker
            .stop
            .store(true, std::sync::atomic::Ordering::Release);
        worker.task.thread().unpark();
        worker
            .task
            .join()
            .map_err(|_| PluginCommandError::failed("metrics worker panicked"))?;
    }
    Ok(())
}

impl RustPlugin for PerformancePlugin {
    type Contract = bmux_performance_plugin_api::Contract;

    fn activate(
        &mut self,
        context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        self.adopt_activation_bus(|| bmux_plugin::activation_event_bus(&context));
        self.activate_owner()?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn deactivate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        self.stop_worker()?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        self.service(&context)
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

impl PerformancePlugin {
    fn service(&self, context: &NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "performance-state", "get-settings" => |_req: (), _ctx| {
                self.handle_get_settings().map(performance_types::PerformanceRuntimeSettings::from)
            },
            "performance-commands", "set-settings" => |req: SetSettingsRequest, _ctx| {
                self.handle_set_settings(&req.settings.into()).map(performance_types::PerformanceRuntimeSettings::from)
            },
            "performance-state", "list-watches" => |_req: (), _ctx| {
                Ok::<Vec<performance_types::MetricWatch>, ServiceResponse>(self.handle_list_watches())
            },
            "performance-commands", "start-watch" => |req: StartWatchRequest, _ctx| {
                Ok::<Vec<performance_types::MetricWatch>, ServiceResponse>(self.handle_start_watch(req.watch))
            },
            "performance-commands", "stop-watch" => |req: StopWatchRequest, _ctx| {
                self.handle_stop_watch(&req.watch_id);
                Ok::<(), ServiceResponse>(())
            },
            "performance-state", "get-snapshot" => |_req: (), _ctx| {
                self.handle_get_snapshot()
            },
            "performance-state", "get-metric-capabilities" => |_req: (), _ctx| {
                Ok::<Vec<performance_types::MetricCapability>, ServiceResponse>(Self::handle_get_metric_capabilities())
            },
            "performance-state", "get-theme-header-settings" => |_req: (), _ctx| {
                Ok::<performance_types::ThemeHeaderSettings, ServiceResponse>(self.handle_get_theme_header_settings())
            },
            "performance-commands", "reset-theme-header-settings" => |_req: (), _ctx| {
                self.handle_set_theme_header_settings(ThemeHeaderSettings::default());
                Ok::<(), ServiceResponse>(())
            },
            "performance-commands", "set-theme-header-settings" => |req: SetThemeHeaderSettingsRequest, _ctx| {
                Ok::<performance_types::ThemeHeaderSettings, ServiceResponse>(self.handle_set_theme_header_settings(req.settings))
            },
            "performance-state", "build-theme-header-settings-form" => |_req: (), _ctx| {
                Ok::<performance_types::PromptForm, ServiceResponse>(self.handle_build_theme_header_settings_form().into())
            },
            "performance-commands", "apply-theme-header-settings-form" => |req: ApplyThemeHeaderSettingsFormRequest, _ctx| {
                let values = req.values.into_iter().map(|(key, value)| (key, value.into())).collect();
                Ok::<performance_types::ThemeHeaderSettings, ServiceResponse>(self.handle_apply_theme_header_settings_form(&values))
            },
            "performance-theme-settings", "build-form" => |defaults: ThemeSettingsPayload, _ctx| {
                Ok::<bmux_plugin_sdk::PromptRequest, ServiceResponse>(self.handle_build_theme_header_settings_form_with_defaults(defaults))
            },
            "performance-theme-settings", "apply-form" => |values: BTreeMap<String, bmux_plugin_sdk::PromptFormValue>, _ctx| {
                Ok::<ThemeSettingsPayload, ServiceResponse>(self.handle_apply_theme_header_settings_form_payload(&values))
            },
            "performance-theme-settings", "set-settings" => |settings: ThemeSettingsPayload, _ctx| {
                Ok::<ThemeSettingsPayload, ServiceResponse>(self.handle_set_theme_header_settings_payload(settings))
            },
        })
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StopWatchRequest {
    watch_id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SetSettingsRequest {
    settings: performance_types::PerformanceRuntimeSettings,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StartWatchRequest {
    watch: performance_types::MetricWatch,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SetThemeHeaderSettingsRequest {
    settings: performance_types::ThemeHeaderSettings,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ApplyThemeHeaderSettingsFormRequest {
    values: BTreeMap<String, performance_types::PromptFormValue>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct ThemeSettingsPayload {
    json: Vec<u8>,
}

impl ThemeSettingsPayload {
    fn from_settings(settings: &ThemeHeaderSettings) -> Self {
        Self {
            json: serde_json::to_vec(settings).unwrap_or_default(),
        }
    }

    fn into_value(self) -> serde_json::Value {
        serde_json::from_slice(&self.json).unwrap_or(serde_json::Value::Null)
    }
}

impl PerformancePlugin {
    fn handle_list_watches(&self) -> Vec<MetricWatch> {
        self.metrics.lock().map_or_else(
            |_| Vec::new(),
            |state| state.watches.values().cloned().collect(),
        )
    }

    fn handle_start_watch(&self, watch: MetricWatch) -> Vec<MetricWatch> {
        if watch.id.trim().is_empty() {
            return self.handle_list_watches();
        }
        if let Ok(mut state) = self.metrics.lock() {
            let watch = watch.normalized();
            state.watches.insert(watch.id.clone(), watch);
            state.snapshot.watches = state.watches.values().cloned().collect();
        }
        self.handle_list_watches()
    }

    fn handle_stop_watch(&self, watch_id: &str) {
        if let Ok(mut state) = self.metrics.lock() {
            state.watches.remove(watch_id);
            state.snapshot.watches = state.watches.values().cloned().collect();
        }
    }

    fn handle_get_snapshot(&self) -> Result<MetricsSnapshot, ServiceResponse> {
        let error = self.worker_error.lock().map_err(|_| {
            ServiceResponse::error("metrics_unavailable", "worker error lock poisoned")
        })?;
        if let Some(error) = error.as_ref() {
            return Err(ServiceResponse::error("metrics_unavailable", error.clone()));
        }
        self.metrics
            .lock()
            .map(|state| state.snapshot.clone())
            .map_err(|_| ServiceResponse::error("metrics_unavailable", "metrics lock poisoned"))
    }

    fn handle_get_metric_capabilities() -> Vec<MetricCapability> {
        metric_capabilities()
    }

    fn handle_get_theme_header_settings(&self) -> ThemeHeaderSettings {
        self.metrics.lock().map_or_else(
            |_| ThemeHeaderSettings::default(),
            |state| state.theme_header_settings.clone(),
        )
    }

    fn handle_set_theme_header_settings(
        &self,
        settings: ThemeHeaderSettings,
    ) -> ThemeHeaderSettings {
        let normalized = normalize_theme_header_settings(settings);
        if let Ok(mut state) = self.metrics.lock() {
            state.theme_header_settings = normalized.clone();
            let watch = watch_from_theme_header_settings(&normalized);
            state.watches.insert(watch.id.clone(), watch);
            state.snapshot.watches = state.watches.values().cloned().collect();
        }
        normalized
    }

    fn handle_build_theme_header_settings_form(&self) -> bmux_plugin_sdk::PromptRequest {
        let settings = self.metrics.lock().map_or_else(
            |_| ThemeHeaderSettings::default(),
            |state| state.theme_header_settings.clone(),
        );
        theme_header_settings_form(&settings)
    }

    fn handle_build_theme_header_settings_form_with_defaults(
        &self,
        defaults: ThemeSettingsPayload,
    ) -> bmux_plugin_sdk::PromptRequest {
        let settings =
            theme_header_settings_from_value(&defaults.into_value()).unwrap_or_else(|| {
                self.metrics.lock().map_or_else(
                    |_| ThemeHeaderSettings::default(),
                    |state| state.theme_header_settings.clone(),
                )
            });
        theme_header_settings_form(&normalize_theme_header_settings(settings))
    }

    fn handle_apply_theme_header_settings_form(
        &self,
        values: &BTreeMap<String, bmux_plugin_sdk::PromptFormValue>,
    ) -> ThemeHeaderSettings {
        self.handle_set_theme_header_settings(settings_from_form_values(values))
    }

    fn handle_apply_theme_header_settings_form_payload(
        &self,
        values: &BTreeMap<String, bmux_plugin_sdk::PromptFormValue>,
    ) -> ThemeSettingsPayload {
        ThemeSettingsPayload::from_settings(&self.handle_apply_theme_header_settings_form(values))
    }

    fn handle_set_theme_header_settings_payload(
        &self,
        settings: ThemeSettingsPayload,
    ) -> ThemeSettingsPayload {
        let settings_value = settings.into_value();
        let settings = theme_header_settings_from_value(&settings_value).unwrap_or_default();
        ThemeSettingsPayload::from_settings(&self.handle_set_theme_header_settings(settings))
    }
}

fn theme_header_settings_from_value(value: &serde_json::Value) -> Option<ThemeHeaderSettings> {
    serde_json::from_value(value.clone()).ok().or_else(|| {
        let object = value.as_object()?;
        let mut settings = ThemeHeaderSettings::default();
        if let Some(value) = object.get("enabled").and_then(serde_json::Value::as_bool) {
            settings.enabled = value;
        }
        if let Some(value) = object
            .get("sample_interval_ms")
            .or_else(|| object.get("sample-interval-ms"))
            .and_then(serde_json::Value::as_u64)
        {
            settings.sample_interval_ms = value;
        }
        if let Some(value) = object.get("scope").and_then(serde_json::Value::as_str) {
            settings.scope = match value {
                "system" => performance_types::ThemeHeaderScope::System,
                "both" => performance_types::ThemeHeaderScope::Both,
                _ => performance_types::ThemeHeaderScope::Pane,
            };
        }
        if let Some(value) = object.get("style").and_then(serde_json::Value::as_str) {
            settings.style = match value {
                "detailed" => performance_types::ThemeHeaderStyle::Detailed,
                "heat-only" | "heat_only" => performance_types::ThemeHeaderStyle::HeatOnly,
                _ => performance_types::ThemeHeaderStyle::Compact,
            };
        }
        if let Some(value) = object
            .get("cpu_percent_mode")
            .or_else(|| object.get("cpu-percent-mode"))
            .and_then(serde_json::Value::as_str)
        {
            settings.cpu_percent_mode = match value {
                "raw-core-sum" | "raw_core_sum" => performance_types::CpuPercentMode::RawCoreSum,
                _ => performance_types::CpuPercentMode::Normalized,
            };
        }
        if let Some(metrics) = object.get("metrics").and_then(serde_json::Value::as_array) {
            settings.metrics = metrics
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter_map(theme_header_metric_from_str)
                .collect();
        }
        Some(settings)
    })
}

fn theme_header_metric_from_str(value: &str) -> Option<ThemeHeaderMetric> {
    match value {
        "cpu" => Some(ThemeHeaderMetric::Cpu),
        "memory" => Some(ThemeHeaderMetric::Memory),
        "process-count" | "process_count" => Some(ThemeHeaderMetric::ProcessCount),
        "disk-read" | "disk_read" => Some(ThemeHeaderMetric::DiskRead),
        "disk-write" | "disk_write" => Some(ThemeHeaderMetric::DiskWrite),
        "network-rx" | "network_rx" => Some(ThemeHeaderMetric::NetworkRx),
        "network-tx" | "network_tx" => Some(ThemeHeaderMetric::NetworkTx),
        _ => None,
    }
}

fn metric_capabilities() -> Vec<MetricCapability> {
    use ThemeHeaderMetric::{Cpu, DiskRead, DiskWrite, Memory, NetworkRx, NetworkTx, ProcessCount};
    [
        (Cpu, true, None),
        (Memory, true, None),
        (ProcessCount, true, None),
        (
            DiskRead,
            false,
            Some("pane disk throughput is not available with reliable per-pane attribution yet"),
        ),
        (
            DiskWrite,
            false,
            Some("pane disk throughput is not available with reliable per-pane attribution yet"),
        ),
        (
            NetworkRx,
            false,
            Some("pane network throughput is not available with reliable per-pane attribution yet"),
        ),
        (
            NetworkTx,
            false,
            Some("pane network throughput is not available with reliable per-pane attribution yet"),
        ),
    ]
    .into_iter()
    .map(|(metric, supported, reason)| MetricCapability {
        metric,
        target: MetricTargetKind::Pane,
        supported,
        disabled_reason: reason.map(str::to_string),
        accuracy: supported.then_some(MetricAccuracy::Estimated),
    })
    .collect()
}

fn normalize_theme_header_settings(mut settings: ThemeHeaderSettings) -> ThemeHeaderSettings {
    settings.sample_interval_ms = settings.sample_interval_ms.max(500);
    let supported = metric_capabilities()
        .into_iter()
        .filter(|capability| capability.supported)
        .map(|capability| capability.metric)
        .collect::<BTreeSet<_>>();
    settings.metrics.retain(|metric| supported.contains(metric));
    if settings.metrics.is_empty() {
        settings.metrics = ThemeHeaderSettings::default().metrics;
    }
    settings
}

fn watch_from_theme_header_settings(settings: &ThemeHeaderSettings) -> MetricWatch {
    MetricWatch {
        id: "theme-header".to_string(),
        target: MetricTarget::System,
        metrics: settings
            .metrics
            .iter()
            .map(|metric| match metric {
                ThemeHeaderMetric::Cpu => bmux_performance_plugin_api::MetricName::CpuPercent,
                ThemeHeaderMetric::Memory => bmux_performance_plugin_api::MetricName::MemoryBytes,
                ThemeHeaderMetric::ProcessCount => {
                    bmux_performance_plugin_api::MetricName::ProcessCount
                }
                ThemeHeaderMetric::DiskRead => {
                    bmux_performance_plugin_api::MetricName::DiskReadBytesPerSec
                }
                ThemeHeaderMetric::DiskWrite => {
                    bmux_performance_plugin_api::MetricName::DiskWriteBytesPerSec
                }
                ThemeHeaderMetric::NetworkRx => {
                    bmux_performance_plugin_api::MetricName::NetworkRxBytesPerSec
                }
                ThemeHeaderMetric::NetworkTx => {
                    bmux_performance_plugin_api::MetricName::NetworkTxBytesPerSec
                }
            })
            .collect(),
        interval_ms: settings.sample_interval_ms,
        cpu_percent_mode: settings.cpu_percent_mode,
    }
    .normalized()
}

fn theme_header_settings_form(settings: &ThemeHeaderSettings) -> bmux_plugin_sdk::PromptRequest {
    let capabilities = metric_capabilities();
    let supported = capabilities
        .iter()
        .filter(|capability| capability.supported)
        .collect::<Vec<_>>();
    let metric_options = supported
        .iter()
        .map(|capability| {
            bmux_plugin_sdk::PromptOption::new(
                metric_form_value(capability.metric),
                metric_label(capability.metric),
            )
        })
        .collect::<Vec<_>>();
    let default_indices = metric_options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            settings
                .metrics
                .iter()
                .any(|metric| metric_form_value(*metric) == option.value)
                .then_some(index)
        })
        .collect::<Vec<_>>();

    let mut fields = vec![
        bmux_plugin_sdk::PromptFormField::new(
            "enabled",
            "Enabled",
            bmux_plugin_sdk::PromptFormFieldKind::Bool {
                default: settings.enabled,
            },
        ),
        bmux_plugin_sdk::PromptFormField::new(
            "metrics",
            "Metrics",
            bmux_plugin_sdk::PromptFormFieldKind::MultiToggle {
                options: metric_options,
                default_indices,
                min_selected: 1,
            },
        ),
        bmux_plugin_sdk::PromptFormField::new(
            "cpu_percent_mode",
            "CPU mode",
            bmux_plugin_sdk::PromptFormFieldKind::SingleSelect {
                options: vec![
                    bmux_plugin_sdk::PromptOption::new("normalized", "Normalized 0-100"),
                    bmux_plugin_sdk::PromptOption::new("raw_core_sum", "Raw core sum"),
                ],
                default_index: usize::from(settings.cpu_percent_mode == CpuPercentMode::RawCoreSum),
            },
        ),
        bmux_plugin_sdk::PromptFormField::new(
            "sample_interval_ms",
            "Sample interval ms",
            bmux_plugin_sdk::PromptFormFieldKind::Integer {
                initial_value: i64::try_from(settings.sample_interval_ms).unwrap_or(1_000),
                min: Some(500),
                max: Some(60_000),
            },
        ),
    ];
    for capability in capabilities
        .iter()
        .filter(|capability| !capability.supported)
    {
        fields.push(
            bmux_plugin_sdk::PromptFormField::new(
                format!("unsupported_{}", metric_form_value(capability.metric)),
                metric_label(capability.metric),
                bmux_plugin_sdk::PromptFormFieldKind::Bool { default: false },
            )
            .disabled(
                capability
                    .disabled_reason
                    .clone()
                    .unwrap_or_else(|| "not supported on this platform".to_string()),
            ),
        );
    }

    bmux_plugin_sdk::PromptRequest::form(
        "Performance Header Settings",
        vec![bmux_plugin_sdk::PromptFormSection::new(
            "performance-header",
            "Performance Header",
            fields,
        )],
    )
    .owner_plugin_id("bmux.performance")
    .modal_id("theme-header-settings")
    .width_range(56, 100)
}

fn settings_from_form_values(
    values: &BTreeMap<String, bmux_plugin_sdk::PromptFormValue>,
) -> ThemeHeaderSettings {
    let mut settings = ThemeHeaderSettings::default();
    if let Some(bmux_plugin_sdk::PromptFormValue::Bool(value)) = values.get("enabled") {
        settings.enabled = *value;
    }
    if let Some(bmux_plugin_sdk::PromptFormValue::Multi(metrics)) = values.get("metrics") {
        settings.metrics = metrics
            .iter()
            .filter_map(|value| metric_from_form_value(value))
            .collect();
    }
    if let Some(bmux_plugin_sdk::PromptFormValue::Single(value)) = values.get("cpu_percent_mode") {
        settings.cpu_percent_mode = if value == "raw_core_sum" {
            CpuPercentMode::RawCoreSum
        } else {
            CpuPercentMode::Normalized
        };
    }
    if let Some(bmux_plugin_sdk::PromptFormValue::Integer(value)) = values.get("sample_interval_ms")
        && let Ok(value) = u64::try_from(*value)
    {
        settings.sample_interval_ms = value;
    }
    normalize_theme_header_settings(settings)
}

fn metric_form_value(metric: ThemeHeaderMetric) -> &'static str {
    match metric {
        ThemeHeaderMetric::Cpu => "cpu",
        ThemeHeaderMetric::Memory => "memory",
        ThemeHeaderMetric::ProcessCount => "process_count",
        ThemeHeaderMetric::DiskRead => "disk_read",
        ThemeHeaderMetric::DiskWrite => "disk_write",
        ThemeHeaderMetric::NetworkRx => "network_rx",
        ThemeHeaderMetric::NetworkTx => "network_tx",
    }
}

fn metric_label(metric: ThemeHeaderMetric) -> &'static str {
    match metric {
        ThemeHeaderMetric::Cpu => "CPU",
        ThemeHeaderMetric::Memory => "Memory",
        ThemeHeaderMetric::ProcessCount => "Process count",
        ThemeHeaderMetric::DiskRead => "Disk read",
        ThemeHeaderMetric::DiskWrite => "Disk write",
        ThemeHeaderMetric::NetworkRx => "Network receive",
        ThemeHeaderMetric::NetworkTx => "Network transmit",
    }
}

fn metric_from_form_value(value: &str) -> Option<ThemeHeaderMetric> {
    match value {
        "cpu" => Some(ThemeHeaderMetric::Cpu),
        "memory" => Some(ThemeHeaderMetric::Memory),
        "process_count" => Some(ThemeHeaderMetric::ProcessCount),
        "disk_read" => Some(ThemeHeaderMetric::DiskRead),
        "disk_write" => Some(ThemeHeaderMetric::DiskWrite),
        "network_rx" => Some(ThemeHeaderMetric::NetworkRx),
        "network_tx" => Some(ThemeHeaderMetric::NetworkTx),
        _ => None,
    }
}

impl PerformancePlugin {
    fn handle_get_settings(&self) -> Result<PerformanceRuntimeSettings, ServiceResponse> {
        self.host
            .settings()
            .map_err(|error| ServiceResponse::error("settings_unavailable", error))
    }
    fn handle_set_settings(
        &self,
        requested: &PerformanceRuntimeSettings,
    ) -> Result<PerformanceRuntimeSettings, ServiceResponse> {
        self.host
            .set_settings(requested)
            .map_err(|error| ServiceResponse::error("settings_failed", error))
    }
    fn ensure_metrics_worker(&self) -> Result<(), PluginCommandError> {
        if self
            .cleanup_pending
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(PluginCommandError::unavailable(
                "metrics cleanup pending or failed; recovery required",
            ));
        }
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| PluginCommandError::failed("metrics worker lock poisoned"))?;
        if worker.is_some() {
            return Err(PluginCommandError::failed("metrics sampler already active"));
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let host = Arc::clone(&self.host);
        let metrics = Arc::clone(&self.metrics);
        let revision = Arc::clone(&self.fact_revision);
        let errors = Arc::clone(&self.worker_error);
        let task = thread::Builder::new()
            .name("bmux-performance-metrics".into())
            .spawn(move || {
                let mut system = System::new_all();
                while !stopped.load(std::sync::atomic::Ordering::Acquire) {
                    thread::park_timeout(DEFAULT_SAMPLE_INTERVAL);
                    if stopped.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let outcome = (|| -> Result<(), String> {
                        let watches = metrics
                            .lock()
                            .map_err(|_| "metrics lock poisoned")?
                            .watches
                            .values()
                            .cloned()
                            .collect();
                        let panes = host.pane_processes()?;
                        let snapshot = sample_metrics(&mut system, watches, panes);
                        host.publish_facts(performance_facts(
                            &snapshot,
                            revision
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                .wrapping_add(1),
                        ))?;
                        host.publish_metrics(&snapshot)?;
                        metrics
                            .lock()
                            .map_err(|_| "metrics lock poisoned")?
                            .snapshot = snapshot;
                        Ok(())
                    })();
                    if let Ok(mut error) = errors.lock() {
                        *error = outcome.err();
                    }
                }
            })
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        *worker = Some(MetricsWorker { stop, task });
        Ok(())
    }
}
impl Drop for PerformancePlugin {
    fn drop(&mut self) {
        // Sampler owns no execution authority. Join before releasing host ports.
        if let Err(error) = self.stop_worker() {
            tracing::warn!(%error, "metrics sampler cleanup failed");
        }
    }
}

fn performance_facts(snapshot: &MetricsSnapshot, revision: u64) -> PresentationFactSnapshot {
    let facts = snapshot
        .panes
        .values()
        .filter(|pane| pane.available)
        .map(|pane| PresentationFact {
            entity: PresentationEntityRef::new("bmux.panes", pane.pane_id.to_string()),
            key: "resource-usage".to_string(),
            role: if pane.cpu_percent >= 90.0 {
                PresentationFactRole::Warning
            } else if pane.cpu_percent > 0.0 {
                PresentationFactRole::Active
            } else {
                PresentationFactRole::Idle
            },
            short_text: format!("cpu {:.0}%", pane.cpu_percent),
            detail_text: Some(format!(
                "memory {} bytes; {} process(es)",
                pane.memory_bytes, pane.process_count
            )),
            icon_id: Some("metrics.cpu".to_string()),
            priority: 10,
        })
        .collect();
    PresentationFactSnapshot { revision, facts }
}

fn sample_metrics(
    system: &mut System,
    watches: Vec<MetricWatch>,
    pane_identities: Vec<bmux_pane_runtime_state::PaneProcessIdentity>,
) -> MetricsSnapshot {
    system.refresh_all();

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

    let cpu_count = thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let cpu_percent_mode = effective_cpu_percent_mode(&watches);
    let process_tree = ProcessTree::from_system(system, cpu_count, cpu_percent_mode);
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
                        cpu_raw_percent: process.cpu_raw_percent,
                        cpu_normalized_percent: process.cpu_normalized_percent,
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
            cpu_raw_percent: system.global_cpu_usage(),
            cpu_normalized_percent: system.global_cpu_usage().clamp(0.0, 100.0),
            memory_used_bytes: system.used_memory(),
            memory_total_bytes: system.total_memory(),
        },
        processes,
        panes,
    }
}

fn effective_cpu_percent_mode(watches: &[MetricWatch]) -> CpuPercentMode {
    if watches
        .iter()
        .any(|watch| watch.cpu_percent_mode == CpuPercentMode::RawCoreSum)
    {
        CpuPercentMode::RawCoreSum
    } else {
        CpuPercentMode::Normalized
    }
}

fn normalize_cpu_percent(raw_percent: f32, cpu_count: usize) -> f32 {
    let bounded_cpu_count = u16::try_from(cpu_count.max(1)).unwrap_or(u16::MAX);
    (raw_percent / f32::from(bounded_cpu_count)).clamp(0.0, 100.0)
}

fn display_cpu_percent(raw_percent: f32, normalized_percent: f32, mode: CpuPercentMode) -> f32 {
    match mode {
        CpuPercentMode::Normalized => normalized_percent,
        CpuPercentMode::RawCoreSum => raw_percent,
    }
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
    cpu_count: usize,
    cpu_percent_mode: CpuPercentMode,
}

impl ProcessTree {
    fn from_system(system: &System, cpu_count: usize, cpu_percent_mode: CpuPercentMode) -> Self {
        let mut processes = BTreeMap::new();
        let mut children_by_parent: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (pid, process) in system.processes() {
            let pid = pid.as_u32();
            let raw_cpu_percent = process.cpu_usage();
            let normalized_cpu_percent = normalize_cpu_percent(raw_cpu_percent, cpu_count);
            processes.insert(
                pid,
                ProcessMetricsSnapshot {
                    pid,
                    cpu_percent: display_cpu_percent(
                        raw_cpu_percent,
                        normalized_cpu_percent,
                        cpu_percent_mode,
                    ),
                    cpu_raw_percent: raw_cpu_percent,
                    cpu_normalized_percent: normalized_cpu_percent,
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
            cpu_count,
            cpu_percent_mode,
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
                aggregate.cpu_raw_percent += process.cpu_raw_percent;
                aggregate.memory_bytes =
                    aggregate.memory_bytes.saturating_add(process.memory_bytes);
                aggregate.process_count = aggregate.process_count.saturating_add(1);
            }
            if let Some(children) = self.children_by_parent.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        aggregate.cpu_normalized_percent =
            normalize_cpu_percent(aggregate.cpu_raw_percent, self.cpu_count);
        aggregate.cpu_percent = display_cpu_percent(
            aggregate.cpu_raw_percent,
            aggregate.cpu_normalized_percent,
            self.cpu_percent_mode,
        );
        aggregate
    }
}

/// Default composition: events go to the bus the current activation
/// received. Without one (for example a dynamic artifact) events are dropped,
/// never routed to a process-wide bus.
#[derive(Default)]
struct ActivationPerformanceHost {
    events: std::sync::RwLock<Option<Arc<EventBus>>>,
}
impl ActivationPerformanceHost {
    fn events(&self) -> Option<Arc<EventBus>> {
        self.events.read().ok().and_then(|events| events.clone())
    }
}
impl PerformanceHost for ActivationPerformanceHost {
    fn activate_channels(&self, initial: MetricsSnapshot) -> Result<(), String> {
        let Some(events) = self.events() else {
            return Ok(());
        };
        events.register_channel::<PerformanceEvent>(EVENT_KIND);
        events.register_channel::<MetricEvent>(METRIC_EVENT_KIND);
        events.register_state_channel_with_json_projection::<MetricsSnapshot>(
            METRICS_STATE_KIND,
            initial,
        );
        Ok(())
    }
    fn settings(&self) -> Result<PerformanceRuntimeSettings, String> {
        let handle = global_plugin_state_registry()
            .get::<PerformanceSettingsHandle>()
            .ok_or("performance settings missing")?;
        let settings = handle
            .read()
            .map_err(|_| "performance settings poisoned")?
            .0
            .current()
            .to_runtime_settings();
        Ok(settings)
    }
    fn set_settings(
        &self,
        requested: &PerformanceRuntimeSettings,
    ) -> Result<PerformanceRuntimeSettings, String> {
        let capture = PerformanceCaptureSettings::from_runtime_settings(requested);
        let settings = capture.to_runtime_settings();
        let handle = global_plugin_state_registry()
            .get::<PerformanceSettingsHandle>()
            .ok_or("performance settings missing")?;
        handle
            .read()
            .map_err(|_| "performance settings poisoned")?
            .0
            .set(capture);
        if let Some(events) = self.events() {
            let _ = events.emit(
                &EVENT_KIND,
                PerformanceEvent::SettingsUpdated {
                    settings: settings.clone().into(),
                },
            );
        }
        Ok(settings)
    }
    fn pane_processes(&self) -> Result<Vec<bmux_pane_runtime_state::PaneProcessIdentity>, String> {
        let handle = global_plugin_state_registry()
            .get::<bmux_pane_runtime_state::SessionRuntimeManagerHandle>()
            .ok_or("pane process source missing")?;
        let processes = handle
            .read()
            .map_err(|_| "pane process source poisoned")?
            .0
            .list_pane_processes();
        Ok(processes)
    }
    fn publish_metrics(&self, snapshot: &MetricsSnapshot) -> Result<(), String> {
        let Some(events) = self.events() else {
            return Ok(());
        };
        events
            .publish_state(&METRICS_STATE_KIND, snapshot.clone())
            .map_err(|error| error.to_string())?;
        let _ = events.emit(
            &METRIC_EVENT_KIND,
            MetricEvent::SnapshotUpdated {
                sampled_at_epoch_ms: snapshot.sampled_at_epoch_ms,
            },
        );
        Ok(())
    }
    fn publish_facts(&self, snapshot: PresentationFactSnapshot) -> Result<(), String> {
        global_presentation_fact_host_service()
            .registry()
            .publish_authorized("bmux.performance", "bmux.performance", snapshot)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

impl bmux_plugin_sdk::definition::PluginInstance for PerformancePlugin {
    fn service_contract_for(&self, interface: &str) -> Option<&'static [u8]> {
        <bmux_performance_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface)
    }

    fn activate(
        &self,
        context: bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>>
    {
        self.adopt_activation_bus(|| {
            bmux_plugin::take_activation_event_bus(&context.capabilities).ok()
        });
        Box::pin(async move { self.activate_owner() })
    }
    fn deactivate(
        &self,
        _context: bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>>
    {
        Box::pin(async move {
            tokio::runtime::Handle::try_current().map_err(|_| {
                PluginCommandError::unavailable("metrics cleanup requires host async runtime")
            })?;
            if self
                .cleanup_pending
                .swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                return Err(PluginCommandError::unavailable(
                    "metrics cleanup pending or failed; recovery required",
                ));
            }
            let worker = Arc::clone(&self.worker);
            let pending = Arc::clone(&self.cleanup_pending);
            // The blocking task retains cleanup authority even if this future
            // is cancelled. Only a successful join permits reactivation.
            tokio::task::spawn_blocking(move || {
                stop_metrics_worker(&worker)?;
                pending.store(false, std::sync::atomic::Ordering::Release);
                Ok(())
            })
            .await
            .map_err(|_| PluginCommandError::failed("metrics cleanup task panicked"))?
        })
    }
    fn invoke(
        &self,
        call: bmux_plugin_sdk::definition::PluginCallContext,
        context: NativeServiceContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        bmux_plugin_sdk::definition::PluginResult<ServiceResponse>,
    > {
        Box::pin(async move {
            if call.cancellation.is_cancelled() {
                return Err(PluginCommandError::unavailable(
                    "performance invocation cancelled",
                ));
            }
            Ok(self.service(&context))
        })
    }
}
// The one definition both bundling and generated dynamic packaging use:
// fresh default author state per created instance.
bmux_plugin_sdk::define_plugin!(
    rust_plugin = PerformancePlugin,
    manifest = include_str!("../plugin.toml"),
    roles = vec![bmux_plugin_sdk::definition::ComponentRole::Backend],
);

pub fn plugin_definition_with_host(
    host: Arc<dyn PerformanceHost>,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    use bmux_plugin_sdk::definition::{PluginDefinition, PluginInstance};
    PluginDefinition::new(definition_metadata(), move |_scope| {
        Ok(Arc::new(PerformancePlugin::with_host(Arc::clone(&host))) as Arc<dyn PluginInstance>)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Host;
    impl PerformanceHost for Host {
        fn activate_channels(&self, _initial: MetricsSnapshot) -> Result<(), String> {
            Ok(())
        }
        fn settings(&self) -> Result<PerformanceRuntimeSettings, String> {
            Err("selected settings unavailable".into())
        }
        fn set_settings(
            &self,
            _settings: &PerformanceRuntimeSettings,
        ) -> Result<PerformanceRuntimeSettings, String> {
            Err("selected settings unavailable".into())
        }
        fn pane_processes(
            &self,
        ) -> Result<Vec<bmux_pane_runtime_state::PaneProcessIdentity>, String> {
            Ok(Vec::new())
        }
        fn publish_metrics(&self, _snapshot: &MetricsSnapshot) -> Result<(), String> {
            Ok(())
        }
        fn publish_facts(&self, _snapshot: PresentationFactSnapshot) -> Result<(), String> {
            Ok(())
        }
    }
    #[test]
    fn selected_host_failure_is_not_default_settings() {
        let plugin = PerformancePlugin::with_host(Arc::new(Host));
        assert!(plugin.handle_get_settings().is_err());
    }
    #[test]
    fn metrics_state_and_worker_are_instance_owned() {
        let first = PerformancePlugin::with_host(Arc::new(Host));
        let second = PerformancePlugin::with_host(Arc::new(Host));
        first.handle_stop_watch(&MetricWatch::default().normalized().id);
        assert_eq!(first.handle_list_watches().len(), 0);
        assert_eq!(second.handle_list_watches().len(), 1);
        first.activate_owner().unwrap();
        assert!(first.activate_owner().is_err());
        assert!(second.worker.lock().unwrap().is_none());
        first.stop_worker().unwrap();
        assert!(first.worker.lock().unwrap().is_none());
    }
    #[test]
    fn performance_snapshot_projects_bounded_pane_facts() {
        let pane_id = uuid::Uuid::from_u128(42);
        let snapshot = MetricsSnapshot {
            sampled_at_epoch_ms: 1,
            panes: BTreeMap::from([(
                pane_id,
                PaneMetricsSnapshot {
                    pane_id,
                    session_id: None,
                    pid: None,
                    process_group_id: None,
                    cpu_percent: 95.0,
                    cpu_raw_percent: 95.0,
                    cpu_normalized_percent: 95.0,
                    memory_bytes: 4096,
                    process_count: 2,
                    available: true,
                },
            )]),
            ..MetricsSnapshot::default()
        };
        let facts = performance_facts(&snapshot, 1);
        assert_eq!(facts.facts.len(), 1);
        assert_eq!(facts.facts[0].role, PresentationFactRole::Warning);
        assert_eq!(facts.facts[0].short_text, "cpu 95%");
    }

    #[test]
    fn process_tree_aggregates_descendants() {
        let tree = ProcessTree {
            processes: BTreeMap::from([
                (
                    10,
                    ProcessMetricsSnapshot {
                        pid: 10,
                        cpu_percent: 5.0,
                        cpu_raw_percent: 5.0,
                        cpu_normalized_percent: 2.5,
                        memory_bytes: 100,
                        process_count: 1,
                    },
                ),
                (
                    11,
                    ProcessMetricsSnapshot {
                        pid: 11,
                        cpu_percent: 7.0,
                        cpu_raw_percent: 7.0,
                        cpu_normalized_percent: 3.5,
                        memory_bytes: 200,
                        process_count: 1,
                    },
                ),
                (
                    12,
                    ProcessMetricsSnapshot {
                        pid: 12,
                        cpu_percent: 9.0,
                        cpu_raw_percent: 9.0,
                        cpu_normalized_percent: 4.5,
                        memory_bytes: 300,
                        process_count: 1,
                    },
                ),
            ]),
            children_by_parent: BTreeMap::from([(10, vec![11]), (11, vec![12])]),
            cpu_count: 2,
            cpu_percent_mode: CpuPercentMode::Normalized,
        };

        let snapshot = tree.snapshot_for_root(10);
        assert_eq!(snapshot.pid, 10);
        assert!((snapshot.cpu_raw_percent - 21.0).abs() < f32::EPSILON);
        assert!((snapshot.cpu_normalized_percent - 10.5).abs() < f32::EPSILON);
        assert!((snapshot.cpu_percent - 10.5).abs() < f32::EPSILON);
        assert_eq!(snapshot.memory_bytes, 600);
        assert_eq!(snapshot.process_count, 3);
    }

    #[test]
    fn start_watch_clamps_and_stores_watch() {
        let plugin = PerformancePlugin::default();
        let watches = plugin.handle_start_watch(MetricWatch {
            id: "test-watch".to_string(),
            target: MetricTarget::System,
            metrics: Vec::new(),
            interval_ms: 1,
            cpu_percent_mode: CpuPercentMode::Normalized,
        });
        let watch = watches
            .into_iter()
            .find(|watch| watch.id == "test-watch")
            .expect("watch stored");
        assert_eq!(
            watch.interval_ms,
            bmux_performance_plugin_api::MIN_METRICS_INTERVAL_MS
        );

        plugin.handle_stop_watch("test-watch");
    }
}

fn definition_metadata() -> bmux_plugin_sdk::definition::PluginMetadata {
    bmux_plugin_sdk::definition::PluginDefinition::from_rust_plugin::<PerformancePlugin>(
        include_str!("../plugin.toml"),
        vec![bmux_plugin_sdk::definition::ComponentRole::Backend],
    )
    .expect("plugin manifest must define valid canonical metadata")
    .metadata
}

#[cfg(test)]
mod canonical_metadata_tests {
    #[test]
    fn embedded_manifest_matches_definition_metadata() {
        let definition = bmux_plugin_sdk::definition::PluginDefinition::new(
            super::definition_metadata(),
            |_scope| {
                Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                    "metadata-only regression",
                ))
            },
        );
        definition
            .validate()
            .expect("canonical embedded identity must validate");
    }
}
