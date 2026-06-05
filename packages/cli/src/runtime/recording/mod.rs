#![allow(dead_code)] // Transitional while remaining attach/playbook recording helpers move to bmux.recording.

use super::cli_parse::{
    RECORDING_AUTO_EXPORT_DIR_OVERRIDE_ENV, RECORDING_AUTO_EXPORT_OVERRIDE_ENV,
};
use super::{
    BmuxConfig, BufWriter, ConfigPaths, ConnectionContext, ConnectionPolicyScope, Context, Instant,
    IsTerminal, Path, PathBuf, RecordingEventEnvelope, RecordingEventKind, RecordingEventKindArg,
    RecordingListOrderArg, RecordingListSortArg, RecordingListStatusArg, RecordingProfileArg,
    RecordingReplayMode, RecordingStatus, RecordingSummary, Result, Uuid, Write,
    active_runtime_name, cleanup_stale_pid_file, connect_if_running_with_context,
    current_cli_build_id, io, read_server_runtime_metadata, terminal,
};
use bmux_cli_output::{Table, TableAlign, TableColumn, write_table};
use bmux_performance_state::{
    PERF_RECORDING_SCHEMA_VERSION, PERF_RECORDING_SOURCE,
    PerformanceRecordingLevel as RuntimePerformanceRecordingLevel,
    PerformanceRuntimeSettings as RuntimePerformanceRuntimeSettings,
};
use bmux_plugin_sdk::{TypedDispatchClientError, TypedServiceClientError};
use bmux_recording_plugin_api::{
    recording_commands, recording_events, recording_state, recording_types,
};
use bmux_recording_protocol::{
    DisplayActivityKind, DisplayCursorShape, DisplayTrackEnvelope, DisplayTrackEvent,
    RecordingPayload as ProtocolRecordingPayload, RecordingProfile, read_frames, write_frame,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod terminal_profile;

type RecordingPayload = ProtocolRecordingPayload<bmux_ipc::Event, bmux_ipc::ErrorCode>;

pub fn recording_plugin_error(error: recording_types::RecordingError) -> anyhow::Error {
    match error {
        recording_types::RecordingError::NoActive => anyhow::anyhow!("no active recording"),
        recording_types::RecordingError::Unavailable => {
            anyhow::anyhow!("recording runtime unavailable")
        }
        recording_types::RecordingError::Failed { reason } => anyhow::anyhow!(reason),
    }
}

fn recording_service_client_error(error: TypedServiceClientError) -> anyhow::Error {
    if let Some(details) = missing_recording_provider_details(&error) {
        let mut message = format!(
            "recording service is unavailable on the running bmux server ({details}). Restart the server with `bmux server stop` and retry; verify `bmux.recording` is enabled with `bmux plugin list`."
        );
        if let Some(hint) = stale_server_build_hint() {
            message.push(' ');
            message.push_str(&hint);
        }
        emit_recording_command_status(&message);
        return anyhow::anyhow!(message);
    }
    anyhow::Error::new(error)
}

fn missing_recording_provider_details(error: &TypedServiceClientError) -> Option<String> {
    let TypedServiceClientError::Dispatch(TypedDispatchClientError::Server { details, .. }) = error
    else {
        return None;
    };
    (details.contains("no provider for service capability='bmux.recording."))
        .then(|| details.clone())
}

fn stale_server_build_hint() -> Option<String> {
    let metadata = read_server_runtime_metadata().ok().flatten()?;
    let cli_build = current_cli_build_id().ok()?;
    (metadata.build_id != cli_build).then(|| {
        format!(
            "Running server build differs from current CLI build (server: {} at {}; cli: {}).",
            metadata.build_id, metadata.executable_path, cli_build
        )
    })
}

pub(super) async fn run_recording_start(
    session_id: Option<&str>,
    capture_input: bool,
    name: Option<&str>,
    profile: Option<RecordingProfileArg>,
    event_kinds: &[RecordingEventKindArg],
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let name = normalize_recording_name(name)?;
    let runtime_config = BmuxConfig::load().unwrap_or_default();
    cleanup_stale_pid_file().await?;
    let mut client = connect_if_running_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-recording-start",
        connection_context,
    )
    .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "recording start requires a running bmux server.\nRun `bmux server start --daemon` and retry."
            )
        })?;
    let session_id = match session_id {
        Some(raw) => Some(Uuid::parse_str(raw).context("invalid --session-id UUID")?),
        None => None,
    };
    let profile_overridden = profile.is_some();
    let effective_profile = profile.unwrap_or(RecordingProfileArg::Functional);
    let profile = recording_profile_arg_to_ipc(Some(effective_profile));
    let event_kinds = if profile_overridden || !event_kinds.is_empty() {
        resolve_event_kind_override(Some(effective_profile), event_kinds, capture_input)
    } else {
        Some(default_event_kinds_from_config(capture_input))
    };
    let summary: RecordingSummary = recording_commands::client::start(
        &mut client,
        session_id,
        capture_input,
        name,
        profile.map(Into::into),
        event_kinds.map(|kinds| kinds.into_iter().map(Into::into).collect()),
    )
    .await?
    .map(Into::into)
    .map_err(recording_plugin_error)?;
    let name_display = summary.name.as_deref().unwrap_or("-");
    println!(
        "recording started: {} name={} (capture_input={} profile={:?} kinds={})",
        summary.id,
        name_display,
        summary.capture_input,
        summary.profile,
        summary
            .event_kinds
            .iter()
            .map(|kind| recording_event_kind_name(*kind))
            .collect::<Vec<_>>()
            .join(",")
    );
    if performance_capture_enabled(runtime_config.performance.recording_level)
        && !event_kinds_include_custom(&summary.event_kinds)
    {
        eprintln!(
            "bmux warning: performance recording level '{}' is enabled, but this recording does not include `custom` events; perf telemetry will be missing",
            performance_recording_level_label(runtime_config.performance.recording_level)
        );
    }
    Ok(0)
}

pub(super) const fn recording_profile_arg_to_ipc(
    profile: Option<RecordingProfileArg>,
) -> Option<RecordingProfile> {
    match profile {
        Some(RecordingProfileArg::Full) => Some(RecordingProfile::Full),
        Some(RecordingProfileArg::Functional) => Some(RecordingProfile::Functional),
        Some(RecordingProfileArg::Visual) => Some(RecordingProfile::Visual),
        None => None,
    }
}

pub(super) fn resolve_event_kind_override(
    profile: Option<RecordingProfileArg>,
    event_kinds: &[RecordingEventKindArg],
    capture_input: bool,
) -> Option<Vec<RecordingEventKind>> {
    if !event_kinds.is_empty() {
        return Some(
            event_kinds
                .iter()
                .copied()
                .map(recording_event_kind_arg_to_ipc)
                .collect(),
        );
    }

    let profile = profile?;
    let mut kinds = match profile {
        RecordingProfileArg::Full => vec![
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::ProtocolReplyRaw,
            RecordingEventKind::PaneImage,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ],
        RecordingProfileArg::Functional => vec![
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::PaneImage,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ],
        RecordingProfileArg::Visual => vec![RecordingEventKind::PaneOutputRaw],
    };
    if capture_input && profile != RecordingProfileArg::Visual {
        kinds.push(RecordingEventKind::PaneInputRaw);
    }
    Some(kinds)
}

const fn recording_event_kind_arg_to_ipc(kind: RecordingEventKindArg) -> RecordingEventKind {
    match kind {
        RecordingEventKindArg::PaneInputRaw => RecordingEventKind::PaneInputRaw,
        RecordingEventKindArg::PaneOutputRaw => RecordingEventKind::PaneOutputRaw,
        RecordingEventKindArg::ProtocolReplyRaw => RecordingEventKind::ProtocolReplyRaw,
        RecordingEventKindArg::PaneImage => RecordingEventKind::PaneImage,
        RecordingEventKindArg::ServerEvent => RecordingEventKind::ServerEvent,
        RecordingEventKindArg::RequestStart => RecordingEventKind::RequestStart,
        RecordingEventKindArg::RequestDone => RecordingEventKind::RequestDone,
        RecordingEventKindArg::RequestError => RecordingEventKind::RequestError,
        RecordingEventKindArg::Custom => RecordingEventKind::Custom,
    }
}

fn default_event_kinds_from_config(capture_input: bool) -> Vec<RecordingEventKind> {
    let config = BmuxConfig::load().unwrap_or_default();
    default_event_kinds_for_flags(
        capture_input && config.recording.capture_input,
        config.recording.capture_output,
        config.recording.capture_events,
    )
}

fn default_event_kinds_for_flags(
    capture_input: bool,
    capture_output: bool,
    capture_events: bool,
) -> Vec<RecordingEventKind> {
    let mut kinds = Vec::new();
    if capture_input {
        kinds.push(RecordingEventKind::PaneInputRaw);
    }
    if capture_output {
        kinds.push(RecordingEventKind::PaneOutputRaw);
    }
    if capture_events {
        kinds.extend([
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ]);
    }
    if kinds.is_empty() {
        kinds.push(RecordingEventKind::PaneOutputRaw);
    }
    kinds
}

const fn performance_recording_level_label(
    level: bmux_config::PerformanceRecordingLevel,
) -> &'static str {
    match level {
        bmux_config::PerformanceRecordingLevel::Off => "off",
        bmux_config::PerformanceRecordingLevel::Basic => "basic",
        bmux_config::PerformanceRecordingLevel::Detailed => "detailed",
        bmux_config::PerformanceRecordingLevel::Trace => "trace",
    }
}

const fn performance_capture_enabled(level: bmux_config::PerformanceRecordingLevel) -> bool {
    !matches!(level, bmux_config::PerformanceRecordingLevel::Off)
}

fn event_kinds_include_custom(kinds: &[RecordingEventKind]) -> bool {
    kinds.contains(&RecordingEventKind::Custom)
}

fn normalize_recording_name(name: Option<&str>) -> Result<Option<String>> {
    let Some(name) = name else {
        return Ok(None);
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("recording name cannot be empty")
    }
    Ok(Some(trimmed.to_string()))
}

#[allow(clippy::struct_excessive_bools)] // Status report intentionally surfaces independent capture toggles.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct RecordingConfigStatus {
    capture_input: bool,
    capture_output: bool,
    capture_events: bool,
    default_event_kinds: Vec<RecordingEventKind>,
    performance_recording_level: bmux_config::PerformanceRecordingLevel,
    perf_custom_events_enabled_by_default: bool,
    segment_mb: usize,
    retention_days: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
struct RecordingStorageUsage {
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    files: u64,
    #[serde(default)]
    directories: u64,
    #[serde(default)]
    recording_dirs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct RecordingStatusView {
    active: Option<RecordingSummary>,
    queue_len: usize,
    root_path: String,
    config: RecordingConfigStatus,
    usage: RecordingStorageUsage,
}

#[derive(Debug, Clone)]
struct RecordingAutoExportSettings {
    enabled: bool,
    output_dir: Option<PathBuf>,
    fps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RecordingAutoExportOutcome {
    Disabled,
    Exported { output_path: PathBuf },
    Failed { output_path: PathBuf, error: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PerfCaptureLevel {
    Off,
    Basic,
    Detailed,
    Trace,
}

impl PerfCaptureLevel {
    #[must_use]
    pub(super) const fn from_config(level: bmux_config::PerformanceRecordingLevel) -> Self {
        match level {
            bmux_config::PerformanceRecordingLevel::Off => Self::Off,
            bmux_config::PerformanceRecordingLevel::Basic => Self::Basic,
            bmux_config::PerformanceRecordingLevel::Detailed => Self::Detailed,
            bmux_config::PerformanceRecordingLevel::Trace => Self::Trace,
        }
    }

    #[must_use]
    pub(super) const fn from_runtime(level: RuntimePerformanceRecordingLevel) -> Self {
        match level {
            RuntimePerformanceRecordingLevel::Off => Self::Off,
            RuntimePerformanceRecordingLevel::Basic => Self::Basic,
            RuntimePerformanceRecordingLevel::Detailed => Self::Detailed,
            RuntimePerformanceRecordingLevel::Trace => Self::Trace,
        }
    }

    #[must_use]
    pub(super) const fn from_plugin(
        level: bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel,
    ) -> Self {
        match level {
            bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel::Off => {
                Self::Off
            }
            bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel::Basic => {
                Self::Basic
            }
            bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel::Detailed => {
                Self::Detailed
            }
            bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel::Trace => {
                Self::Trace
            }
        }
    }

    #[must_use]
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Basic => "basic",
            Self::Detailed => "detailed",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PerfCaptureSettings {
    level: PerfCaptureLevel,
    window_ms: u64,
    max_events_per_sec: u32,
    max_payload_bytes_per_sec: usize,
}

impl PerfCaptureSettings {
    #[must_use]
    pub(super) fn from_config(config: &BmuxConfig) -> Self {
        let perf = &config.performance;
        Self {
            level: PerfCaptureLevel::from_config(perf.recording_level),
            window_ms: perf.window_ms.max(1),
            max_events_per_sec: perf.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: perf.max_payload_bytes_per_sec.max(1),
        }
    }

    #[must_use]
    pub(super) fn from_runtime_settings(settings: &RuntimePerformanceRuntimeSettings) -> Self {
        Self {
            level: PerfCaptureLevel::from_runtime(settings.recording_level),
            window_ms: settings.window_ms.max(1),
            max_events_per_sec: settings.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: settings.max_payload_bytes_per_sec.max(1),
        }
    }

    #[must_use]
    pub(super) fn from_plugin_settings(
        settings: &bmux_performance_plugin_api::performance_types::PerformanceRuntimeSettings,
    ) -> Self {
        Self {
            level: PerfCaptureLevel::from_plugin(settings.recording_level),
            window_ms: settings.window_ms.max(1),
            max_events_per_sec: settings.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: usize::try_from(settings.max_payload_bytes_per_sec)
                .unwrap_or(usize::MAX)
                .max(1),
        }
    }
}

#[derive(Debug)]
pub(super) struct PerfEventEmitter {
    settings: PerfCaptureSettings,
    rate_window_started_at: Instant,
    emitted_events_in_window: u32,
    emitted_payload_bytes_in_window: usize,
    dropped_events_since_emit: u64,
    dropped_payload_bytes_since_emit: u64,
}

impl PerfEventEmitter {
    #[must_use]
    pub(super) fn new(settings: PerfCaptureSettings) -> Self {
        Self {
            settings,
            rate_window_started_at: Instant::now(),
            emitted_events_in_window: 0,
            emitted_payload_bytes_in_window: 0,
            dropped_events_since_emit: 0,
            dropped_payload_bytes_since_emit: 0,
        }
    }

    pub(super) fn update_settings(&mut self, settings: PerfCaptureSettings) {
        self.settings = settings;
        self.rate_window_started_at = Instant::now();
        self.emitted_events_in_window = 0;
        self.emitted_payload_bytes_in_window = 0;
        self.dropped_events_since_emit = 0;
        self.dropped_payload_bytes_since_emit = 0;
    }

    #[must_use]
    pub(super) const fn window_ms(&self) -> u64 {
        self.settings.window_ms
    }

    #[must_use]
    pub(super) fn level_at_least(&self, level: PerfCaptureLevel) -> bool {
        self.settings.level >= level
    }

    #[must_use]
    pub(super) fn enabled(&self) -> bool {
        self.settings.level != PerfCaptureLevel::Off
    }

    fn reset_rate_window_if_needed(&mut self) {
        if self.rate_window_started_at.elapsed() >= std::time::Duration::from_secs(1) {
            self.rate_window_started_at = Instant::now();
            self.emitted_events_in_window = 0;
            self.emitted_payload_bytes_in_window = 0;
        }
    }

    fn can_emit_payload(&mut self, payload_len: usize) -> bool {
        if !self.enabled() {
            return false;
        }

        self.reset_rate_window_if_needed();

        let event_limit_hit = self.emitted_events_in_window >= self.settings.max_events_per_sec;
        let payload_limit_hit = self
            .emitted_payload_bytes_in_window
            .saturating_add(payload_len)
            > self.settings.max_payload_bytes_per_sec;
        if event_limit_hit || payload_limit_hit {
            self.dropped_events_since_emit = self.dropped_events_since_emit.saturating_add(1);
            self.dropped_payload_bytes_since_emit = self
                .dropped_payload_bytes_since_emit
                .saturating_add(u64::try_from(payload_len).unwrap_or(u64::MAX));
            return false;
        }

        self.emitted_events_in_window = self.emitted_events_in_window.saturating_add(1);
        self.emitted_payload_bytes_in_window = self
            .emitted_payload_bytes_in_window
            .saturating_add(payload_len);
        true
    }

    fn normalized_payload(&mut self, payload: serde_json::Value) -> serde_json::Value {
        let mut object = match payload {
            serde_json::Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("value".to_string(), other);
                map
            }
        };
        object.insert(
            "schema_version".to_string(),
            serde_json::Value::from(PERF_RECORDING_SCHEMA_VERSION),
        );
        object.insert(
            "level".to_string(),
            serde_json::Value::String(self.settings.level.as_str().to_string()),
        );
        object.insert(
            "runtime".to_string(),
            serde_json::Value::String(active_runtime_name()),
        );
        object.insert(
            "ts_epoch_ms".to_string(),
            serde_json::Value::from(epoch_millis_now()),
        );

        if self.dropped_events_since_emit > 0 || self.dropped_payload_bytes_since_emit > 0 {
            object.insert(
                "dropped_events_since_emit".to_string(),
                serde_json::Value::from(self.dropped_events_since_emit),
            );
            object.insert(
                "dropped_payload_bytes_since_emit".to_string(),
                serde_json::Value::from(self.dropped_payload_bytes_since_emit),
            );
            self.dropped_events_since_emit = 0;
            self.dropped_payload_bytes_since_emit = 0;
        }

        serde_json::Value::Object(object)
    }

    pub(super) async fn emit_with_client(
        &mut self,
        client: &mut bmux_client::BmuxClient,
        session_id: Option<Uuid>,
        pane_id: Option<Uuid>,
        event_name: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }

        let payload = self.normalized_payload(payload);
        let encoded = serde_json::to_vec(&payload).context("failed encoding perf payload")?;
        if !self.can_emit_payload(encoded.len()) {
            return Ok(());
        }

        recording_commands::client::write_custom_event(
            client,
            session_id,
            pane_id,
            PERF_RECORDING_SOURCE.to_string(),
            event_name.to_string(),
            encoded,
        )
        .await?
        .map_err(recording_plugin_error)
    }

    pub(super) async fn emit_with_streaming_client(
        &mut self,
        client: &mut bmux_client::StreamingBmuxClient,
        session_id: Option<Uuid>,
        pane_id: Option<Uuid>,
        event_name: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }

        let payload = self.normalized_payload(payload);
        let encoded = serde_json::to_vec(&payload).context("failed encoding perf payload")?;
        if !self.can_emit_payload(encoded.len()) {
            return Ok(());
        }

        recording_commands::client::write_custom_event(
            client,
            session_id,
            pane_id,
            PERF_RECORDING_SOURCE.to_string(),
            event_name.to_string(),
            encoded,
        )
        .await?
        .map_err(recording_plugin_error)
    }
}

#[allow(clippy::cast_possible_truncation)]
fn epoch_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn parse_bool_env_flag(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_path_override(name: &str) -> Option<PathBuf> {
    let raw = std::env::var_os(name)?;
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return Some(path);
    }
    match std::env::current_dir() {
        Ok(cwd) => Some(cwd.join(path)),
        Err(_) => Some(path),
    }
}

fn recording_auto_export_settings() -> RecordingAutoExportSettings {
    let paths = ConfigPaths::default();
    let config = BmuxConfig::load_from_path(&paths.config_file()).unwrap_or_default();
    let output_dir = env_path_override(RECORDING_AUTO_EXPORT_DIR_OVERRIDE_ENV)
        .or_else(|| config.recording_auto_export_dir(&paths));
    let config_enabled = config.recording.auto_export || output_dir.is_some();
    let enabled = std::env::var(RECORDING_AUTO_EXPORT_OVERRIDE_ENV)
        .ok()
        .map_or(config_enabled, |raw| {
            parse_bool_env_flag(&raw).unwrap_or_else(|| {
                tracing::warn!(
                    "ignoring invalid {} value {:?}",
                    RECORDING_AUTO_EXPORT_OVERRIDE_ENV,
                    raw
                );
                config_enabled
            })
        });
    RecordingAutoExportSettings {
        enabled,
        output_dir,
        fps: config.recording.export.fps.max(1),
    }
}

fn auto_export_default_dir(recording_dir: &Path) -> PathBuf {
    recording_dir
        .parent()
        .map_or_else(|| recording_dir.to_path_buf(), std::path::Path::to_path_buf)
}

fn auto_export_filename_stem(timestamp: time::OffsetDateTime) -> String {
    let hour = timestamp.hour();
    let (hour12, meridiem) = match hour {
        0 => (12_u8, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12_u8, "PM"),
        _ => (hour - 12, "PM"),
    };
    format!(
        "Recording {:04}-{:02}-{:02} at {}.{:02}.{:02} {meridiem}",
        timestamp.year(),
        u8::from(timestamp.month()),
        timestamp.day(),
        hour12,
        timestamp.minute(),
        timestamp.second(),
    )
}

fn unique_auto_export_path(output_dir: &Path, stem: &str) -> PathBuf {
    let mut candidate = output_dir.join(format!("{stem}.gif"));
    if !candidate.exists() {
        return candidate;
    }
    let mut suffix = 2_u32;
    loop {
        candidate = output_dir.join(format!("{stem} {suffix}.gif"));
        if !candidate.exists() {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn auto_export_output_path(recording_dir: &Path, explicit_output_dir: Option<&Path>) -> PathBuf {
    let output_dir = explicit_output_dir.map_or_else(
        || auto_export_default_dir(recording_dir),
        std::path::Path::to_path_buf,
    );
    unique_auto_export_path(
        &output_dir,
        &auto_export_filename_stem(time::OffsetDateTime::now_utc()),
    )
}

pub(super) async fn maybe_auto_export_recording(
    recording_id: Uuid,
    recording_path: Option<&Path>,
    fps_override: Option<u32>,
) -> RecordingAutoExportOutcome {
    let settings = recording_auto_export_settings();
    if !settings.enabled {
        return RecordingAutoExportOutcome::Disabled;
    }

    let recording_dir = recording_path.map_or_else(
        || recordings_root_dir().join(recording_id.to_string()),
        std::path::Path::to_path_buf,
    );
    let output_path = auto_export_output_path(&recording_dir, settings.output_dir.as_deref());
    let output = output_path.to_string_lossy().into_owned();
    publish_recording_export_started(recording_id, output.clone());
    let recording_id_string = recording_id.to_string();
    let fps = fps_override.unwrap_or(settings.fps).max(1);
    match super::recording_cli::run_recording_auto_export_gif(
        &recording_id_string,
        &output,
        Some(fps),
    )
    .await
    {
        Ok(_) => {
            tracing::info!(
                %recording_id,
                output_path = %output_path.display(),
                fps,
                "recording auto-export completed"
            );
            publish_recording_export_completed(recording_id, output);
            RecordingAutoExportOutcome::Exported { output_path }
        }
        Err(error) => {
            let error = error.to_string();
            tracing::warn!(
                %recording_id,
                output_path = %output_path.display(),
                fps,
                error = %error,
                "recording auto-export failed"
            );
            publish_recording_export_failed(recording_id, output, error.clone());
            RecordingAutoExportOutcome::Failed { output_path, error }
        }
    }
}

fn publish_recording_export_started(recording_id: Uuid, output_path: String) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::ExportStarted {
            recording_id,
            output_path,
        },
    );
}

fn publish_recording_export_completed(recording_id: Uuid, output_path: String) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::ExportCompleted {
            recording_id,
            output_path,
        },
    );
}

fn publish_recording_export_failed(recording_id: Uuid, output_path: String, reason: String) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::ExportFailed {
            recording_id,
            output_path,
            reason,
        },
    );
}

fn emit_recording_command_status(message: impl Into<String>) {
    bmux_plugin_sdk::record_command_outcome_metadata(
        bmux_plugin_sdk::COMMAND_OUTCOME_STATUS_MESSAGE_KEY,
        serde_json::json!(message.into()),
    );
}

fn print_auto_export_outcome(recording_id: Uuid, outcome: &RecordingAutoExportOutcome) {
    match outcome {
        RecordingAutoExportOutcome::Disabled => {}
        RecordingAutoExportOutcome::Exported { output_path } => {
            println!("recording auto-exported: {}", output_path.display());
        }
        RecordingAutoExportOutcome::Failed { output_path, error } => {
            eprintln!(
                "bmux warning: recording auto-export failed for {} (output={}): {}",
                recording_id,
                output_path.display(),
                error
            );
        }
    }
}

fn auto_export_status_suffix(outcome: &RecordingAutoExportOutcome) -> Option<String> {
    match outcome {
        RecordingAutoExportOutcome::Disabled => None,
        RecordingAutoExportOutcome::Exported { output_path } => {
            Some(format!("; GIF exported to {}", output_path.display()))
        }
        RecordingAutoExportOutcome::Failed { output_path, error } => Some(format!(
            "; GIF export failed for {}: {error}",
            output_path.display()
        )),
    }
}

fn recording_config_and_root() -> (RecordingConfigStatus, PathBuf) {
    let paths = ConfigPaths::default();
    let (config, root) = BmuxConfig::load_from_path(&paths.config_file()).map_or_else(
        |_| (BmuxConfig::default(), paths.recordings_dir()),
        |config| {
            let root = config.recordings_dir(&paths);
            (config, root)
        },
    );
    let capture_input = config.recording.capture_input;
    let capture_output = config.recording.capture_output;
    let capture_events = config.recording.capture_events;
    let default_event_kinds =
        default_event_kinds_for_flags(capture_input, capture_output, capture_events);
    (
        RecordingConfigStatus {
            capture_input,
            capture_output,
            capture_events,
            performance_recording_level: config.performance.recording_level,
            perf_custom_events_enabled_by_default: event_kinds_include_custom(&default_event_kinds),
            default_event_kinds,
            segment_mb: config.recording.segment_mb,
            retention_days: config.recording.retention_days,
        },
        root,
    )
}

fn collect_recording_storage_usage(root: &Path) -> Result<RecordingStorageUsage> {
    if !root.exists() {
        return Ok(RecordingStorageUsage::default());
    }
    let mut usage = RecordingStorageUsage::default();
    collect_recording_storage_usage_recursive(root, &mut usage, true)?;
    Ok(usage)
}

fn collect_recording_storage_usage_recursive(
    dir: &Path,
    usage: &mut RecordingStorageUsage,
    is_root: bool,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed reading recordings dir {}", dir.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            if is_root && entry.file_name() == ".rolling" {
                continue;
            }
            usage.directories = usage.directories.saturating_add(1);
            if path.join("manifest.json").exists() {
                usage.recording_dirs = usage.recording_dirs.saturating_add(1);
            }
            collect_recording_storage_usage_recursive(&path, usage, false)?;
            continue;
        }
        if file_type.is_file() {
            usage.files = usage.files.saturating_add(1);
            usage.bytes = usage.bytes.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)] // Byte size formatting; precision loss is acceptable for display
fn format_byte_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

const fn recording_status_label(ended_epoch_ms: Option<u64>) -> &'static str {
    if ended_epoch_ms.is_some() {
        "done"
    } else {
        "active"
    }
}

fn format_recording_age(started_epoch_ms: u64, now_epoch_ms: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const YEAR: u64 = 365 * DAY;

    let elapsed_secs = now_epoch_ms.saturating_sub(started_epoch_ms) / 1_000;
    if elapsed_secs == 0 {
        return "now".to_string();
    }

    let (value, unit) = if elapsed_secs < MINUTE {
        (elapsed_secs, "s")
    } else if elapsed_secs < HOUR {
        (elapsed_secs / MINUTE, "m")
    } else if elapsed_secs < DAY {
        (elapsed_secs / HOUR, "h")
    } else if elapsed_secs < WEEK {
        (elapsed_secs / DAY, "d")
    } else if elapsed_secs < YEAR {
        (elapsed_secs / WEEK, "w")
    } else {
        (elapsed_secs / YEAR, "y")
    };

    format!("{value}{unit} ago")
}

fn write_stdout_table(table: &Table) -> Result<()> {
    let mut stdout = io::stdout().lock();
    write_table(&mut stdout, table).context("failed rendering recording list table")
}

const RECORDING_LIST_DEFAULT_LIMIT: usize = 10;

#[derive(Debug, Clone, Copy)]
pub(super) struct RecordingListOptions<'a> {
    pub limit: Option<usize>,
    pub all: bool,
    pub sort: Option<RecordingListSortArg>,
    pub order: Option<RecordingListOrderArg>,
    pub status: Option<RecordingListStatusArg>,
    pub query: Option<&'a str>,
}

const fn resolve_recording_list_limit(
    as_json: bool,
    explicit_limit: Option<usize>,
    all: bool,
) -> Option<usize> {
    if all {
        None
    } else if let Some(limit) = explicit_limit {
        Some(limit)
    } else if as_json {
        None
    } else {
        Some(RECORDING_LIST_DEFAULT_LIMIT)
    }
}

const fn default_recording_list_order(sort: RecordingListSortArg) -> RecordingListOrderArg {
    match sort {
        RecordingListSortArg::Started
        | RecordingListSortArg::Events
        | RecordingListSortArg::Size => RecordingListOrderArg::Desc,
        RecordingListSortArg::Name => RecordingListOrderArg::Asc,
    }
}

const fn recording_matches_status(
    recording: &RecordingSummary,
    status: RecordingListStatusArg,
) -> bool {
    match status {
        RecordingListStatusArg::All => true,
        RecordingListStatusArg::Active => recording.ended_epoch_ms.is_none(),
        RecordingListStatusArg::Done => recording.ended_epoch_ms.is_some(),
    }
}

fn recording_matches_query(recording: &RecordingSummary, query: &str) -> bool {
    let id = recording.id.to_string();
    if id.starts_with(query) {
        return true;
    }
    recording
        .name
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().contains(query))
}

fn filter_recordings(
    recordings: Vec<RecordingSummary>,
    status: RecordingListStatusArg,
    query: Option<&str>,
) -> Vec<RecordingSummary> {
    let normalized_query = query
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);

    recordings
        .into_iter()
        .filter(|recording| {
            recording_matches_status(recording, status)
                && normalized_query
                    .as_deref()
                    .is_none_or(|value| recording_matches_query(recording, value))
        })
        .collect()
}

fn compare_recordings(
    left: &RecordingSummary,
    right: &RecordingSummary,
    sort: RecordingListSortArg,
) -> std::cmp::Ordering {
    let primary = match sort {
        RecordingListSortArg::Started => left.started_epoch_ms.cmp(&right.started_epoch_ms),
        RecordingListSortArg::Events => left.event_count.cmp(&right.event_count),
        RecordingListSortArg::Size => left.payload_bytes.cmp(&right.payload_bytes),
        RecordingListSortArg::Name => {
            let left_name = left.name.as_deref().unwrap_or("");
            let right_name = right.name.as_deref().unwrap_or("");
            let presence = left_name.is_empty().cmp(&right_name.is_empty());
            if presence != std::cmp::Ordering::Equal {
                return presence;
            }
            let by_name = left_name
                .to_ascii_lowercase()
                .cmp(&right_name.to_ascii_lowercase());
            if by_name != std::cmp::Ordering::Equal {
                return by_name;
            }
            left.started_epoch_ms.cmp(&right.started_epoch_ms)
        }
    };

    if primary != std::cmp::Ordering::Equal {
        return primary;
    }

    left.id.cmp(&right.id)
}

fn sort_recordings(
    recordings: &mut [RecordingSummary],
    sort: RecordingListSortArg,
    order: RecordingListOrderArg,
) {
    recordings.sort_by(|left, right| {
        let ordering = compare_recordings(left, right, sort);
        if order == RecordingListOrderArg::Asc {
            ordering
        } else {
            ordering.reverse()
        }
    });
}

pub(super) async fn run_recording_stop(
    recording_id: Option<&str>,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect_if_running_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-recording-stop",
        connection_context,
    )
    .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "recording stop requires a running bmux server.\nRun `bmux server start --daemon` and retry."
            )
        })?;
    let recording_id = match recording_id {
        Some(raw) => Some(Uuid::parse_str(raw).context("invalid recording id")?),
        None => None,
    };
    let stopped_id = recording_commands::client::stop(&mut client, recording_id)
        .await?
        .map_err(recording_plugin_error)?;
    println!("recording stopped: {stopped_id}");
    let auto_export = maybe_auto_export_recording(stopped_id, None, None).await;
    print_auto_export_outcome(stopped_id, &auto_export);
    let mut status = format!("recording stopped: {stopped_id}");
    if let Some(suffix) = auto_export_status_suffix(&auto_export) {
        status.push_str(&suffix);
    }
    emit_recording_command_status(status);
    Ok(0)
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_recording_status(
    as_json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let runtime_status = match connect_if_running_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-recording-status",
        connection_context,
    )
    .await?
    {
        Some(mut client) => recording_state::client::status(&mut client).await?.into(),
        None => offline_recording_status(),
    };
    let (config, root_path) = recording_config_and_root();
    let usage = collect_recording_storage_usage(&root_path)?;
    let status = RecordingStatusView {
        active: runtime_status.active,
        queue_len: runtime_status.queue_len,
        root_path: root_path.display().to_string(),
        config,
        usage,
    };

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status)
                .context("failed encoding recording status json")?
        );
        return Ok(0);
    }

    println!("recordings root: {}", status.root_path);
    println!(
        "default capture input: {}",
        if status.config.capture_input {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "default capture output: {}",
        if status.config.capture_output {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "default capture events: {}",
        if status.config.capture_events {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "performance recording level: {}",
        performance_recording_level_label(status.config.performance_recording_level)
    );
    println!(
        "default perf custom-event capture: {}",
        if status.config.perf_custom_events_enabled_by_default {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "default event kinds: {}",
        status
            .config
            .default_event_kinds
            .iter()
            .map(|kind| recording_event_kind_name(*kind))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "segment size: {} MiB retention days: {}",
        status.config.segment_mb, status.config.retention_days
    );
    if performance_capture_enabled(status.config.performance_recording_level)
        && !status.config.perf_custom_events_enabled_by_default
    {
        eprintln!(
            "bmux warning: perf recording is enabled but default recording event kinds exclude `custom`; enable `recording.capture_events` or add `--kind custom` when starting recordings"
        );
    }

    if let Some(active) = status.active.as_ref() {
        println!(
            "active recording: {} name={} events={} bytes={} capture_input={} profile={:?} kinds={} path={}",
            active.id,
            active.name.as_deref().unwrap_or("-"),
            active.event_count,
            active.payload_bytes,
            active.capture_input,
            active.profile,
            active
                .event_kinds
                .iter()
                .map(|kind| recording_event_kind_name(*kind))
                .collect::<Vec<_>>()
                .join(","),
            active.path
        );
    } else {
        println!("active recording: none");
    }
    println!("queue length: {}", status.queue_len);
    println!(
        "usage: bytes={} ({}) files={} dirs={} recordings={}",
        status.usage.bytes,
        format_byte_size(status.usage.bytes),
        status.usage.files,
        status.usage.directories,
        status.usage.recording_dirs
    );
    Ok(0)
}

pub(super) fn run_recording_path(as_json: bool) -> Result<u8> {
    let (_config, root_path) = recording_config_and_root();
    let path = root_path.display().to_string();
    if as_json {
        let payload = serde_json::json!({ "path": path });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed encoding recording path json")?
        );
    } else {
        println!("{path}");
    }
    Ok(0)
}

pub(super) async fn run_recording_list(
    as_json: bool,
    options: RecordingListOptions<'_>,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let recordings = match connect_if_running_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-recording-list",
        connection_context,
    )
    .await?
    {
        Some(mut client) => recording_state::client::list_recordings(&mut client)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
        None => list_recordings_from_disk()?,
    };

    let sort = options.sort.unwrap_or(RecordingListSortArg::Started);
    let order = options
        .order
        .unwrap_or_else(|| default_recording_list_order(sort));
    let status = options.status.unwrap_or(RecordingListStatusArg::All);

    let mut recordings = filter_recordings(recordings, status, options.query);
    sort_recordings(&mut recordings, sort, order);

    let total_count = recordings.len();
    if let Some(limit) = resolve_recording_list_limit(as_json, options.limit, options.all) {
        recordings.truncate(limit);
    }

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&recordings)
                .context("failed encoding recording list json")?
        );
        return Ok(0);
    }

    if recordings.is_empty() {
        println!("no recordings");
        return Ok(0);
    }

    let now_epoch_ms = epoch_millis_now();
    let mut table = Table::new(vec![
        TableColumn::new("ID").min_width(36),
        TableColumn::new("NAME").min_width(8),
        TableColumn::new("STATUS").min_width(6),
        TableColumn::new("STARTED").min_width(8),
        TableColumn::new("EVENTS")
            .align(TableAlign::Right)
            .min_width(6),
        TableColumn::new("SIZE").min_width(8),
    ]);
    for recording in recordings {
        table.push_row(vec![
            recording.id.to_string(),
            recording.name.unwrap_or_else(|| "-".to_string()),
            recording_status_label(recording.ended_epoch_ms).to_string(),
            format_recording_age(recording.started_epoch_ms, now_epoch_ms),
            recording.event_count.to_string(),
            format_byte_size(recording.payload_bytes),
        ]);
    }
    write_stdout_table(&table)?;
    if total_count > table.rows().len() {
        println!(
            "showing {} of {} recordings (use --limit N or --all)",
            table.rows().len(),
            total_count
        );
    }
    Ok(0)
}

pub(super) async fn run_recording_delete(
    recording_id_or_prefix: &str,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    if let Some(mut client) = connect_if_running_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-recording-delete",
        connection_context,
    )
    .await?
    {
        let status: RecordingStatus = recording_state::client::status(&mut client).await?.into();
        let recordings: Vec<RecordingSummary> =
            recording_state::client::list_recordings(&mut client)
                .await?
                .into_iter()
                .map(Into::into)
                .collect();
        let resolved = resolve_recording_id_prefix(recording_id_or_prefix, &recordings)?;

        if status
            .active
            .as_ref()
            .is_some_and(|active| active.id == resolved)
        {
            let stopped_id = recording_commands::client::stop(&mut client, Some(resolved))
                .await?
                .map_err(recording_plugin_error)?;
            println!("stopped active recording {stopped_id} before delete");
        }

        let deleted_id = recording_commands::client::delete(&mut client, resolved)
            .await?
            .map_err(recording_plugin_error)?;
        println!("deleted recording {deleted_id}");
    } else {
        let recordings = list_recordings_from_disk()?;
        let resolved = resolve_recording_id_prefix(recording_id_or_prefix, &recordings)?;
        delete_recording_dir(resolved)?;
        println!("deleted recording {resolved}");
    }
    Ok(0)
}

pub(super) async fn run_recording_delete_all(
    yes: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    if !confirm_delete_all_recordings(yes)? {
        println!("skipped recording delete-all");
        return Ok(0);
    }

    cleanup_stale_pid_file().await?;
    if let Some(mut client) = connect_if_running_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-recording-delete-all",
        connection_context,
    )
    .await?
    {
        let status: RecordingStatus = recording_state::client::status(&mut client).await?.into();
        if let Some(active) = status.active {
            let stopped_id = recording_commands::client::stop(&mut client, Some(active.id))
                .await?
                .map_err(recording_plugin_error)?;
            println!("stopped active recording {stopped_id} before delete");
        }
        let deleted_count =
            usize::try_from(recording_commands::client::delete_all(&mut client).await?)
                .unwrap_or(usize::MAX);
        println!("deleted {deleted_count} recordings");
    } else {
        let deleted_count = delete_all_recordings_from_disk()?;
        println!("deleted {deleted_count} recordings");
    }
    Ok(0)
}

pub(super) async fn run_recording_cut(
    last_seconds: Option<u64>,
    export_fps: Option<u32>,
    name: Option<&str>,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let name = normalize_recording_name(name)?;
    cleanup_stale_pid_file().await?;
    let mut client = connect_if_running_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-recording-cut",
        connection_context,
    )
    .await?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "recording cut requires a running bmux server.\nRun `bmux server start --daemon` and retry."
        )
    })?;

    let queued_job =
        recording_commands::client::queue_cut(&mut client, last_seconds, name, export_fps)
            .await
            .map_err(recording_service_client_error)?;
    println!("recording cut queued: {}", queued_job.id);

    let completed_job = wait_for_recording_job(&mut client, queued_job).await?;
    let recording_path = completed_job.recording_path.as_deref().unwrap_or("-");
    let export_path = completed_job.export_output_path.as_deref();
    match completed_job.status {
        recording_types::RecordingJobStatus::Completed => {
            let mut status = format!("recording cut created: {recording_path}");
            if let Some(output) = export_path {
                let _ = write!(status, "; GIF exported to {output}");
                println!("recording GIF exported: {output}");
            }
            println!("recording cut created: {recording_path}");
            emit_recording_command_status(status);
            Ok(0)
        }
        recording_types::RecordingJobStatus::Failed if completed_job.recording_id.is_some() => {
            let reason = completed_job
                .error
                .as_deref()
                .unwrap_or("recording export failed");
            eprintln!("bmux warning: recording cut created but export failed: {reason}");
            println!("recording cut created: {recording_path}");
            emit_recording_command_status(format!(
                "recording cut created: {recording_path}; GIF export failed: {reason}"
            ));
            Ok(0)
        }
        recording_types::RecordingJobStatus::Failed => {
            let reason = completed_job
                .error
                .unwrap_or_else(|| "recording cut failed".to_string());
            emit_recording_command_status(format!("recording cut failed: {reason}"));
            Err(anyhow::anyhow!(reason))
        }
        other => Err(anyhow::anyhow!(
            "recording cut job ended unexpectedly with status {other:?}"
        )),
    }
}

async fn wait_for_recording_job(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    queued_job: recording_types::RecordingJob,
) -> Result<recording_types::RecordingJob> {
    let mut last_job = queued_job;
    loop {
        if matches!(
            last_job.status,
            recording_types::RecordingJobStatus::Completed
                | recording_types::RecordingJobStatus::Failed
        ) {
            return Ok(last_job);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        last_job = recording_state::client::job_status(client, last_job.id)
            .await
            .map_err(recording_service_client_error)?
            .ok_or_else(|| anyhow::anyhow!("recording job disappeared: {}", last_job.id))?;
    }
}

pub(super) async fn run_recording_prune(
    older_than: Option<u64>,
    json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let deleted_count = if let Some(mut client) = connect_if_running_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-recording-prune",
        connection_context,
    )
    .await?
    {
        usize::try_from(recording_commands::client::prune(&mut client, older_than).await?)
            .unwrap_or(usize::MAX)
    } else {
        let root = recordings_root_dir();
        let config = bmux_config::BmuxConfig::load().unwrap_or_default();
        let retention = older_than.unwrap_or(config.recording.retention_days);
        bmux_recording_plugin_api::prune_old_recordings(&root, retention)?
    };

    if json {
        let report = serde_json::json!({
            "deleted_count": deleted_count,
            "older_than_days": older_than,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if deleted_count > 0 {
        println!("pruned {deleted_count} recording(s)");
    } else {
        println!("no recordings to prune");
    }

    Ok(0)
}

#[allow(clippy::too_many_lines)]
pub(super) fn run_recording_inspect(
    recording_id: &str,
    limit: usize,
    kind: Option<&str>,
    as_json: bool,
) -> Result<u8> {
    let events = load_recording_events(recording_id)?;
    let filtered = events
        .into_iter()
        .filter(|event| {
            kind.is_none_or(|kind| {
                recording_event_kind_name(event.kind) == kind.to_ascii_lowercase()
            })
        })
        .take(limit.max(1))
        .collect::<Vec<_>>();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&filtered)
                .context("failed encoding recording inspect json")?
        );
        return Ok(0);
    }
    for event in filtered {
        println!(
            "seq={} t={} kind={:?} session={:?} pane={:?} client={:?}",
            event.seq, event.mono_ns, event.kind, event.session_id, event.pane_id, event.client_id
        );
    }
    Ok(0)
}

#[derive(Debug, Clone, serde::Serialize, Default)]
struct PerfTimingSummary {
    count: u64,
    min_ms: u64,
    p50_ms: u64,
    p95_ms: u64,
    p99_ms: u64,
    avg_ms: u64,
    max_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
struct PerfOutlierSample {
    event_name: String,
    metric: String,
    value_ms: u64,
    p95_ms: u64,
    ts_epoch_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
struct PerfRenderOutlierSample {
    reason: String,
    frame_index: Option<u64>,
    since_attach_start_ms: Option<u64>,
    frame_render_ms: Option<u64>,
    frame_bytes: Option<u64>,
    full_frame_fallback: bool,
    full_surface_fallbacks: u64,
    dirty_reasons: Vec<String>,
    dirty_events: Vec<serde_json::Value>,
    extension_stats: BTreeMap<String, BTreeMap<String, u64>>,
}

#[derive(Debug, Clone)]
struct PerfTimingSample {
    event_name: String,
    metric: String,
    value_ms: u64,
    ts_epoch_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
struct PerfAnalysisReport {
    recording_events: usize,
    perf_events: usize,
    malformed_payloads: usize,
    dropped_events_reported: u64,
    dropped_payload_bytes_reported: u64,
    first_ts_epoch_ms: Option<u64>,
    last_ts_epoch_ms: Option<u64>,
    span_ms: Option<u64>,
    by_event_name: BTreeMap<String, u64>,
    by_level: BTreeMap<String, u64>,
    attach_window_counters: BTreeMap<String, u64>,
    overrender_counters: BTreeMap<String, u64>,
    extension_counters: BTreeMap<String, BTreeMap<String, u64>>,
    timings_ms: BTreeMap<String, PerfTimingSummary>,
    outlier_samples: Vec<PerfOutlierSample>,
    render_outliers: Vec<PerfRenderOutlierSample>,
    connect_to_first_frame_ms: Option<u64>,
    connect_to_interactive_ms: Option<u64>,
    reconnect_outage_max_ms: Option<u64>,
    hints: Vec<String>,
}

fn percentile_nearest_rank(sorted_values: &[u64], percentile: u8) -> u64 {
    if sorted_values.is_empty() {
        return 0;
    }
    let clamped = usize::from(percentile.min(100));
    let len = sorted_values.len();
    let rank = (clamped.saturating_mul(len).saturating_add(99)) / 100;
    let index = rank.saturating_sub(1).min(len.saturating_sub(1));
    sorted_values[index]
}

fn timing_summary_from_values(values: &[u64]) -> PerfTimingSummary {
    if values.is_empty() {
        return PerfTimingSummary::default();
    }

    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let count = u64::try_from(sorted.len()).unwrap_or(u64::MAX);
    let sum = sorted
        .iter()
        .fold(0_u128, |acc, value| acc.saturating_add(u128::from(*value)));
    let avg_ms = u64::try_from(sum / u128::from(count.max(1))).unwrap_or(u64::MAX);

    PerfTimingSummary {
        count,
        min_ms: sorted[0],
        p50_ms: percentile_nearest_rank(&sorted, 50),
        p95_ms: percentile_nearest_rank(&sorted, 95),
        p99_ms: percentile_nearest_rank(&sorted, 99),
        avg_ms,
        max_ms: *sorted.last().unwrap_or(&0),
    }
}

fn perf_counter(counters: &BTreeMap<String, u64>, name: &str) -> u64 {
    counters.get(name).copied().unwrap_or(0)
}

fn parse_extension_stats(
    value: Option<&serde_json::Value>,
) -> BTreeMap<String, BTreeMap<String, u64>> {
    let Some(object) = value.and_then(serde_json::Value::as_object) else {
        return BTreeMap::new();
    };
    object
        .iter()
        .filter_map(|(extension_name, counters)| {
            let counters = counters.as_object()?;
            let parsed = counters
                .iter()
                .filter_map(|(counter_name, value)| {
                    value
                        .as_u64()
                        .map(|counter| (counter_name.clone(), counter))
                })
                .collect::<BTreeMap<_, _>>();
            (!parsed.is_empty()).then(|| (extension_name.clone(), parsed))
        })
        .collect()
}

fn aggregate_extension_counters(
    target: &mut BTreeMap<String, BTreeMap<String, u64>>,
    source: &BTreeMap<String, BTreeMap<String, u64>>,
) {
    for (extension_name, counters) in source {
        let target_counters = target.entry(extension_name.clone()).or_default();
        for (counter_name, value) in counters {
            let target_value = target_counters.entry(counter_name.clone()).or_default();
            *target_value = target_value.saturating_add(*value);
        }
    }
}

fn string_array_field(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Vec<String> {
    object
        .get(field)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn render_outlier_from_frame_trace(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Option<PerfRenderOutlierSample> {
    let full_frame_fallback = object
        .get("full_frame_fallback")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let extension_render_calls = object
        .get("extension_render_calls")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let extension_cache_hits = object
        .get("extension_cache_hits")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let extension_imperative_calls = object
        .get("extension_imperative_calls")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let extension_cache_misses = extension_render_calls.saturating_sub(extension_cache_hits);
    let extension_pressure = extension_render_calls > 0
        && extension_cache_misses.saturating_add(extension_imperative_calls) > 0;
    if !full_frame_fallback && !extension_pressure {
        return None;
    }

    let reason = match (full_frame_fallback, extension_pressure) {
        (true, true) => "full_frame_and_extension_pressure",
        (true, false) => "full_frame_fallback",
        (false, true) => "extension_cache_miss_or_imperative",
        (false, false) => return None,
    }
    .to_string();

    Some(PerfRenderOutlierSample {
        reason,
        frame_index: object
            .get("frame_index")
            .and_then(serde_json::Value::as_u64),
        since_attach_start_ms: object
            .get("since_attach_start_ms")
            .and_then(serde_json::Value::as_u64),
        frame_render_ms: object
            .get("frame_render_ms")
            .and_then(serde_json::Value::as_u64),
        frame_bytes: object
            .get("frame_bytes")
            .and_then(serde_json::Value::as_u64),
        full_frame_fallback,
        full_surface_fallbacks: object
            .get("full_surface_fallbacks")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        dirty_reasons: string_array_field(object, "dirty_reasons"),
        dirty_events: object
            .get("dirty_events")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default(),
        extension_stats: parse_extension_stats(object.get("extension_stats")),
    })
}

fn append_attach_window_counter_hints(hints: &mut Vec<String>, counters: &BTreeMap<String, u64>) {
    let render_frames = perf_counter(counters, "render_frames");
    if perf_counter(counters, "terminal_graphic_deletes") > 0 {
        hints.push("retained terminal graphics were deleted during captured attach windows; if this occurs outside tab/layout teardown, inspect graphic identity and lifecycle reconciliation".to_string());
    }
    if perf_counter(counters, "terminal_graphic_transmits") > 0
        && perf_counter(counters, "terminal_graphic_places")
            > render_frames.max(1).saturating_mul(2)
    {
        hints.push("terminal graphics placements greatly exceed rendered frames; inspect retained graphics placement stability and damage intersection checks".to_string());
    }
    if perf_counter(counters, "terminal_graphic_bytes") > 512 * 1024 {
        hints.push("terminal graphics uploaded more than 512KiB of raw pixels; prefer small retained sources and placement scaling for stable decorations".to_string());
    }
    if perf_counter(counters, "full_frame_fallbacks") > 0
        && perf_counter(counters, "damage_area_cells_max")
            < perf_counter(counters, "viewport_cells")
    {
        hints.push("full-frame fallback was observed without a matching full viewport damage area; inspect damage coalescing and dirty reason promotion".to_string());
    }
}

fn append_extension_counter_hints(
    hints: &mut Vec<String>,
    extension_counters: &BTreeMap<String, BTreeMap<String, u64>>,
) {
    let Some((extension_name, counters)) = extension_counters
        .iter()
        .max_by_key(|(_, counters)| perf_counter(counters, "render_calls"))
    else {
        return;
    };
    let render_calls = perf_counter(counters, "render_calls");
    if render_calls > 0 {
        let cache_hits = perf_counter(counters, "cache_hits");
        let imperative_calls = perf_counter(counters, "imperative_calls");
        let full_surface_calls = perf_counter(counters, "full_surface_calls");
        hints.push(format!(
            "top render extension by call count was {extension_name} (calls={render_calls}, cache_hits={cache_hits}, imperative={imperative_calls}, full_surface={full_surface_calls})"
        ));
    }
}

fn append_overrender_perf_hints(hints: &mut Vec<String>, counters: &BTreeMap<String, u64>) {
    if perf_counter(counters, "dirty_no_visible_row_change_frames") > 0 {
        hints.push("attach rendered dirty frames with no visible pane row changes; inspect dirty-source tracking for no-op invalidations".to_string());
    }
    if perf_counter(counters, "full_frame_fallback_flagged_frames") > 0 {
        hints.push("attach used full-frame damage fallbacks; inspect layout/resize/overlay churn before optimizing terminal throughput".to_string());
    }
    if perf_counter(counters, "extension_full_surface_excessive_frames") > 0 {
        hints.push("extension rendering frequently fell back to full-surface damage; prefer precise extension regions when possible".to_string());
    }
    if perf_counter(counters, "extension_imperative_or_cache_miss_frames") > 0 {
        hints.push("extension rendering had imperative fallback or cache-miss pressure; check render-op revisions and cacheability".to_string());
    }
    if perf_counter(counters, "status_overlay_only_emits_pane_work_frames") > 0 {
        hints.push("status/overlay-only frames emitted pane content; inspect damage classification for unnecessary scene redraws".to_string());
    }
    if perf_counter(counters, "slow_terminal_write_per_kib_frames") > 0 {
        hints.push("terminal writes were slow per KiB; host terminal throughput may be the bottleneck after render work is bounded".to_string());
    }
}

fn derive_perf_hints(
    report: &PerfAnalysisReport,
    recording_captures_custom: Option<bool>,
) -> Vec<String> {
    let mut hints = Vec::new();

    if report.perf_events == 0 {
        if matches!(recording_captures_custom, Some(false)) {
            hints.push(
                "recording did not capture `custom` events; perf telemetry requires `custom` event kind"
                    .to_string(),
            );
        } else {
            hints.push(
                "no bmux.perf events found; drive a telemetry-emitting path (for example real attach/runtime activity) and ensure `performance.recording_level` is enabled"
                    .to_string(),
            );
        }
        return hints;
    }

    if report.malformed_payloads > 0 {
        hints.push(format!(
            "{} perf payloads could not be parsed; check plugin/runtime payload compatibility",
            report.malformed_payloads
        ));
    }

    if report.dropped_events_reported > 0 || report.dropped_payload_bytes_reported > 0 {
        hints.push(format!(
            "perf telemetry was rate-limited (dropped events={}, dropped payload bytes={}); consider raising `performance.max_events_per_sec` or `performance.max_payload_bytes_per_sec`",
            report.dropped_events_reported, report.dropped_payload_bytes_reported
        ));
    }

    if let Some(connect_to_interactive_ms) = report.connect_to_interactive_ms
        && connect_to_interactive_ms > 1500
    {
        hints.push(format!(
            "connect-to-interactive took {connect_to_interactive_ms}ms; inspect iroh connect stages and attach hydration timing"
        ));
    }

    if let Some(max_outage_ms) = report.reconnect_outage_max_ms
        && max_outage_ms > 1000
    {
        hints.push(format!(
            "max reconnect outage was {max_outage_ms}ms; investigate network stability and relay path quality"
        ));
    }

    if let Some(render_max) = report.timings_ms.get("render_ms_max")
        && render_max.p95_ms > 16
    {
        hints.push(format!(
            "render p95 is {}ms (>16ms frame budget); local rendering may be a bottleneck",
            render_max.p95_ms
        ));
    }

    append_overrender_perf_hints(&mut hints, &report.overrender_counters);
    append_attach_window_counter_hints(&mut hints, &report.attach_window_counters);
    append_extension_counter_hints(&mut hints, &report.extension_counters);

    if let Some(drain_ipc_max) = report.timings_ms.get("drain_ipc_ms_max")
        && drain_ipc_max.p95_ms > 20
    {
        hints.push(format!(
            "drain IPC p95 is {}ms; server/client round-trip latency is likely impacting smoothness",
            drain_ipc_max.p95_ms
        ));
    }

    if hints.is_empty() {
        hints.push("no obvious bottleneck stood out from captured perf telemetry".to_string());
    }

    hints
}

#[allow(clippy::too_many_lines)] // Perf analysis intentionally combines parsing, aggregation, and correlation in one pass.
fn analyze_perf_events(
    events: &[RecordingEventEnvelope],
    recording_captures_custom: Option<bool>,
) -> PerfAnalysisReport {
    let mut report = PerfAnalysisReport {
        recording_events: events.len(),
        ..PerfAnalysisReport::default()
    };

    let mut timing_values: HashMap<String, Vec<u64>> = HashMap::new();
    let mut timing_samples = Vec::new();
    let mut first_connect_ts_epoch_ms = None;
    let mut first_attach_first_frame_ts_epoch_ms = None;
    let mut first_attach_interactive_ts_epoch_ms = None;
    let mut reconnect_outage_max_ms = None;

    for event in events {
        let RecordingPayload::Custom {
            source,
            name,
            payload,
        } = &event.payload
        else {
            continue;
        };

        if source != PERF_RECORDING_SOURCE {
            continue;
        }

        report.perf_events = report.perf_events.saturating_add(1);
        *report.by_event_name.entry(name.clone()).or_default() += 1;

        let decoded: serde_json::Value = if let Ok(value) = serde_json::from_slice(payload) {
            value
        } else {
            report.malformed_payloads = report.malformed_payloads.saturating_add(1);
            continue;
        };
        let Some(object) = decoded.as_object() else {
            report.malformed_payloads = report.malformed_payloads.saturating_add(1);
            continue;
        };

        if let Some(level) = object.get("level").and_then(serde_json::Value::as_str) {
            *report.by_level.entry(level.to_string()).or_default() += 1;
        }

        if let Some(ts_epoch_ms) = object
            .get("ts_epoch_ms")
            .and_then(serde_json::Value::as_u64)
        {
            report.first_ts_epoch_ms = Some(
                report
                    .first_ts_epoch_ms
                    .map_or(ts_epoch_ms, |first| first.min(ts_epoch_ms)),
            );
            report.last_ts_epoch_ms = Some(
                report
                    .last_ts_epoch_ms
                    .map_or(ts_epoch_ms, |last| last.max(ts_epoch_ms)),
            );
        }
        let ts_epoch_ms = object
            .get("ts_epoch_ms")
            .and_then(serde_json::Value::as_u64);

        if name == "attach.window" {
            let extension_stats = parse_extension_stats(object.get("extension_stats"));
            aggregate_extension_counters(&mut report.extension_counters, &extension_stats);
            for (field, value) in object {
                if matches!(field.as_str(), "schema_version" | "ts_epoch_ms")
                    || field.ends_with("_ms")
                {
                    continue;
                }
                if let Some(counter) = value.as_u64() {
                    *report
                        .attach_window_counters
                        .entry(field.clone())
                        .or_default() += counter;
                }
            }
            for field in [
                "overrender_flagged_frames",
                "dirty_no_visible_row_change_frames",
                "high_cached_skip_ratio_frames",
                "large_partial_frame_frames",
                "extension_full_surface_excessive_frames",
                "full_frame_fallback_flagged_frames",
                "extension_imperative_or_cache_miss_frames",
                "status_overlay_only_emits_pane_work_frames",
                "slow_terminal_write_per_kib_frames",
            ] {
                let value = object
                    .get(field)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                *report
                    .overrender_counters
                    .entry(field.to_string())
                    .or_default() += value;
            }
        }

        if name == "attach.frame.trace"
            && let Some(outlier) = render_outlier_from_frame_trace(object)
        {
            report.render_outliers.push(outlier);
        }

        if name == "iroh.connect.summary" && first_connect_ts_epoch_ms.is_none() {
            first_connect_ts_epoch_ms = ts_epoch_ms;
        }
        if name == "attach.first_frame" && first_attach_first_frame_ts_epoch_ms.is_none() {
            first_attach_first_frame_ts_epoch_ms = ts_epoch_ms;
        }
        if name == "attach.interactive.ready" && first_attach_interactive_ts_epoch_ms.is_none() {
            first_attach_interactive_ts_epoch_ms = ts_epoch_ms;
        }
        if name == "iroh.reconnect.outage"
            && let Some(outage_ms) = object.get("outage_ms").and_then(serde_json::Value::as_u64)
        {
            reconnect_outage_max_ms = Some(
                reconnect_outage_max_ms.map_or(outage_ms, |current: u64| current.max(outage_ms)),
            );
        }

        report.dropped_events_reported = report.dropped_events_reported.saturating_add(
            object
                .get("dropped_events_since_emit")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        );
        report.dropped_payload_bytes_reported =
            report.dropped_payload_bytes_reported.saturating_add(
                object
                    .get("dropped_payload_bytes_since_emit")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            );

        for (field, value) in object {
            if !field.ends_with("_ms") && !field.contains("_ms_") {
                continue;
            }
            let Some(ms) = value.as_u64() else {
                continue;
            };
            timing_values.entry(field.clone()).or_default().push(ms);
            timing_samples.push(PerfTimingSample {
                event_name: name.clone(),
                metric: field.clone(),
                value_ms: ms,
                ts_epoch_ms,
            });
        }
    }

    if let (Some(first), Some(last)) = (report.first_ts_epoch_ms, report.last_ts_epoch_ms) {
        report.span_ms = Some(last.saturating_sub(first));
    }

    report.timings_ms = timing_values
        .into_iter()
        .map(|(field, values)| (field, timing_summary_from_values(&values)))
        .collect();

    let p95_by_metric = report
        .timings_ms
        .iter()
        .map(|(metric, summary)| (metric.clone(), summary.p95_ms))
        .collect::<HashMap<_, _>>();

    report.outlier_samples = timing_samples
        .into_iter()
        .filter_map(|sample| {
            let p95_ms = p95_by_metric.get(&sample.metric).copied()?;
            if p95_ms == 0 || sample.value_ms < p95_ms {
                return None;
            }
            Some(PerfOutlierSample {
                event_name: sample.event_name,
                metric: sample.metric,
                value_ms: sample.value_ms,
                p95_ms,
                ts_epoch_ms: sample.ts_epoch_ms,
            })
        })
        .collect();
    report
        .outlier_samples
        .sort_by_key(|sample| std::cmp::Reverse(sample.value_ms));
    report.outlier_samples.truncate(20);
    report.render_outliers.sort_by(|left, right| {
        right
            .frame_bytes
            .unwrap_or(0)
            .cmp(&left.frame_bytes.unwrap_or(0))
            .then_with(|| left.frame_index.cmp(&right.frame_index))
    });
    report.render_outliers.truncate(20);

    if let (Some(connect_ts_epoch_ms), Some(first_frame_ts_epoch_ms)) = (
        first_connect_ts_epoch_ms,
        first_attach_first_frame_ts_epoch_ms,
    ) && first_frame_ts_epoch_ms >= connect_ts_epoch_ms
    {
        report.connect_to_first_frame_ms = Some(first_frame_ts_epoch_ms - connect_ts_epoch_ms);
    }
    if let (Some(connect_ts_epoch_ms), Some(interactive_ts_epoch_ms)) = (
        first_connect_ts_epoch_ms,
        first_attach_interactive_ts_epoch_ms,
    ) && interactive_ts_epoch_ms >= connect_ts_epoch_ms
    {
        report.connect_to_interactive_ms = Some(interactive_ts_epoch_ms - connect_ts_epoch_ms);
    }
    report.reconnect_outage_max_ms = reconnect_outage_max_ms.or_else(|| {
        report
            .timings_ms
            .get("outage_ms")
            .map(|timing| timing.max_ms)
    });

    report.hints = derive_perf_hints(&report, recording_captures_custom);

    report
}

fn print_nonzero_perf_counters(label: &str, counters: &BTreeMap<String, u64>) {
    if counters.is_empty() || !counters.values().any(|value| *value > 0) {
        return;
    }
    println!("{label}:");
    for (name, value) in counters {
        if *value > 0 {
            println!("  {name}: {value}");
        }
    }
}

fn print_extension_counters(counters: &BTreeMap<String, BTreeMap<String, u64>>) {
    if counters.is_empty() {
        return;
    }
    println!("extension counters:");
    for (extension_name, extension_counters) in counters {
        let summary = extension_counters
            .iter()
            .filter(|(_, value)| **value > 0)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(", ");
        if !summary.is_empty() {
            println!("  {extension_name}: {summary}");
        }
    }
}

fn print_render_outliers(outliers: &[PerfRenderOutlierSample]) {
    if outliers.is_empty() {
        return;
    }
    println!("render outliers:");
    for outlier in outliers.iter().take(10) {
        println!(
            "  frame={} reason={} bytes={} render_ms={} full_frame={} dirty={}",
            outlier
                .frame_index
                .map_or_else(|| "?".to_string(), |value| value.to_string()),
            outlier.reason,
            outlier
                .frame_bytes
                .map_or_else(|| "?".to_string(), |value| value.to_string()),
            outlier
                .frame_render_ms
                .map_or_else(|| "?".to_string(), |value| value.to_string()),
            outlier.full_frame_fallback,
            outlier.dirty_reasons.join("+")
        );
    }
}

fn print_perf_analysis_text(report: &PerfAnalysisReport) {
    if report.perf_events == 0 {
        println!("no bmux.perf custom events found in recording");
        return;
    }

    println!(
        "perf events: {} / {} (malformed payloads: {})",
        report.perf_events, report.recording_events, report.malformed_payloads
    );
    if let Some(span_ms) = report.span_ms {
        println!("time span: {span_ms}ms");
    }
    if report.dropped_events_reported > 0 || report.dropped_payload_bytes_reported > 0 {
        println!(
            "reported drops: events={} payload_bytes={}",
            report.dropped_events_reported, report.dropped_payload_bytes_reported
        );
    }

    if !report.by_level.is_empty() {
        let levels = report
            .by_level
            .iter()
            .map(|(level, count)| format!("{level}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("levels: {levels}");
    }

    if !report.by_event_name.is_empty() {
        println!("events:");
        let mut entries = report.by_event_name.iter().collect::<Vec<_>>();
        entries.sort_by(|(left_name, left_count), (right_name, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| left_name.cmp(right_name))
        });
        for (name, count) in entries.into_iter().take(12) {
            println!("  {name}: {count}");
        }
    }

    print_nonzero_perf_counters("attach window counters", &report.attach_window_counters);
    print_nonzero_perf_counters("render inefficiency counters", &report.overrender_counters);
    print_extension_counters(&report.extension_counters);

    if !report.timings_ms.is_empty() {
        println!("timings (ms):");
        let mut timings = report.timings_ms.iter().collect::<Vec<_>>();
        timings.sort_by(|(left_name, left), (right_name, right)| {
            right
                .p95_ms
                .cmp(&left.p95_ms)
                .then_with(|| left_name.cmp(right_name))
        });
        for (name, timing) in timings.into_iter().take(16) {
            println!(
                "  {name}: count={} min={} p50={} p95={} p99={} avg={} max={}",
                timing.count,
                timing.min_ms,
                timing.p50_ms,
                timing.p95_ms,
                timing.p99_ms,
                timing.avg_ms,
                timing.max_ms
            );
        }
    }

    if let Some(connect_to_first_frame_ms) = report.connect_to_first_frame_ms {
        println!("connect to first frame: {connect_to_first_frame_ms}ms");
    }
    if let Some(connect_to_interactive_ms) = report.connect_to_interactive_ms {
        println!("connect to interactive: {connect_to_interactive_ms}ms");
    }
    if let Some(reconnect_outage_max_ms) = report.reconnect_outage_max_ms {
        println!("max reconnect outage: {reconnect_outage_max_ms}ms");
    }

    if !report.outlier_samples.is_empty() {
        println!("timing outliers:");
        for outlier in report.outlier_samples.iter().take(10) {
            if let Some(ts_epoch_ms) = outlier.ts_epoch_ms {
                println!(
                    "  {}: value={}ms p95={}ms ts={} event={}",
                    outlier.metric,
                    outlier.value_ms,
                    outlier.p95_ms,
                    ts_epoch_ms,
                    outlier.event_name
                );
            } else {
                println!(
                    "  {}: value={}ms p95={}ms event={}",
                    outlier.metric, outlier.value_ms, outlier.p95_ms, outlier.event_name
                );
            }
        }
    }

    print_render_outliers(&report.render_outliers);

    if !report.hints.is_empty() {
        println!("hints:");
        for hint in &report.hints {
            println!("  - {hint}");
        }
    }
}

fn resolve_recording_summary(recording_id: &str) -> Result<RecordingSummary> {
    let recordings = list_recordings_from_disk()?;
    let id = resolve_recording_id_prefix(recording_id, &recordings)?;
    recordings
        .into_iter()
        .find(|recording| recording.id == id)
        .ok_or_else(|| anyhow::anyhow!("recording '{recording_id}' not found after resolving id"))
}

pub(super) fn run_recording_analyze(recording_id: &str, perf: bool, as_json: bool) -> Result<u8> {
    if !perf {
        anyhow::bail!("recording analyze currently supports only --perf")
    }

    let recording_summary = resolve_recording_summary(recording_id)?;
    let events = load_recording_events(recording_id)?;

    let report = analyze_perf_events(
        &events,
        Some(event_kinds_include_custom(&recording_summary.event_kinds)),
    );
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .context("failed encoding recording analyze json")?
        );
        return Ok(0);
    }

    print_perf_analysis_text(&report);
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_recording_replay(
    recording_id: &str,
    mode: RecordingReplayMode,
    speed: f64,
    target_bmux: Option<&str>,
    compare_recording: Option<&str>,
    ignore: Option<&str>,
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
) -> Result<u8> {
    let events = load_recording_events(recording_id)?;
    match mode {
        RecordingReplayMode::Watch => super::replay_watch(&events, speed),
        RecordingReplayMode::Interactive => super::replay_interactive(&events, speed),
        RecordingReplayMode::Verify => {
            super::replay_verify(
                &events,
                target_bmux,
                compare_recording,
                ignore,
                strict_timing,
                max_verify_duration_secs,
                verify_start_timeout_secs,
            )
            .await
        }
    }
}

pub(super) async fn run_recording_verify_smoke(
    recording_id: &str,
    target_bmux: Option<&str>,
    compare_recording: Option<&str>,
    ignore: Option<&str>,
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
) -> Result<u8> {
    let events = load_recording_events(recording_id)?;
    let report = super::verify_recording_report(
        &events,
        target_bmux,
        compare_recording,
        ignore,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
    )
    .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report)
            .context("failed encoding verify smoke report json")?
    );
    Ok(u8::from(!report.pass))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorVisualShape {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy)]
struct CursorReplayState {
    shape: CursorVisualShape,
    blink_enabled: bool,
}

impl Default for CursorReplayState {
    fn default() -> Self {
        Self {
            shape: CursorVisualShape::Block,
            blink_enabled: true,
        }
    }
}

fn update_cursor_replay_state(state: &mut CursorReplayState, data: &[u8]) {
    let mut index = 0usize;
    while index + 4 < data.len() {
        if data[index] != 0x1b || data[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let mut cursor = index + 2;
        let mut value: u16 = 0;
        let mut saw_digit = false;
        while cursor < data.len() && data[cursor].is_ascii_digit() {
            saw_digit = true;
            value = value
                .saturating_mul(10)
                .saturating_add(u16::from(data[cursor].saturating_sub(b'0')));
            cursor += 1;
        }
        if cursor + 1 >= data.len() || data[cursor] != b' ' || data[cursor + 1] != b'q' {
            index += 1;
            continue;
        }
        let ps = if saw_digit { value } else { 0 };
        match ps {
            0 | 1 => {
                state.shape = CursorVisualShape::Block;
                state.blink_enabled = true;
            }
            2 => {
                state.shape = CursorVisualShape::Block;
                state.blink_enabled = false;
            }
            3 => {
                state.shape = CursorVisualShape::Underline;
                state.blink_enabled = true;
            }
            4 => {
                state.shape = CursorVisualShape::Underline;
                state.blink_enabled = false;
            }
            5 => {
                state.shape = CursorVisualShape::Bar;
                state.blink_enabled = true;
            }
            6 => {
                state.shape = CursorVisualShape::Bar;
                state.blink_enabled = false;
            }
            _ => {}
        }
        index = cursor + 2;
    }
}

const fn display_cursor_shape_from_visual(shape: CursorVisualShape) -> DisplayCursorShape {
    match shape {
        CursorVisualShape::Block => DisplayCursorShape::Block,
        CursorVisualShape::Bar => DisplayCursorShape::Bar,
        CursorVisualShape::Underline => DisplayCursorShape::Underline,
    }
}

#[derive(Clone, Copy, Debug)]
struct CellMetrics {
    width: u16,
    height: u16,
}

fn infer_cell_metrics(
    window_width: u16,
    window_height: u16,
    cols: u16,
    rows: u16,
) -> Option<CellMetrics> {
    if window_width == 0 || window_height == 0 || cols == 0 || rows == 0 {
        return None;
    }
    let width = (window_width / cols).max(1);
    let height = (window_height / rows).max(1);
    Some(CellMetrics { width, height })
}

fn capture_stream_open_metrics() -> (Option<u16>, Option<u16>, Option<u16>, Option<u16>) {
    let (window_width_px, window_height_px) =
        terminal::window_size().ok().map_or((None, None), |value| {
            (
                (value.width > 0).then_some(value.width),
                (value.height > 0).then_some(value.height),
            )
        });

    let (cell_width_px, cell_height_px) = terminal::size()
        .ok()
        .and_then(|(cols, rows)| {
            let window_width = window_width_px?;
            let window_height = window_height_px?;
            infer_cell_metrics(window_width, window_height, cols, rows)
        })
        .map_or((None, None), |value| {
            (Some(value.width), Some(value.height))
        });

    (
        cell_width_px,
        cell_height_px,
        window_width_px,
        window_height_px,
    )
}

pub(super) fn parse_ignore_rules(ignore: Option<&str>) -> Vec<String> {
    ignore
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub(super) fn apply_ignore_rules(
    events: &[RecordingEventEnvelope],
    ignore_rules: &[String],
) -> Vec<RecordingEventEnvelope> {
    if ignore_rules.is_empty() {
        return events.to_vec();
    }
    events
        .iter()
        .filter(|event| {
            let name = recording_event_kind_name(event.kind);
            !ignore_rules.contains(&name)
        })
        .cloned()
        .collect()
}

pub(super) fn recording_event_kind_name(kind: RecordingEventKind) -> String {
    match kind {
        RecordingEventKind::PaneInputRaw => "pane_input_raw",
        RecordingEventKind::PaneOutputRaw => "pane_output_raw",
        RecordingEventKind::ProtocolReplyRaw => "protocol_reply_raw",
        RecordingEventKind::PaneImage => "pane_image",
        RecordingEventKind::ServerEvent => "server_event",
        RecordingEventKind::RequestStart => "request_start",
        RecordingEventKind::RequestDone => "request_done",
        RecordingEventKind::RequestError => "request_error",
        RecordingEventKind::Custom => "custom",
    }
    .to_string()
}

pub(super) fn load_recording_events(recording_id: &str) -> Result<Vec<RecordingEventEnvelope>> {
    let recordings = list_recordings_from_disk()?;
    let id = resolve_recording_id_prefix(recording_id, &recordings)?;
    let recording_dir = recordings_root_dir().join(id.to_string());
    let manifest_path = recording_dir.join("manifest.json");

    // Read manifest to discover segment files.
    let segments = if manifest_path.exists() {
        let manifest_bytes = std::fs::read(&manifest_path)
            .with_context(|| format!("failed reading manifest {}", manifest_path.display()))?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
        manifest["summary"]["segments"].as_array().map_or_else(
            || vec!["events_0.bin".to_string()],
            |arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            },
        )
    } else {
        // Fallback: try legacy single-file format.
        vec!["events.bin".to_string()]
    };

    let mut all_frames = Vec::new();
    for segment_name in &segments {
        let segment_path = recording_dir.join(segment_name);
        if !segment_path.exists() {
            tracing::warn!(
                "recording {id}: segment file {} not found, skipping",
                segment_path.display()
            );
            continue;
        }
        let bytes = std::fs::read(&segment_path).with_context(|| {
            format!(
                "failed reading recording segment {}",
                segment_path.display()
            )
        })?;
        let result = read_frames(&bytes).map_err(|e| {
            anyhow::anyhow!(
                "failed parsing recording segment {}: {e}",
                segment_path.display()
            )
        })?;
        if result.bytes_remaining > 0 {
            tracing::warn!(
                "recording {id}: segment {} has {} trailing bytes (truncated?)",
                segment_name,
                result.bytes_remaining
            );
        }
        all_frames.extend(result.frames);
    }

    Ok(all_frames)
}

pub(super) fn resolve_recording_id_prefix(
    value: &str,
    recordings: &[RecordingSummary],
) -> Result<Uuid> {
    let query = value.trim();
    if query.is_empty() {
        anyhow::bail!("recording id/name cannot be empty");
    }

    if let Ok(id) = Uuid::parse_str(query)
        && recordings.iter().any(|recording| recording.id == id)
    {
        return Ok(id);
    }

    let exact_name_matches = recordings
        .iter()
        .filter_map(|recording| {
            recording
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(query))
                .then_some(recording.id)
        })
        .collect::<Vec<_>>();

    match exact_name_matches.as_slice() {
        [id] => return Ok(*id),
        [] => {}
        _ => {
            let mut options = exact_name_matches
                .iter()
                .filter_map(|id| recordings.iter().find(|recording| recording.id == *id))
                .map(recording_selection_label)
                .collect::<Vec<_>>();
            options.sort();
            anyhow::bail!(
                "recording name '{query}' is ambiguous; matches: {}",
                options.join(", ")
            )
        }
    }

    let normalized = query.to_ascii_lowercase();
    let mut seen = HashSet::new();
    let matches = recordings
        .iter()
        .filter_map(|recording| {
            let id_match = recording.id.to_string().starts_with(&normalized);
            let name_match = recording
                .name
                .as_ref()
                .is_some_and(|name| name.to_ascii_lowercase().starts_with(&normalized));
            (id_match || name_match)
                .then_some(recording.id)
                .filter(|id| seen.insert(*id))
        })
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [id] => Ok(*id),
        [] => anyhow::bail!("no recording matches id/name '{value}'"),
        _ => {
            let mut options = matches
                .iter()
                .filter_map(|id| recordings.iter().find(|recording| recording.id == *id))
                .map(recording_selection_label)
                .collect::<Vec<_>>();
            options.sort();
            anyhow::bail!(
                "recording id/name '{value}' is ambiguous; matches: {}",
                options.join(", ")
            )
        }
    }
}

fn recording_selection_label(recording: &RecordingSummary) -> String {
    recording.name.as_ref().map_or_else(
        || recording.id.to_string(),
        |name| format!("{} (name={name})", recording.id),
    )
}

pub(super) fn delete_recording_dir(recording_id: Uuid) -> Result<()> {
    delete_recording_dir_at(&recordings_root_dir(), recording_id)
}

pub(super) fn delete_recording_dir_at(recordings_root: &Path, recording_id: Uuid) -> Result<()> {
    let dir = recordings_root.join(recording_id.to_string());
    let manifest = dir.join("manifest.json");
    if !manifest.exists() {
        anyhow::bail!("recording not found: {recording_id}");
    }
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("failed removing recording directory {}", dir.display()))?;
    Ok(())
}

pub(super) fn delete_all_recordings_from_disk() -> Result<usize> {
    delete_all_recordings_from_dir(&recordings_root_dir())
}

pub(super) fn delete_all_recordings_from_dir(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }

    let mut deleted_count = 0_usize;
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("failed reading recordings dir {}", root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let manifest = entry.path().join("manifest.json");
        if !manifest.exists() {
            continue;
        }
        std::fs::remove_dir_all(entry.path()).with_context(|| {
            format!(
                "failed removing recording directory {}",
                entry.path().display()
            )
        })?;
        deleted_count = deleted_count.saturating_add(1);
    }
    Ok(deleted_count)
}

pub(super) fn confirm_delete_all_recordings(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        anyhow::bail!("recording delete-all requires --yes in non-interactive mode");
    }

    println!("Delete all recordings? [y/N]");
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed reading delete-all confirmation")?;
    let trimmed = answer.trim().to_ascii_lowercase();
    Ok(trimmed == "y" || trimmed == "yes")
}

#[derive(Debug, serde::Deserialize)]
struct RecordingManifest {
    summary: RecordingSummary,
}

fn read_recording_manifest(manifest_path: &Path) -> Result<RecordingSummary> {
    let bytes = std::fs::read(manifest_path).with_context(|| {
        format!(
            "failed reading recording manifest {}",
            manifest_path.display()
        )
    })?;
    let manifest: RecordingManifest = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed parsing recording manifest {}",
            manifest_path.display()
        )
    })?;
    Ok(manifest.summary)
}

pub(super) fn list_recordings_from_disk() -> Result<Vec<RecordingSummary>> {
    list_recordings_from_dir(&recordings_root_dir())
}

pub(super) fn recordings_root_dir() -> PathBuf {
    let (_config, root) = recording_config_and_root();
    root
}

pub(super) fn list_recordings_from_dir(recordings_root: &Path) -> Result<Vec<RecordingSummary>> {
    if !recordings_root.exists() {
        return Ok(Vec::new());
    }

    let mut recordings = Vec::new();
    for entry in std::fs::read_dir(recordings_root).with_context(|| {
        format!(
            "failed reading recordings dir {}",
            recordings_root.display()
        )
    })? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        if let Ok(summary) = read_recording_manifest(&manifest_path) {
            recordings.push(summary);
        }
    }

    recordings.sort_by_key(|recording| std::cmp::Reverse(recording.started_epoch_ms));
    Ok(recordings)
}

pub(super) const fn offline_recording_status() -> RecordingStatus {
    RecordingStatus {
        active: None,
        queue_len: 0,
    }
}

// Display track types are defined in bmux_ipc for cross-module sharing.

const DISPLAY_CAPTURE_QUEUE_CAPACITY: usize = 4096;
const DISPLAY_CAPTURE_SEGMENT_MAX_AGE: Duration = Duration::from_secs(2);
const DISPLAY_CAPTURE_PRUNE_GRACE: Duration = Duration::from_secs(5);
pub(super) struct DisplayCaptureWriter {
    sender: mpsc::SyncSender<DisplayCaptureCommand>,
    worker: Option<thread::JoinHandle<()>>,
    dropped_events: u64,
}

enum DisplayCaptureCommand {
    Event(DisplayTrackEvent),
    CursorSnapshot(Option<crate::runtime::attach::state::AttachCursorState>),
    Flush(mpsc::Sender<Result<()>>),
    Close(mpsc::Sender<Result<()>>),
}

struct DisplayCaptureFileWriter {
    recording_path: PathBuf,
    client_id: Uuid,
    rolling_window: Option<Duration>,
    started_at: Instant,
    writer: BufWriter<std::fs::File>,
    segment_index: u64,
    segment_start_ns: u64,
    closed_segments: VecDeque<(PathBuf, u64)>,
    stream_opened_baseline: DisplayTrackEvent,
    latest_resize: Option<(u16, u16)>,
    replay_grid: bmux_terminal_grid::TerminalGridStream,
    cursor_replay_state: CursorReplayState,
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    last_image_count: usize,
}

impl DisplayCaptureWriter {
    /// Create a display capture writer backed by a dedicated OS thread.  The
    /// attach loop only enqueues events; disk writes, rotation, and pruning are
    /// kept off the interactive hot path.
    pub(super) fn open(
        recording_id: Uuid,
        recording_path: &Path,
        client_id: Uuid,
        rolling_window_secs: Option<u64>,
    ) -> Result<Self> {
        let mut writer = DisplayCaptureFileWriter::open(
            recording_id,
            recording_path,
            client_id,
            rolling_window_secs.map(Duration::from_secs),
        )?;
        writer.record_stream_opened()?;
        let (sender, receiver) = mpsc::sync_channel(DISPLAY_CAPTURE_QUEUE_CAPACITY);
        let worker = thread::Builder::new()
            .name(format!("bmux-display-capture-{recording_id}"))
            .spawn(move || display_capture_writer_loop(&mut writer, receiver))
            .context("failed spawning display capture writer thread")?;
        Ok(Self {
            sender,
            worker: Some(worker),
            dropped_events: 0,
        })
    }

    pub(super) fn record_resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.enqueue(DisplayCaptureCommand::Event(DisplayTrackEvent::Resize {
            cols,
            rows,
        }))
    }

    pub(super) fn record_frame_bytes(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.enqueue(DisplayCaptureCommand::Event(
            DisplayTrackEvent::FrameBytes {
                data: data.to_vec(),
            },
        ))
    }

    pub(super) fn record_activity(&mut self, kind: DisplayActivityKind) -> Result<()> {
        self.enqueue(DisplayCaptureCommand::Event(DisplayTrackEvent::Activity {
            kind,
        }))
    }

    pub(super) fn record_cursor_snapshot(
        &mut self,
        cursor_state: Option<crate::runtime::attach::state::AttachCursorState>,
    ) -> Result<()> {
        self.enqueue(DisplayCaptureCommand::CursorSnapshot(cursor_state))
    }

    pub(super) fn record_stream_closed(&mut self) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.sender
            .send(DisplayCaptureCommand::Close(sender))
            .context("display capture writer is closed")?;
        let result = receiver
            .recv()
            .context("display capture writer closed without acknowledgement")?;
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            return Err(anyhow::anyhow!("display capture writer thread panicked"));
        }
        result
    }

    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub(super) fn record_images(
        &mut self,
        images: &[bmux_attach_image_protocol::AttachPaneImage],
    ) -> Result<()> {
        self.enqueue(DisplayCaptureCommand::Event(
            DisplayTrackEvent::ImageUpdate {
                images: images.to_vec(),
            },
        ))
    }

    pub(super) fn flush(&self) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.sender
            .send(DisplayCaptureCommand::Flush(sender))
            .context("display capture writer is closed")?;
        receiver
            .recv()
            .context("display capture writer closed without flushing")?
    }

    fn enqueue(&mut self, command: DisplayCaptureCommand) -> Result<()> {
        match self.sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                self.dropped_events = self.dropped_events.saturating_add(1);
                if self.dropped_events == 1 || self.dropped_events.is_multiple_of(1024) {
                    tracing::warn!(
                        dropped_events = self.dropped_events,
                        "display capture queue is full; dropping recording display events"
                    );
                }
                Ok(())
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err(anyhow::anyhow!("display capture writer is closed"))
            }
        }
    }
}

impl Drop for DisplayCaptureWriter {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.record_stream_closed();
        }
    }
}

impl DisplayCaptureFileWriter {
    fn open(
        recording_id: Uuid,
        recording_path: &Path,
        client_id: Uuid,
        rolling_window: Option<Duration>,
    ) -> Result<Self> {
        std::fs::create_dir_all(recording_path).with_context(|| {
            format!(
                "failed creating recording path {}",
                recording_path.display()
            )
        })?;
        let display_track_path =
            display_track_output_path(recording_path, client_id, 0, rolling_window);
        let file = open_display_track_file(&display_track_path)?;
        let stream_opened_baseline = capture_stream_opened_event(recording_id, client_id);
        let latest_resize = current_terminal_size();
        let (initial_cols, initial_rows) = latest_resize.unwrap_or((80, 24));
        let replay_grid = bmux_terminal_grid::TerminalGridStream::new(
            initial_cols.max(1),
            initial_rows.max(1),
            bmux_terminal_grid::GridLimits::default(),
        )
        .expect("display capture replay grid dimensions are valid");
        Ok(Self {
            recording_path: recording_path.to_path_buf(),
            client_id,
            rolling_window,
            started_at: Instant::now(),
            writer: BufWriter::new(file),
            segment_index: 0,
            segment_start_ns: 0,
            closed_segments: VecDeque::new(),
            stream_opened_baseline,
            latest_resize,
            replay_grid,
            cursor_replay_state: CursorReplayState::default(),
            #[cfg(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            ))]
            last_image_count: 0,
        })
    }

    fn record_stream_opened(&mut self) -> Result<()> {
        self.record_segment_baseline()
    }

    fn record_segment_baseline(&mut self) -> Result<()> {
        self.record(self.stream_opened_baseline.clone())?;
        if let Some((cols, rows)) = self.latest_resize {
            self.record(DisplayTrackEvent::Resize { cols, rows })?;
        }
        let repaint = bmux_terminal_grid::full_screen_repaint_bytes(self.replay_grid.grid());
        if !repaint.is_empty() {
            self.record(DisplayTrackEvent::FrameBytes { data: repaint })?;
        }
        Ok(())
    }

    fn record_cursor_snapshot(
        &mut self,
        cursor_state: Option<crate::runtime::attach::state::AttachCursorState>,
    ) -> Result<()> {
        let (x, y, visible) =
            cursor_state.map_or((0, 0, false), |state| (state.x, state.y, state.visible));
        self.record(DisplayTrackEvent::CursorSnapshot {
            x,
            y,
            visible,
            shape: display_cursor_shape_from_visual(self.cursor_replay_state.shape),
            blink_enabled: self.cursor_replay_state.blink_enabled,
        })
    }

    fn record(&mut self, event: DisplayTrackEvent) -> Result<()> {
        if let DisplayTrackEvent::Resize { cols, rows } = &event
            && *cols > 0
            && *rows > 0
        {
            self.latest_resize = Some((*cols, *rows));
            let _ = self.replay_grid.resize(*cols, *rows);
        }
        if let DisplayTrackEvent::FrameBytes { data } = &event {
            update_cursor_replay_state(&mut self.cursor_replay_state, data);
            self.replay_grid.process(data);
        }
        #[cfg(any(
            feature = "image-sixel",
            feature = "image-kitty",
            feature = "image-iterm2"
        ))]
        if let DisplayTrackEvent::ImageUpdate { images } = &event {
            let count = images.len();
            if count == 0 && self.last_image_count == 0 {
                return Ok(());
            }
            self.last_image_count = count;
        }
        let mono_ns = u64::try_from(
            self.started_at
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)),
        )
        .unwrap_or(u64::MAX);
        let envelope = DisplayTrackEnvelope { mono_ns, event };
        write_frame(&mut self.writer, &envelope)
            .map_err(|e| anyhow::anyhow!("display track write_frame failed: {e}"))?;
        self.maybe_rotate(mono_ns)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.writer
            .flush()
            .context("failed flushing display capture writer")
    }

    fn maybe_rotate(&mut self, mono_ns: u64) -> Result<()> {
        if self.rolling_window.is_none() {
            return Ok(());
        }
        let segment_age = Duration::from_nanos(mono_ns.saturating_sub(self.segment_start_ns));
        if segment_age < DISPLAY_CAPTURE_SEGMENT_MAX_AGE {
            return Ok(());
        }
        self.rotate(mono_ns)
    }

    fn rotate(&mut self, end_ns: u64) -> Result<()> {
        self.flush()?;
        let old_path =
            display_track_segment_path(&self.recording_path, self.client_id, self.segment_index);
        self.closed_segments.push_back((old_path, end_ns));
        self.segment_index = self.segment_index.saturating_add(1);
        self.segment_start_ns = end_ns;
        let new_path =
            display_track_segment_path(&self.recording_path, self.client_id, self.segment_index);
        self.writer = BufWriter::new(open_display_track_file(&new_path)?);
        self.record_segment_baseline()?;
        self.prune_closed_segments(end_ns)
    }

    fn prune_closed_segments(&mut self, now_ns: u64) -> Result<()> {
        let Some(window) = self.rolling_window else {
            return Ok(());
        };
        let retention = window.saturating_add(DISPLAY_CAPTURE_PRUNE_GRACE);
        let cutoff_ns = now_ns.saturating_sub(duration_nanos_u64(retention));
        while self
            .closed_segments
            .front()
            .is_some_and(|(_, end_ns)| *end_ns < cutoff_ns)
        {
            let Some((path, _)) = self.closed_segments.pop_front() else {
                break;
            };
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error).with_context(|| {
                    format!(
                        "failed removing old display track segment {}",
                        path.display()
                    )
                });
            }
        }
        Ok(())
    }
}

fn display_capture_writer_loop(
    writer: &mut DisplayCaptureFileWriter,
    receiver: mpsc::Receiver<DisplayCaptureCommand>,
) {
    for command in receiver {
        match command {
            DisplayCaptureCommand::Event(event) => {
                if let Err(error) = writer.record(event) {
                    tracing::warn!(error = %error, "display capture write failed");
                }
            }
            DisplayCaptureCommand::CursorSnapshot(cursor_state) => {
                if let Err(error) = writer.record_cursor_snapshot(cursor_state) {
                    tracing::warn!(error = %error, "display capture cursor snapshot failed");
                }
            }
            DisplayCaptureCommand::Flush(ack) => {
                let _ = ack.send(writer.flush());
            }
            DisplayCaptureCommand::Close(ack) => {
                let result = writer
                    .record(DisplayTrackEvent::StreamClosed)
                    .and_then(|()| writer.flush());
                let _ = ack.send(result);
                break;
            }
        }
    }
}

fn capture_stream_opened_event(recording_id: Uuid, client_id: Uuid) -> DisplayTrackEvent {
    let (cell_width_px, cell_height_px, window_width_px, window_height_px) =
        capture_stream_open_metrics();
    let terminal_profile = terminal_profile::detect_render_profile();
    let terminal_profile_bytes = terminal_profile
        .as_ref()
        .and_then(|p| bmux_ipc::encode(p).ok());
    DisplayTrackEvent::StreamOpened {
        client_id,
        recording_id,
        cell_width_px,
        cell_height_px,
        window_width_px,
        window_height_px,
        terminal_profile: terminal_profile_bytes,
    }
}

fn current_terminal_size() -> Option<(u16, u16)> {
    let Ok((cols, rows)) = terminal::size() else {
        return None;
    };
    (cols > 0 && rows > 0).then_some((cols, rows))
}

fn open_display_track_file(path: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed opening display track {}", path.display()))
}

fn duration_nanos_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos().min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

fn display_track_path(recording_path: &Path, client_id: Uuid) -> PathBuf {
    recording_path.join(format!("display-{client_id}.bin"))
}

fn display_track_segment_path(recording_path: &Path, client_id: Uuid, index: u64) -> PathBuf {
    recording_path.join(format!("display-{client_id}.part{index}.bin"))
}

fn display_track_output_path(
    recording_path: &Path,
    client_id: Uuid,
    index: u64,
    rolling_window: Option<Duration>,
) -> PathBuf {
    if rolling_window.is_some() {
        display_track_segment_path(recording_path, client_id, index)
    } else {
        display_track_path(recording_path, client_id)
    }
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[test]
    fn display_track_envelope_round_trips_through_codec() {
        let envelope = DisplayTrackEnvelope {
            mono_ns: 1,
            event: DisplayTrackEvent::StreamOpened {
                client_id: Uuid::nil(),
                recording_id: Uuid::nil(),
                cell_width_px: Some(8),
                cell_height_px: Some(16),
                window_width_px: Some(640),
                window_height_px: Some(480),
                terminal_profile: None,
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &envelope).expect("write should succeed");
        let result = read_frames::<DisplayTrackEnvelope>(&buf).expect("read should succeed");
        assert_eq!(result.bytes_remaining, 0);
        assert_eq!(result.frames.len(), 1);
        assert_eq!(result.frames[0].mono_ns, 1);
        match &result.frames[0].event {
            DisplayTrackEvent::StreamOpened {
                cell_width_px,
                cell_height_px,
                ..
            } => {
                assert_eq!(*cell_width_px, Some(8));
                assert_eq!(*cell_height_px, Some(16));
            }
            _ => panic!("expected stream_opened event"),
        }
    }

    #[test]
    fn full_screen_repaint_bytes_reconstructs_visible_text() {
        let mut grid = test_grid(80, 24);
        grid.process(b"hello\r\nworld");

        let repaint = bmux_terminal_grid::full_screen_repaint_bytes(grid.grid());
        let mut replay = test_grid(80, 24);
        replay.process(&repaint);

        let contents = bmux_terminal_grid::visible_text(replay.grid(), 0, replay.grid().height());
        assert!(contents.contains("hello"));
        assert!(contents.contains("world"));
    }

    #[test]
    fn display_capture_rotation_writes_full_repaint_baseline() {
        let root = temp_dir();
        let recording_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let mut writer = DisplayCaptureFileWriter::open(
            recording_id,
            &root,
            client_id,
            Some(Duration::from_mins(5)),
        )
        .expect("writer should open");

        writer
            .record(DisplayTrackEvent::Resize { cols: 80, rows: 24 })
            .expect("resize should record");
        writer
            .record(DisplayTrackEvent::FrameBytes {
                data: b"hello before rotation".to_vec(),
            })
            .expect("frame should record");
        writer
            .rotate(2_000_000_000)
            .expect("rotation should write baseline");
        writer.flush().expect("writer should flush");

        let segment = display_track_segment_path(&root, client_id, 1);
        let bytes = std::fs::read(segment).expect("second segment should read");
        let frames = read_frames::<DisplayTrackEnvelope>(&bytes)
            .expect("segment should decode")
            .frames;
        let repaint = frames
            .iter()
            .find_map(|frame| match &frame.event {
                DisplayTrackEvent::FrameBytes { data } => Some(data),
                _ => None,
            })
            .expect("rotated segment should contain repaint frame");
        let mut replay = test_grid(80, 24);
        replay.process(repaint);
        assert!(
            bmux_terminal_grid::visible_text(replay.grid(), 0, replay.grid().height())
                .contains("hello before rotation"),
            "rotated segment baseline should carry prior visible content"
        );
    }

    #[test]
    fn update_cursor_replay_state_parses_decscusr() {
        let mut state = CursorReplayState::default();
        update_cursor_replay_state(&mut state, b"\x1b[6 q");
        assert!(matches!(state.shape, CursorVisualShape::Bar));
        assert!(!state.blink_enabled);
        update_cursor_replay_state(&mut state, b"\x1b[3 q");
        assert!(matches!(state.shape, CursorVisualShape::Underline));
        assert!(state.blink_enabled);
    }

    #[test]
    fn display_cursor_shape_from_visual_maps_shapes() {
        assert_eq!(
            display_cursor_shape_from_visual(CursorVisualShape::Block),
            DisplayCursorShape::Block
        );
        assert_eq!(
            display_cursor_shape_from_visual(CursorVisualShape::Bar),
            DisplayCursorShape::Bar
        );
        assert_eq!(
            display_cursor_shape_from_visual(CursorVisualShape::Underline),
            DisplayCursorShape::Underline
        );
    }

    fn test_grid(cols: u16, rows: u16) -> bmux_terminal_grid::TerminalGridStream {
        bmux_terminal_grid::TerminalGridStream::new(
            cols,
            rows,
            bmux_terminal_grid::GridLimits::default(),
        )
        .expect("test grid dimensions are valid")
    }

    fn temp_dir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be monotonic for test")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bmux-cli-plugin-test-{nanos}"));
        std::fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    use crate::runtime::recording::{
        auto_export_default_dir, auto_export_filename_stem, collect_recording_storage_usage,
        confirm_delete_all_recordings, default_event_kinds_for_flags, default_recording_list_order,
        delete_all_recordings_from_dir, delete_recording_dir_at, filter_recordings,
        format_recording_age, list_recordings_from_dir, offline_recording_status,
        recording_status_label, resolve_recording_id_prefix, resolve_recording_list_limit,
        sort_recordings, unique_auto_export_path,
    };
    use bmux_cli_schema::{RecordingListOrderArg, RecordingListSortArg, RecordingListStatusArg};
    use bmux_recording_protocol::RECORDING_FORMAT_VERSION;
    use std::fs;
    use uuid::Uuid;

    fn recording_summary_for_list_test(
        id: &str,
        name: Option<&str>,
        started_epoch_ms: u64,
        ended_epoch_ms: Option<u64>,
        event_count: u64,
        payload_bytes: u64,
    ) -> RecordingSummary {
        RecordingSummary {
            id: Uuid::parse_str(id).expect("test id should parse"),
            name: name.map(str::to_string),
            format_version: RECORDING_FORMAT_VERSION,
            session_id: None,
            capture_input: true,
            profile: RecordingProfile::Functional,
            event_kinds: vec![RecordingEventKind::PaneOutputRaw],
            started_epoch_ms,
            ended_epoch_ms,
            event_count,
            payload_bytes,
            path: "/tmp/test-recording".to_string(),
            segments: vec!["events_0.bin".to_string()],
            total_segment_bytes: payload_bytes,
        }
    }

    #[test]
    fn auto_export_filename_stem_uses_macos_like_timestamp() {
        let timestamp =
            time::OffsetDateTime::from_unix_timestamp(0).expect("timestamp should parse");
        assert_eq!(
            auto_export_filename_stem(timestamp),
            "Recording 1970-01-01 at 12.00.00 AM"
        );
    }

    #[test]
    fn auto_export_default_dir_uses_recording_parent_directory() {
        let recording_dir = std::path::PathBuf::from("/tmp/bmux/recordings/demo");
        assert_eq!(
            auto_export_default_dir(&recording_dir),
            std::path::PathBuf::from("/tmp/bmux/recordings")
        );
    }

    #[test]
    fn unique_auto_export_path_adds_numeric_suffix_when_needed() {
        let root = temp_dir();
        let stem = "Recording 2026-04-05 at 1.02.03 PM";
        fs::write(root.join(format!("{stem}.gif")), b"gif").expect("seed gif should write");

        let output = unique_auto_export_path(&root, stem);
        assert_eq!(output, root.join(format!("{stem} 2.gif")));
    }

    #[test]
    fn list_recordings_from_dir_returns_empty_when_missing() {
        let missing_dir = temp_dir().join("does-not-exist");
        let recordings = list_recordings_from_dir(&missing_dir).expect("listing should succeed");
        assert!(recordings.is_empty());
    }

    #[test]
    fn list_recordings_from_dir_reads_and_sorts_manifests() {
        let root = temp_dir();
        let newer_id = Uuid::new_v4();
        let older_id = Uuid::new_v4();
        let newer_dir = root.join(newer_id.to_string());
        let older_dir = root.join(older_id.to_string());
        fs::create_dir_all(&newer_dir).expect("newer recording dir should exist");
        fs::create_dir_all(&older_dir).expect("older recording dir should exist");

        let newer_manifest = serde_json::json!({
            "summary": {
                "id": newer_id,
                "session_id": serde_json::Value::Null,
                "capture_input": true,
                "started_epoch_ms": 200,
                "ended_epoch_ms": serde_json::Value::Null,
                "event_count": 12,
                "payload_bytes": 1024,
                "path": newer_dir.to_string_lossy().to_string()
            }
        });
        let older_manifest = serde_json::json!({
            "summary": {
                "id": older_id,
                "session_id": serde_json::Value::Null,
                "capture_input": false,
                "started_epoch_ms": 100,
                "ended_epoch_ms": 150,
                "event_count": 4,
                "payload_bytes": 128,
                "path": older_dir.to_string_lossy().to_string()
            }
        });

        fs::write(
            newer_dir.join("manifest.json"),
            serde_json::to_vec(&newer_manifest).expect("newer manifest should encode"),
        )
        .expect("newer manifest should write");
        fs::write(
            older_dir.join("manifest.json"),
            serde_json::to_vec(&older_manifest).expect("older manifest should encode"),
        )
        .expect("older manifest should write");

        let recordings = list_recordings_from_dir(&root).expect("listing should succeed");
        assert_eq!(recordings.len(), 2);
        assert_eq!(recordings[0].id, newer_id);
        assert_eq!(recordings[1].id, older_id);
    }

    #[test]
    fn offline_recording_status_reports_no_active_recording() {
        let status = offline_recording_status();
        assert!(status.active.is_none());
        assert_eq!(status.queue_len, 0);
    }

    #[test]
    fn default_event_kinds_for_flags_falls_back_to_output() {
        let kinds = default_event_kinds_for_flags(false, false, false);
        assert_eq!(kinds, vec![RecordingEventKind::PaneOutputRaw]);
    }

    #[test]
    fn recording_status_label_reflects_active_and_done_states() {
        assert_eq!(recording_status_label(None), "active");
        assert_eq!(recording_status_label(Some(1)), "done");
    }

    #[test]
    fn format_recording_age_uses_compact_units() {
        assert_eq!(format_recording_age(1_000, 1_900), "now");
        assert_eq!(format_recording_age(1_000, 32_000), "31s ago");
        assert_eq!(format_recording_age(1_000, 121_000), "2m ago");
        assert_eq!(format_recording_age(1_000, 3_601_000), "1h ago");
        assert_eq!(format_recording_age(1_000, 172_801_000), "2d ago");
        assert_eq!(format_recording_age(1_000, 691_201_000), "1w ago");
        assert_eq!(format_recording_age(1_000, 31_536_001_000), "1y ago");
    }

    #[test]
    fn resolve_recording_list_limit_uses_table_default_and_json_full() {
        assert_eq!(resolve_recording_list_limit(false, None, false), Some(10));
        assert_eq!(resolve_recording_list_limit(true, None, false), None);
        assert_eq!(resolve_recording_list_limit(false, Some(3), false), Some(3));
        assert_eq!(resolve_recording_list_limit(true, Some(3), false), Some(3));
        assert_eq!(resolve_recording_list_limit(false, Some(3), true), None);
    }

    #[test]
    fn default_recording_list_order_matches_sort_field() {
        assert_eq!(
            default_recording_list_order(RecordingListSortArg::Started),
            RecordingListOrderArg::Desc
        );
        assert_eq!(
            default_recording_list_order(RecordingListSortArg::Name),
            RecordingListOrderArg::Asc
        );
        assert_eq!(
            default_recording_list_order(RecordingListSortArg::Events),
            RecordingListOrderArg::Desc
        );
        assert_eq!(
            default_recording_list_order(RecordingListSortArg::Size),
            RecordingListOrderArg::Desc
        );
    }

    #[test]
    fn filter_recordings_applies_status_and_case_insensitive_query() {
        let active = recording_summary_for_list_test(
            "550e8400-e29b-41d4-a716-446655440000",
            Some("Startup Repro"),
            3,
            None,
            12,
            512,
        );
        let done = recording_summary_for_list_test(
            "550e8400-e29b-41d4-a716-446655440001",
            Some("Latency Sweep"),
            2,
            Some(9),
            8,
            256,
        );

        let filtered = filter_recordings(
            vec![active.clone(), done.clone()],
            RecordingListStatusArg::Active,
            Some("startup"),
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, active.id);

        let filtered = filter_recordings(
            vec![active, done.clone()],
            RecordingListStatusArg::Done,
            Some("550e8400-e29b-41d4-a716-446655440001"),
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, done.id);
    }

    #[test]
    fn sort_recordings_supports_name_events_and_size() {
        let alpha = recording_summary_for_list_test(
            "550e8400-e29b-41d4-a716-446655440000",
            Some("alpha"),
            10,
            Some(11),
            2,
            100,
        );
        let beta = recording_summary_for_list_test(
            "550e8400-e29b-41d4-a716-446655440001",
            Some("beta"),
            20,
            Some(21),
            9,
            900,
        );

        let mut recordings = vec![beta.clone(), alpha.clone()];
        sort_recordings(
            &mut recordings,
            RecordingListSortArg::Name,
            RecordingListOrderArg::Asc,
        );
        assert_eq!(recordings[0].id, alpha.id);

        sort_recordings(
            &mut recordings,
            RecordingListSortArg::Events,
            RecordingListOrderArg::Desc,
        );
        assert_eq!(recordings[0].id, beta.id);

        sort_recordings(
            &mut recordings,
            RecordingListSortArg::Size,
            RecordingListOrderArg::Asc,
        );
        assert_eq!(recordings[0].id, alpha.id);
    }

    #[test]
    fn collect_recording_storage_usage_skips_hidden_rolling_dir() {
        let root = temp_dir();
        let manual_id = Uuid::new_v4();
        let manual_dir = root.join(manual_id.to_string());
        fs::create_dir_all(&manual_dir).expect("manual recording dir should exist");
        fs::write(
            manual_dir.join("manifest.json"),
            br#"{"summary":{"id":"00000000-0000-0000-0000-000000000000","session_id":null,"capture_input":true,"started_epoch_ms":1,"ended_epoch_ms":null,"event_count":0,"payload_bytes":0,"path":"x"}}"#,
        )
        .expect("manual manifest should write");
        fs::write(manual_dir.join("events_0.bin"), b"manual-bytes")
            .expect("manual events should write");

        let rolling_dir = root.join(".rolling").join("active");
        fs::create_dir_all(&rolling_dir).expect("rolling dir should exist");
        fs::write(
            rolling_dir.join("manifest.json"),
            br#"{"summary":{"id":"00000000-0000-0000-0000-000000000000","session_id":null,"capture_input":true,"started_epoch_ms":1,"ended_epoch_ms":null,"event_count":0,"payload_bytes":0,"path":"x"}}"#,
        )
        .expect("rolling manifest should write");
        fs::write(rolling_dir.join("events_0.bin"), b"rolling-bytes")
            .expect("rolling events should write");

        let usage =
            collect_recording_storage_usage(&root).expect("usage collection should succeed");
        assert_eq!(usage.recording_dirs, 1);
        assert_eq!(usage.directories, 1);
        assert_eq!(usage.files, 2);
    }

    #[test]
    fn resolve_recording_id_prefix_prefers_exact_match() {
        let exact = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000")
            .expect("exact uuid should parse");
        let other = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440001")
            .expect("other uuid should parse");
        let recordings = vec![
            RecordingSummary {
                id: other,
                name: None,
                format_version: RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: RecordingProfile::Functional,
                event_kinds: vec![RecordingEventKind::PaneOutputRaw],
                started_epoch_ms: 1,
                ended_epoch_ms: Some(2),
                event_count: 0,
                payload_bytes: 0,
                path: "/tmp/other".to_string(),
                segments: vec!["events_0.bin".to_string()],
                total_segment_bytes: 0,
            },
            RecordingSummary {
                id: exact,
                name: None,
                format_version: RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: RecordingProfile::Functional,
                event_kinds: vec![RecordingEventKind::PaneOutputRaw],
                started_epoch_ms: 3,
                ended_epoch_ms: Some(4),
                event_count: 0,
                payload_bytes: 0,
                path: "/tmp/exact".to_string(),
                segments: vec!["events_0.bin".to_string()],
                total_segment_bytes: 0,
            },
        ];

        let resolved =
            resolve_recording_id_prefix("550e8400-e29b-41d4-a716-446655440000", &recordings)
                .expect("exact id should resolve");
        assert_eq!(resolved, exact);
    }

    #[test]
    fn resolve_recording_id_prefix_rejects_ambiguous_prefix() {
        let first = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000")
            .expect("first uuid should parse");
        let second = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440001")
            .expect("second uuid should parse");
        let recordings = vec![
            RecordingSummary {
                id: first,
                name: None,
                format_version: RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: RecordingProfile::Functional,
                event_kinds: vec![RecordingEventKind::PaneOutputRaw],
                started_epoch_ms: 1,
                ended_epoch_ms: None,
                event_count: 0,
                payload_bytes: 0,
                path: "/tmp/first".to_string(),
                segments: vec!["events_0.bin".to_string()],
                total_segment_bytes: 0,
            },
            RecordingSummary {
                id: second,
                name: None,
                format_version: RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: RecordingProfile::Functional,
                event_kinds: vec![RecordingEventKind::PaneOutputRaw],
                started_epoch_ms: 2,
                ended_epoch_ms: None,
                event_count: 0,
                payload_bytes: 0,
                path: "/tmp/second".to_string(),
                segments: vec!["events_0.bin".to_string()],
                total_segment_bytes: 0,
            },
        ];

        let error = resolve_recording_id_prefix("550e8400", &recordings)
            .expect_err("ambiguous prefix should fail");
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn resolve_recording_id_prefix_accepts_exact_name() {
        let named = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000")
            .expect("named uuid should parse");
        let recordings = vec![RecordingSummary {
            id: named,
            name: Some("startup regression".to_string()),
            format_version: RECORDING_FORMAT_VERSION,
            session_id: None,
            capture_input: true,
            profile: RecordingProfile::Functional,
            event_kinds: vec![RecordingEventKind::PaneOutputRaw],
            started_epoch_ms: 1,
            ended_epoch_ms: Some(2),
            event_count: 0,
            payload_bytes: 0,
            path: "/tmp/named".to_string(),
            segments: vec!["events_0.bin".to_string()],
            total_segment_bytes: 0,
        }];

        let resolved = resolve_recording_id_prefix("startup regression", &recordings)
            .expect("exact recording name should resolve");
        assert_eq!(resolved, named);
    }

    #[test]
    fn resolve_recording_id_prefix_rejects_ambiguous_name_prefix() {
        let first = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000")
            .expect("first uuid should parse");
        let second = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440001")
            .expect("second uuid should parse");
        let recordings = vec![
            RecordingSummary {
                id: first,
                name: Some("bug repro startup".to_string()),
                format_version: RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: RecordingProfile::Functional,
                event_kinds: vec![RecordingEventKind::PaneOutputRaw],
                started_epoch_ms: 1,
                ended_epoch_ms: None,
                event_count: 0,
                payload_bytes: 0,
                path: "/tmp/first".to_string(),
                segments: vec!["events_0.bin".to_string()],
                total_segment_bytes: 0,
            },
            RecordingSummary {
                id: second,
                name: Some("bug repro render".to_string()),
                format_version: RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: RecordingProfile::Functional,
                event_kinds: vec![RecordingEventKind::PaneOutputRaw],
                started_epoch_ms: 2,
                ended_epoch_ms: None,
                event_count: 0,
                payload_bytes: 0,
                path: "/tmp/second".to_string(),
                segments: vec!["events_0.bin".to_string()],
                total_segment_bytes: 0,
            },
        ];

        let error = resolve_recording_id_prefix("bug repro", &recordings)
            .expect_err("ambiguous name prefix should fail");
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn delete_recording_helpers_remove_manifest_directories() {
        let root = temp_dir();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        fs::create_dir_all(root.join(first.to_string())).expect("first dir should exist");
        fs::create_dir_all(root.join(second.to_string())).expect("second dir should exist");
        fs::write(
                root.join(first.to_string()).join("manifest.json"),
                br#"{"summary":{"id":"00000000-0000-0000-0000-000000000000","session_id":null,"capture_input":true,"started_epoch_ms":1,"ended_epoch_ms":null,"event_count":0,"payload_bytes":0,"path":"x"}}"#,
            )
            .expect("first manifest should write");
        fs::write(
                root.join(second.to_string()).join("manifest.json"),
                br#"{"summary":{"id":"00000000-0000-0000-0000-000000000000","session_id":null,"capture_input":true,"started_epoch_ms":1,"ended_epoch_ms":null,"event_count":0,"payload_bytes":0,"path":"x"}}"#,
            )
            .expect("second manifest should write");

        delete_recording_dir_at(&root, first).expect("single delete should succeed");
        assert!(!root.join(first.to_string()).exists());

        let deleted_count =
            delete_all_recordings_from_dir(&root).expect("delete-all helper should succeed");
        assert_eq!(deleted_count, 1);
        assert!(!root.join(second.to_string()).exists());
    }

    #[test]
    fn confirm_delete_all_requires_yes_for_non_interactive_mode() {
        assert!(confirm_delete_all_recordings(true).expect("--yes should bypass prompt"));
        let error = confirm_delete_all_recordings(false).expect_err("non-interactive should fail");
        assert!(error.to_string().contains("requires --yes"));
    }

    fn perf_custom_event(
        seq: u64,
        name: &str,
        ts_epoch_ms: u64,
        payload: serde_json::Value,
    ) -> RecordingEventEnvelope {
        let mut payload_object = match payload {
            serde_json::Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("value".to_string(), other);
                map
            }
        };
        payload_object.insert(
            "ts_epoch_ms".to_string(),
            serde_json::Value::from(ts_epoch_ms),
        );
        payload_object.insert(
            "level".to_string(),
            serde_json::Value::String("detailed".to_string()),
        );
        RecordingEventEnvelope {
            seq,
            mono_ns: seq.saturating_mul(1_000_000),
            wall_epoch_ms: ts_epoch_ms,
            session_id: None,
            pane_id: None,
            client_id: None,
            kind: RecordingEventKind::Custom,
            payload: RecordingPayload::Custom {
                source: PERF_RECORDING_SOURCE.to_string(),
                name: name.to_string(),
                payload: serde_json::to_vec(&serde_json::Value::Object(payload_object))
                    .expect("perf payload should encode"),
            },
        }
    }

    #[test]
    fn perf_event_emitter_surfaces_drop_counters_on_next_payload() {
        let settings = PerfCaptureSettings {
            level: PerfCaptureLevel::Basic,
            window_ms: 1_000,
            max_events_per_sec: 1,
            max_payload_bytes_per_sec: 4_096,
        };
        let mut emitter = PerfEventEmitter::new(settings);

        let payload_one = emitter.normalized_payload(serde_json::json!({"sample": 1}));
        let encoded_one = serde_json::to_vec(&payload_one).expect("payload should encode");
        assert!(emitter.can_emit_payload(encoded_one.len()));

        let payload_two = emitter.normalized_payload(serde_json::json!({"sample": 2}));
        let encoded_two = serde_json::to_vec(&payload_two).expect("payload should encode");
        assert!(!emitter.can_emit_payload(encoded_two.len()));

        let payload_three = emitter.normalized_payload(serde_json::json!({"sample": 3}));
        let object = payload_three
            .as_object()
            .expect("normalized payload should be object");
        assert_eq!(
            object
                .get("dropped_events_since_emit")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert!(
            object
                .get("dropped_payload_bytes_since_emit")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|bytes| bytes > 0),
            "drop payload bytes should be included after a rate-limited emit"
        );
    }

    #[test]
    fn analyze_perf_events_computes_percentiles_correlations_and_hints() {
        let events = vec![
            perf_custom_event(
                1,
                "iroh.connect.summary",
                1_000,
                serde_json::json!({"connect_ms": 120_u64, "total_ms": 300_u64}),
            ),
            perf_custom_event(
                2,
                "attach.first_frame",
                1_300,
                serde_json::json!({"time_to_first_frame_ms": 300_u64}),
            ),
            perf_custom_event(
                3,
                "attach.interactive.ready",
                1_600,
                serde_json::json!({"time_to_interactive_ms": 600_u64}),
            ),
            perf_custom_event(
                4,
                "attach.window",
                1_700,
                serde_json::json!({
                    "render_ms_max": 24_u64,
                    "drain_ipc_ms_max": 28_u64,
                    "render_ms_avg": 12_u64,
                    "drain_ipc_ms_avg": 8_u64,
                    "dropped_events_since_emit": 2_u64,
                    "dropped_payload_bytes_since_emit": 64_u64,
                    "dirty_no_visible_row_change_frames": 3_u64,
                    "extension_imperative_or_cache_miss_frames": 2_u64,
                    "terminal_graphic_deletes": 1_u64,
                }),
            ),
            perf_custom_event(
                5,
                "iroh.reconnect.outage",
                2_100,
                serde_json::json!({"outage_ms": 1_800_u64}),
            ),
        ];

        let report = analyze_perf_events(&events, Some(true));
        assert_eq!(report.perf_events, 5);
        assert_eq!(report.connect_to_first_frame_ms, Some(300));
        assert_eq!(report.connect_to_interactive_ms, Some(600));
        assert_eq!(report.reconnect_outage_max_ms, Some(1_800));
        assert_eq!(report.dropped_events_reported, 2);
        assert_eq!(report.dropped_payload_bytes_reported, 64);
        assert_eq!(
            report
                .overrender_counters
                .get("dirty_no_visible_row_change_frames"),
            Some(&3)
        );
        assert_eq!(
            report
                .attach_window_counters
                .get("terminal_graphic_deletes"),
            Some(&1)
        );
        assert_eq!(
            report
                .timings_ms
                .get("connect_ms")
                .map(|timing| timing.p95_ms),
            Some(120)
        );
        assert!(
            report
                .hints
                .iter()
                .any(|hint| hint.contains("reconnect outage")),
            "expected reconnect outage hint in analysis output"
        );
        assert!(
            report
                .hints
                .iter()
                .any(|hint| hint.contains("no-op invalidations")),
            "expected render inefficiency hint in analysis output"
        );
        assert!(
            report
                .hints
                .iter()
                .any(|hint| hint.contains("retained terminal graphics")),
            "expected retained graphics hint in analysis output"
        );
    }

    #[test]
    fn analyze_perf_events_hints_when_custom_events_were_not_captured() {
        let report = analyze_perf_events(&[], Some(false));
        assert_eq!(report.perf_events, 0);
        assert!(
            report
                .hints
                .iter()
                .any(|hint| hint.contains("did not capture `custom` events")),
            "expected missing-custom-events guidance"
        );
    }
}
