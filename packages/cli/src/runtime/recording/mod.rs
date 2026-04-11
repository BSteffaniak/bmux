use super::cli_parse::{
    RECORDING_AUTO_EXPORT_DIR_OVERRIDE_ENV, RECORDING_AUTO_EXPORT_OVERRIDE_ENV,
};
use super::{
    BmuxConfig, BufWriter, ConfigPaths, ConnectionContext, ConnectionPolicyScope, Context,
    GifEncoder, GifFrame, Instant, IsTerminal, Path, PathBuf, RecordingCursorBlinkMode,
    RecordingCursorMode, RecordingCursorPaintMode, RecordingCursorProfile, RecordingCursorShape,
    RecordingCursorTextMode, RecordingEventEnvelope, RecordingEventKind, RecordingEventKindArg,
    RecordingExportFormat, RecordingListOrderArg, RecordingListSortArg, RecordingListStatusArg,
    RecordingPaletteSource, RecordingProfileArg, RecordingRenderMode, RecordingReplayMode,
    RecordingStatus, RecordingSummary, Repeat, Result, Uuid, Write, active_runtime_name,
    cleanup_stale_pid_file, connect_if_running_with_context, io, map_cli_client_error,
    parse_uuid_value, terminal,
};
use ab_glyph::{Font, FontArc, FontVec, PxScale, ScaleFont, point};
use bmux_cli_output::{Table, TableAlign, TableColumn, write_table};
use bmux_fonts::FontPreset;
use bmux_ipc::RecordingPayload;
use font8x8::UnicodeFonts;
use resvg::{tiny_skia, usvg};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

mod terminal_profile;

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
    let summary = client
        .recording_start(session_id, capture_input, name, profile, event_kinds)
        .await
        .map_err(map_cli_client_error)?;
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
) -> Option<bmux_ipc::RecordingProfile> {
    match profile {
        Some(RecordingProfileArg::Full) => Some(bmux_ipc::RecordingProfile::Full),
        Some(RecordingProfileArg::Functional) => Some(bmux_ipc::RecordingProfile::Functional),
        Some(RecordingProfileArg::Visual) => Some(bmux_ipc::RecordingProfile::Visual),
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
}

const PERF_RECORDING_SOURCE: &str = bmux_ipc::PERF_RECORDING_SOURCE;
const PERF_RECORDING_SCHEMA_VERSION: u8 = bmux_ipc::PERF_RECORDING_SCHEMA_VERSION;

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
    pub(super) const fn from_runtime(level: bmux_ipc::PerformanceRecordingLevel) -> Self {
        match level {
            bmux_ipc::PerformanceRecordingLevel::Off => Self::Off,
            bmux_ipc::PerformanceRecordingLevel::Basic => Self::Basic,
            bmux_ipc::PerformanceRecordingLevel::Detailed => Self::Detailed,
            bmux_ipc::PerformanceRecordingLevel::Trace => Self::Trace,
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
    pub(super) fn from_runtime_settings(settings: &bmux_ipc::PerformanceRuntimeSettings) -> Self {
        Self {
            level: PerfCaptureLevel::from_runtime(settings.recording_level),
            window_ms: settings.window_ms.max(1),
            max_events_per_sec: settings.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: settings.max_payload_bytes_per_sec.max(1),
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

        client
            .recording_write_custom_event(
                session_id,
                pane_id,
                PERF_RECORDING_SOURCE.to_string(),
                event_name.to_string(),
                encoded,
            )
            .await
            .map_err(map_cli_client_error)
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

        client
            .recording_write_custom_event(
                session_id,
                pane_id,
                PERF_RECORDING_SOURCE.to_string(),
                event_name.to_string(),
                encoded,
            )
            .await
            .map_err(map_cli_client_error)
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
    let enabled = std::env::var(RECORDING_AUTO_EXPORT_OVERRIDE_ENV)
        .ok()
        .map_or(config.recording.auto_export, |raw| {
            parse_bool_env_flag(&raw).unwrap_or_else(|| {
                tracing::warn!(
                    "ignoring invalid {} value {:?}",
                    RECORDING_AUTO_EXPORT_OVERRIDE_ENV,
                    raw
                );
                config.recording.auto_export
            })
        });
    let output_dir = env_path_override(RECORDING_AUTO_EXPORT_DIR_OVERRIDE_ENV)
        .or_else(|| config.recording_auto_export_dir(&paths));
    RecordingAutoExportSettings {
        enabled,
        output_dir,
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

pub(super) async fn maybe_auto_export_recording(recording_id: Uuid, recording_path: Option<&Path>) {
    let settings = recording_auto_export_settings();
    if !settings.enabled {
        return;
    }

    let recording_dir = recording_path.map_or_else(
        || recordings_root_dir().join(recording_id.to_string()),
        std::path::Path::to_path_buf,
    );
    let output_path = auto_export_output_path(&recording_dir, settings.output_dir.as_deref());
    let output = output_path.to_string_lossy().into_owned();
    let recording_id_string = recording_id.to_string();
    if let Err(error) =
        super::recording_cli::run_recording_auto_export_gif(&recording_id_string, &output).await
    {
        eprintln!(
            "bmux warning: recording auto-export failed for {} (output={}): {}",
            recording_id,
            output_path.display(),
            error
        );
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
    let stopped_id = client
        .recording_stop(recording_id)
        .await
        .map_err(map_cli_client_error)?;
    println!("recording stopped: {stopped_id}");
    maybe_auto_export_recording(stopped_id, None).await;
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
        Some(mut client) => client
            .recording_status()
            .await
            .map_err(map_cli_client_error)?,
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
        Some(mut client) => client
            .recording_list()
            .await
            .map_err(map_cli_client_error)?,
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
        let status = client
            .recording_status()
            .await
            .map_err(map_cli_client_error)?;
        let recordings = client
            .recording_list()
            .await
            .map_err(map_cli_client_error)?;
        let resolved = resolve_recording_id_prefix(recording_id_or_prefix, &recordings)?;

        if status
            .active
            .as_ref()
            .is_some_and(|active| active.id == resolved)
        {
            let stopped_id = client
                .recording_stop(Some(resolved))
                .await
                .map_err(map_cli_client_error)?;
            println!("stopped active recording {stopped_id} before delete");
        }

        let deleted_id = client
            .recording_delete(resolved)
            .await
            .map_err(map_cli_client_error)?;
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
        let status = client
            .recording_status()
            .await
            .map_err(map_cli_client_error)?;
        if let Some(active) = status.active {
            let stopped_id = client
                .recording_stop(Some(active.id))
                .await
                .map_err(map_cli_client_error)?;
            println!("stopped active recording {stopped_id} before delete");
        }
        let deleted_count = client
            .recording_delete_all()
            .await
            .map_err(map_cli_client_error)?;
        println!("deleted {deleted_count} recordings");
    } else {
        let deleted_count = delete_all_recordings_from_disk()?;
        println!("deleted {deleted_count} recordings");
    }
    Ok(0)
}

pub(super) async fn run_recording_cut(
    last_seconds: Option<u64>,
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

    let recording = client
        .recording_cut(last_seconds, name)
        .await
        .map_err(map_cli_client_error)?;
    let name_display = recording.name.as_deref().unwrap_or("-");
    tracing::info!(
        id = %recording.id,
        name = %name_display,
        events = recording.event_count,
        bytes = recording.payload_bytes,
        path = %recording.path,
        "recording cut created",
    );
    println!(
        "recording cut created: {} name={} events={} bytes={} path={}",
        recording.id, name_display, recording.event_count, recording.payload_bytes, recording.path
    );
    let recording_path = PathBuf::from(&recording.path);
    maybe_auto_export_recording(recording.id, Some(&recording_path)).await;
    Ok(0)
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
        client
            .recording_prune(older_than)
            .await
            .map_err(map_cli_client_error)?
    } else {
        let root = recordings_root_dir();
        let config = bmux_config::BmuxConfig::load().unwrap_or_default();
        let retention = older_than.unwrap_or(config.recording.retention_days);
        bmux_server::recording::prune_old_recordings(&root, retention)?
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
    timings_ms: BTreeMap<String, PerfTimingSummary>,
    outlier_samples: Vec<PerfOutlierSample>,
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
                "no bmux.perf events found; set `performance.recording_level` and reproduce with recording enabled"
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
            if !field.ends_with("_ms") {
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
        .sort_by(|left, right| right.value_ms.cmp(&left.value_ms));
    report.outlier_samples.truncate(20);

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
        println!("outliers:");
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

#[allow(clippy::unused_async, clippy::too_many_arguments)] // Called in async context; may need async for future network export
pub(super) async fn run_recording_export(
    recording_id: &str,
    format: RecordingExportFormat,
    output: &str,
    view_client: Option<&str>,
    speed: f64,
    fps: u32,
    max_duration: Option<u64>,
    max_frames: Option<u32>,
    renderer: RecordingRenderMode,
    cell_size: Option<(u16, u16)>,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
    font_family: Option<&str>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    font_path: &[String],
    palette_source: RecordingPaletteSource,
    palette_foreground: Option<&str>,
    palette_background: Option<&str>,
    palette_color: &[String],
    cursor: RecordingCursorMode,
    cursor_shape: RecordingCursorShape,
    cursor_blink: RecordingCursorBlinkMode,
    cursor_blink_period_ms: u32,
    cursor_color: &str,
    cursor_profile: RecordingCursorProfile,
    cursor_solid_after_activity_ms: Option<u32>,
    cursor_solid_after_input_ms: Option<u32>,
    cursor_solid_after_output_ms: Option<u32>,
    cursor_solid_after_cursor_ms: Option<u32>,
    cursor_paint_mode: RecordingCursorPaintMode,
    cursor_text_mode: RecordingCursorTextMode,
    cursor_bar_width_pct: u8,
    cursor_underline_height_pct: u8,
    export_metadata: Option<&str>,
    show_progress: bool,
) -> Result<u8> {
    let recordings = list_recordings_from_disk()?;
    let recording_id = resolve_recording_id_prefix(recording_id, &recordings)?;
    let recording_dir = recordings_root_dir().join(recording_id.to_string());
    if !recording_dir.exists() {
        anyhow::bail!("recording not found: {recording_id}")
    }
    let manifest_summary = read_recording_manifest(&recording_dir.join("manifest.json"))?;
    if manifest_summary.format_version != bmux_ipc::RECORDING_FORMAT_VERSION {
        anyhow::bail!(
            "recording format version {} is unsupported; expected {}. re-record with current bmux",
            manifest_summary.format_version,
            bmux_ipc::RECORDING_FORMAT_VERSION
        )
    }

    let selected_client = if let Some(raw) = view_client {
        parse_uuid_value(raw, "view client id")?
    } else if let Some(owner) = read_recording_owner_client(&recording_dir)? {
        owner
    } else {
        match infer_display_track_client(&recording_dir) {
            InferredClient::One(id) => id,
            InferredClient::Multiple(ids) => {
                anyhow::bail!(
                    "multiple display tracks found; pass --view-client with one of: {}",
                    ids.iter()
                        .map(std::string::ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            InferredClient::None => {
                anyhow::bail!("no display tracks found in recording; cannot export");
            }
        }
    };

    let events = load_display_track_events(&recording_dir, selected_client)?;
    if events.is_empty() {
        anyhow::bail!(
            "display track is empty for client {selected_client}; cannot export exact-view media"
        )
    }

    let recording_profile = recording_terminal_profile(&events);
    let host_profile = terminal_profile::detect_render_profile();
    let terminal_profile = recording_profile.as_ref().or(host_profile.as_ref());

    match format {
        RecordingExportFormat::Gif => export_recording_gif(
            &events,
            output,
            speed,
            fps,
            max_duration,
            max_frames,
            terminal_profile,
            recording_profile.as_ref(),
            host_profile.as_ref(),
            renderer,
            cell_size,
            cell_width,
            cell_height,
            font_family,
            font_size,
            line_height,
            font_path,
            palette_source,
            palette_foreground,
            palette_background,
            palette_color,
            cursor,
            cursor_shape,
            cursor_blink,
            cursor_blink_period_ms,
            cursor_color,
            cursor_profile,
            cursor_solid_after_activity_ms,
            cursor_solid_after_input_ms,
            cursor_solid_after_output_ms,
            cursor_solid_after_cursor_ms,
            cursor_paint_mode,
            cursor_text_mode,
            cursor_bar_width_pct,
            cursor_underline_height_pct,
            export_metadata,
            show_progress,
        )?,
    }

    println!("export complete: format={format:?} view_client={selected_client} output={output}");
    Ok(0)
}

fn read_recording_owner_client(recording_dir: &Path) -> Result<Option<Uuid>> {
    let owner_path = recording_dir.join("owner-client-id.txt");
    let content = match std::fs::read_to_string(&owner_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed reading {}", owner_path.display()));
        }
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(parse_uuid_value(trimmed, "owner client id")?))
}

enum InferredClient {
    One(Uuid),
    Multiple(Vec<Uuid>),
    None,
}

/// When `owner-client-id.txt` is missing, scan the recording directory for
/// `display-{uuid}.bin` files. If exactly one exists, return its client id so
/// the export can proceed without requiring `--view-client`.
fn infer_display_track_client(recording_dir: &Path) -> InferredClient {
    let Ok(entries) = std::fs::read_dir(recording_dir) else {
        return InferredClient::None;
    };
    let mut found: Vec<Uuid> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(rest) = name.strip_prefix("display-")
            && let Some(uuid_str) = rest.strip_suffix(".bin")
            && let Ok(id) = uuid_str.parse::<Uuid>()
        {
            found.push(id);
        }
    }
    match found.len() {
        1 => InferredClient::One(found[0]),
        n if n > 1 => InferredClient::Multiple(found),
        _ => InferredClient::None,
    }
}

fn load_display_track_events(
    recording_dir: &Path,
    client_id: Uuid,
) -> Result<Vec<DisplayTrackEnvelope>> {
    let path = display_track_path(recording_dir, client_id);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed reading display track {}", path.display()))?;
    let result = bmux_ipc::read_frames(&bytes)
        .map_err(|e| anyhow::anyhow!("failed parsing display track {}: {e}", path.display()))?;
    if result.bytes_remaining > 0 {
        tracing::warn!(
            "display track {}: {} trailing bytes could not be parsed (truncated?)",
            path.display(),
            result.bytes_remaining
        );
    }
    Ok(result.frames)
}

fn recording_terminal_profile(
    events: &[DisplayTrackEnvelope],
) -> Option<terminal_profile::DetectedTerminalProfile> {
    for envelope in events {
        if let DisplayTrackEvent::StreamOpened {
            terminal_profile: Some(profile_bytes),
            ..
        } = &envelope.event
            && let Ok(profile) =
                bmux_ipc::decode::<terminal_profile::DetectedTerminalProfile>(profile_bytes)
        {
            return Some(profile);
        }
    }
    None
}

#[derive(Clone, Copy, Debug)]
struct CellMetrics {
    width: u16,
    height: u16,
}

fn resolve_export_cell_metrics(
    events: &[DisplayTrackEnvelope],
    cell_size: Option<(u16, u16)>,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
) -> Result<CellMetrics> {
    if cell_size.is_some_and(|(w, h)| w == 0 || h == 0) {
        anyhow::bail!("--cell-size values must be greater than zero")
    }
    if cell_width.is_some_and(|value| value == 0) {
        anyhow::bail!("--cell-width must be greater than zero")
    }
    if cell_height.is_some_and(|value| value == 0) {
        anyhow::bail!("--cell-height must be greater than zero")
    }

    let (size_width, size_height) = cell_size.unwrap_or((0, 0));
    let cli_width = cell_width.or_else(|| (size_width > 0).then_some(size_width));
    let cli_height = cell_height.or_else(|| (size_height > 0).then_some(size_height));

    let recorded = recording_cell_metrics(events);
    let current = current_terminal_cell_metrics();
    let width = cli_width
        .or_else(|| recorded.map(|value| value.width))
        .or_else(|| current.map(|value| value.width))
        .unwrap_or(8);
    let height = cli_height
        .or_else(|| recorded.map(|value| value.height))
        .or_else(|| current.map(|value| value.height))
        .unwrap_or(16);
    Ok(CellMetrics { width, height })
}

fn recording_cell_metrics(events: &[DisplayTrackEnvelope]) -> Option<CellMetrics> {
    let mut stream_opened = None::<(Option<u16>, Option<u16>, Option<u16>, Option<u16>)>;
    let mut fallback_cols_rows = None::<(u16, u16)>;
    for envelope in events {
        match envelope.event {
            DisplayTrackEvent::StreamOpened {
                cell_width_px,
                cell_height_px,
                window_width_px,
                window_height_px,
                ..
            } => {
                stream_opened = Some((
                    cell_width_px,
                    cell_height_px,
                    window_width_px,
                    window_height_px,
                ));
                if let (Some(width), Some(height)) = (cell_width_px, cell_height_px)
                    && width > 0
                    && height > 0
                {
                    return Some(CellMetrics { width, height });
                }
            }
            DisplayTrackEvent::Resize { cols, rows } => {
                if fallback_cols_rows.is_none() && cols > 0 && rows > 0 {
                    fallback_cols_rows = Some((cols, rows));
                }
            }
            DisplayTrackEvent::FrameBytes { .. }
            | DisplayTrackEvent::CursorSnapshot { .. }
            | DisplayTrackEvent::Activity { .. }
            | DisplayTrackEvent::ImageUpdate { .. }
            | DisplayTrackEvent::StreamClosed => {}
        }
    }

    let (cell_width_px, cell_height_px, window_width_px, window_height_px) = stream_opened?;
    if let (Some(width), Some(height)) = (cell_width_px, cell_height_px)
        && width > 0
        && height > 0
    {
        return Some(CellMetrics { width, height });
    }
    let (window_width, window_height) = (window_width_px?, window_height_px?);
    let (cols, rows) = fallback_cols_rows?;
    infer_cell_metrics(window_width, window_height, cols, rows)
}

fn current_terminal_cell_metrics() -> Option<CellMetrics> {
    let (cols, rows) = terminal::size().ok()?;
    if cols == 0 || rows == 0 {
        return None;
    }
    let size = terminal::window_size().ok()?;
    infer_cell_metrics(size.width, size.height, cols, rows)
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

fn infer_export_terminal_bounds(events: &[DisplayTrackEnvelope]) -> Result<(u16, u16)> {
    let mut resize_bounds = None::<(u16, u16)>;
    let mut stream_bounds = None::<(u16, u16)>;
    let mut cursor_cols = 0_u16;
    let mut cursor_rows = 0_u16;

    for envelope in events {
        match envelope.event {
            DisplayTrackEvent::Resize { cols, rows } => {
                let cols = cols.max(1);
                let rows = rows.max(1);
                resize_bounds = Some(match resize_bounds {
                    Some((current_cols, current_rows)) => {
                        (current_cols.max(cols), current_rows.max(rows))
                    }
                    None => (cols, rows),
                });
            }
            DisplayTrackEvent::StreamOpened {
                cell_width_px,
                cell_height_px,
                window_width_px,
                window_height_px,
                ..
            } => {
                if let (
                    Some(cell_width),
                    Some(cell_height),
                    Some(window_width),
                    Some(window_height),
                ) = (
                    cell_width_px,
                    cell_height_px,
                    window_width_px,
                    window_height_px,
                ) && cell_width > 0
                    && cell_height > 0
                {
                    let cols = (window_width / cell_width).max(1);
                    let rows = (window_height / cell_height).max(1);
                    stream_bounds = Some(match stream_bounds {
                        Some((current_cols, current_rows)) => {
                            (current_cols.max(cols), current_rows.max(rows))
                        }
                        None => (cols, rows),
                    });
                }
            }
            DisplayTrackEvent::CursorSnapshot { x, y, .. } => {
                cursor_cols = cursor_cols.max(x.saturating_add(1));
                cursor_rows = cursor_rows.max(y.saturating_add(1));
            }
            DisplayTrackEvent::FrameBytes { .. }
            | DisplayTrackEvent::Activity { .. }
            | DisplayTrackEvent::ImageUpdate { .. }
            | DisplayTrackEvent::StreamClosed => {}
        }
    }

    if let Some((cols, rows)) = resize_bounds {
        return Ok((cols, rows));
    }

    if let Some((stream_cols, stream_rows)) = stream_bounds {
        return Ok((stream_cols.max(cursor_cols), stream_rows.max(cursor_rows)));
    }

    anyhow::bail!(
        "recording export cannot infer terminal bounds: display track is missing resize events and stream-opened grid metrics"
    )
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

#[derive(Debug, Clone)]
struct CursorExportOptions {
    mode: RecordingCursorMode,
    shape: RecordingCursorShape,
    blink: RecordingCursorBlinkMode,
    profile: RecordingCursorProfile,
    blink_period_ns: u64,
    solid_after_input_ns: u64,
    solid_after_output_ns: u64,
    solid_after_cursor_ns: u64,
    paint_mode: RecordingCursorPaintMode,
    text_mode: RecordingCursorTextMode,
    bar_width_pct: u8,
    underline_height_pct: u8,
    color_label: String,
    color_override: Option<(u8, u8, u8)>,
}

#[derive(Debug, Clone, Copy)]
struct RecordedCursorSnapshot {
    x: u16,
    y: u16,
    visible: bool,
    shape: bmux_ipc::DisplayCursorShape,
    blink_enabled: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CursorVisibilityReason {
    Hidden,
    ForcedOn,
    HoldInput,
    HoldOutput,
    HoldCursor,
    BlinkOn,
    BlinkOff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockPaintMode {
    Invert,
    Fill,
    Outline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockTextMode {
    SwapFgBg,
    ForceContrast,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ExportCursorFrame {
    mono_ns: u64,
    row: u16,
    col: u16,
    visible: bool,
    shape: &'static str,
    blink_on: bool,
    cursor_source: &'static str,
    visible_reason: CursorVisibilityReason,
    paint_mode_used: &'static str,
    text_mode_used: &'static str,
    paint_fallback_reason: Option<&'static str>,
    last_input_activity_ns: Option<u64>,
    last_output_activity_ns: Option<u64>,
    last_cursor_activity_ns: Option<u64>,
}

#[derive(Debug, serde::Serialize)]
struct ExportMetadata<'a> {
    format: &'a str,
    output: &'a str,
    fps: u32,
    speed: f64,
    emitted_frames: u32,
    cursor: CursorMetadata<'a>,
    frames: Vec<ExportCursorFrame>,
}

#[derive(Debug, serde::Serialize)]
struct CursorMetadata<'a> {
    mode: &'a str,
    shape: &'a str,
    blink: &'a str,
    profile: &'a str,
    blink_period_ms: u32,
    solid_after_input_ms: u32,
    solid_after_output_ms: u32,
    solid_after_cursor_ms: u32,
    paint_mode: &'a str,
    text_mode: &'a str,
    bar_width_pct: u8,
    underline_height_pct: u8,
    color: &'a str,
}

fn parse_cursor_color(value: &str) -> Result<Option<(u8, u8, u8)>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let Some(rgb) = parse_rgb_color(trimmed) else {
        anyhow::bail!("invalid cursor color '{value}'; expected auto or a color value")
    };
    Ok(Some(rgb))
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

const fn display_cursor_shape_from_visual(
    shape: CursorVisualShape,
) -> bmux_ipc::DisplayCursorShape {
    match shape {
        CursorVisualShape::Block => bmux_ipc::DisplayCursorShape::Block,
        CursorVisualShape::Bar => bmux_ipc::DisplayCursorShape::Bar,
        CursorVisualShape::Underline => bmux_ipc::DisplayCursorShape::Underline,
    }
}

fn cursor_snapshot_from_parser_fallback(
    parser: &vt100::Parser,
    replay_state: CursorReplayState,
) -> RecordedCursorSnapshot {
    let (y, x) = parser.screen().cursor_position();
    RecordedCursorSnapshot {
        x,
        y,
        visible: !parser.screen().hide_cursor(),
        shape: display_cursor_shape_from_visual(replay_state.shape),
        blink_enabled: replay_state.blink_enabled,
    }
}

const fn effective_cursor_shape(
    options: &CursorExportOptions,
    replay_state: CursorReplayState,
    snapshot_shape: bmux_ipc::DisplayCursorShape,
) -> CursorVisualShape {
    match options.shape {
        RecordingCursorShape::Auto => match snapshot_shape {
            bmux_ipc::DisplayCursorShape::Block => replay_state.shape,
            bmux_ipc::DisplayCursorShape::Bar => CursorVisualShape::Bar,
            bmux_ipc::DisplayCursorShape::Underline => CursorVisualShape::Underline,
        },
        RecordingCursorShape::Block => CursorVisualShape::Block,
        RecordingCursorShape::Bar => CursorVisualShape::Bar,
        RecordingCursorShape::Underline => CursorVisualShape::Underline,
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn compute_cursor_visibility(
    options: &CursorExportOptions,
    replay_state: CursorReplayState,
    snapshot_blink_enabled: bool,
    parser_visible: bool,
    mono_ns: u64,
    last_input_activity_ns: Option<u64>,
    last_output_activity_ns: Option<u64>,
    last_cursor_activity_ns: Option<u64>,
    blink_anchor_ns: &mut Option<u64>,
) -> (bool, bool, CursorVisibilityReason) {
    let base_visible = match options.mode {
        RecordingCursorMode::Auto => parser_visible,
        RecordingCursorMode::On => true,
        RecordingCursorMode::Off => false,
    };
    if !base_visible {
        return (false, true, CursorVisibilityReason::Hidden);
    }
    if matches!(options.mode, RecordingCursorMode::On) {
        return (true, true, CursorVisibilityReason::ForcedOn);
    }
    let blink_enabled = match options.blink {
        RecordingCursorBlinkMode::Auto => replay_state.blink_enabled && snapshot_blink_enabled,
        RecordingCursorBlinkMode::On => true,
        RecordingCursorBlinkMode::Off => false,
    };
    if !blink_enabled {
        return (true, true, CursorVisibilityReason::ForcedOn);
    }
    if last_input_activity_ns
        .is_some_and(|last| mono_ns.saturating_sub(last) < options.solid_after_input_ns)
    {
        return (true, true, CursorVisibilityReason::HoldInput);
    }
    if last_output_activity_ns
        .is_some_and(|last| mono_ns.saturating_sub(last) < options.solid_after_output_ns)
    {
        return (true, true, CursorVisibilityReason::HoldOutput);
    }
    if last_cursor_activity_ns
        .is_some_and(|last| mono_ns.saturating_sub(last) < options.solid_after_cursor_ns)
    {
        return (true, true, CursorVisibilityReason::HoldCursor);
    }
    let latest_activity = [
        last_input_activity_ns,
        last_output_activity_ns,
        last_cursor_activity_ns,
    ]
    .into_iter()
    .flatten()
    .max();
    if let Some(last_activity) = latest_activity
        && matches!(options.profile, RecordingCursorProfile::Ghostty)
        && last_activity <= mono_ns
        && blink_anchor_ns.is_none_or(|anchor| last_activity > anchor)
    {
        *blink_anchor_ns = Some(last_activity);
    }
    let period = options.blink_period_ns.max(1);
    let anchor = *blink_anchor_ns.get_or_insert(mono_ns);
    let phase_ns = mono_ns.saturating_sub(anchor);
    let blink_on = (phase_ns / period).is_multiple_of(2);
    (
        blink_on,
        blink_on,
        if blink_on {
            CursorVisibilityReason::BlinkOn
        } else {
            CursorVisibilityReason::BlinkOff
        },
    )
}

const fn cursor_shape_name(shape: CursorVisualShape) -> &'static str {
    match shape {
        CursorVisualShape::Block => "block",
        CursorVisualShape::Bar => "bar",
        CursorVisualShape::Underline => "underline",
    }
}

const fn paint_mode_name(mode: BlockPaintMode) -> &'static str {
    match mode {
        BlockPaintMode::Invert => "invert",
        BlockPaintMode::Fill => "fill",
        BlockPaintMode::Outline => "outline",
    }
}

const fn text_mode_name(mode: BlockTextMode) -> &'static str {
    match mode {
        BlockTextMode::SwapFgBg => "swap_fg_bg",
        BlockTextMode::ForceContrast => "force_contrast",
    }
}

fn relative_luminance(rgb: (u8, u8, u8)) -> f32 {
    let channel = |value: u8| {
        let v = f32::from(value) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.0722f32.mul_add(
        channel(rgb.2),
        0.2126f32.mul_add(channel(rgb.0), 0.7152 * channel(rgb.1)),
    )
}

fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let l1 = relative_luminance(a);
    let l2 = relative_luminance(b);
    let (high, low) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (high + 0.05) / (low + 0.05)
}

fn pick_contrast_text_color(fill: (u8, u8, u8)) -> (u8, u8, u8) {
    if contrast_ratio((0, 0, 0), fill) >= contrast_ratio((255, 255, 255), fill) {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn overlay_cursor_rgba(
    pixels: &mut [u8],
    frame_width: usize,
    frame_height: usize,
    cell_w: usize,
    cell_h: usize,
    row: u16,
    col: u16,
    shape: CursorVisualShape,
    paint_mode: BlockPaintMode,
    text_mode: BlockTextMode,
    bar_width_pct: u8,
    underline_height_pct: u8,
    cell_foreground: (u8, u8, u8),
    cell_background: (u8, u8, u8),
    color: (u8, u8, u8),
) -> (BlockPaintMode, BlockTextMode, Option<&'static str>) {
    if frame_width == 0 || frame_height == 0 || cell_w == 0 || cell_h == 0 {
        return (paint_mode, text_mode, None);
    }
    let x0 = usize::from(col).saturating_mul(cell_w);
    let y0 = usize::from(row).saturating_mul(cell_h);
    if x0 >= frame_width || y0 >= frame_height {
        return (paint_mode, text_mode, None);
    }
    let resolved_paint_mode = paint_mode;
    let mut resolved_text_mode = text_mode;
    let mut fallback_reason = None;
    match shape {
        CursorVisualShape::Block => match resolved_paint_mode {
            BlockPaintMode::Invert => {
                for py in 0..cell_h {
                    let y = y0 + py;
                    if y >= frame_height {
                        continue;
                    }
                    for px in 0..cell_w {
                        let x = x0 + px;
                        if x >= frame_width {
                            continue;
                        }
                        let idx = (y * frame_width + x) * 4;
                        pixels[idx] = 255_u8.saturating_sub(pixels[idx]);
                        pixels[idx + 1] = 255_u8.saturating_sub(pixels[idx + 1]);
                        pixels[idx + 2] = 255_u8.saturating_sub(pixels[idx + 2]);
                        pixels[idx + 3] = 255;
                    }
                }
            }
            BlockPaintMode::Fill => {
                let mut effective_text_mode = text_mode;
                if matches!(text_mode, BlockTextMode::SwapFgBg)
                    && contrast_ratio(cell_background, color) < 2.0
                {
                    effective_text_mode = BlockTextMode::ForceContrast;
                    fallback_reason = Some("swap_fg_bg_low_contrast");
                }
                resolved_text_mode = effective_text_mode;
                let fill_text = match effective_text_mode {
                    BlockTextMode::SwapFgBg => cell_background,
                    BlockTextMode::ForceContrast => pick_contrast_text_color(color),
                };
                for py in 0..cell_h {
                    let y = y0 + py;
                    if y >= frame_height {
                        continue;
                    }
                    for px in 0..cell_w {
                        let x = x0 + px;
                        if x >= frame_width {
                            continue;
                        }
                        let idx = (y * frame_width + x) * 4;
                        pixels[idx] = color.0;
                        pixels[idx + 1] = color.1;
                        pixels[idx + 2] = color.2;
                        pixels[idx + 3] = 255;
                    }
                }
                let inset_x = (cell_w / 8).max(1);
                let inset_y = (cell_h / 8).max(1);
                if cell_w > inset_x.saturating_mul(2) && cell_h > inset_y.saturating_mul(2) {
                    for py in inset_y..(cell_h - inset_y) {
                        let y = y0 + py;
                        if y >= frame_height {
                            continue;
                        }
                        for px in inset_x..(cell_w - inset_x) {
                            let x = x0 + px;
                            if x >= frame_width {
                                continue;
                            }
                            let idx = (y * frame_width + x) * 4;
                            pixels[idx] = fill_text.0;
                            pixels[idx + 1] = fill_text.1;
                            pixels[idx + 2] = fill_text.2;
                            pixels[idx + 3] = 255;
                        }
                    }
                }
            }
            BlockPaintMode::Outline => {
                for py in 0..cell_h {
                    let y = y0 + py;
                    if y >= frame_height {
                        continue;
                    }
                    for px in 0..cell_w {
                        let x = x0 + px;
                        if x >= frame_width {
                            continue;
                        }
                        if px > 0
                            && py > 0
                            && px < cell_w.saturating_sub(1)
                            && py < cell_h.saturating_sub(1)
                        {
                            continue;
                        }
                        let idx = (y * frame_width + x) * 4;
                        pixels[idx] = color.0;
                        pixels[idx + 1] = color.1;
                        pixels[idx + 2] = color.2;
                        pixels[idx + 3] = 255;
                    }
                }
            }
        },
        CursorVisualShape::Bar => {
            let bar_width =
                ((cell_w.saturating_mul(usize::from(bar_width_pct.clamp(1, 100)))) / 100).max(1);
            for py in 0..cell_h {
                let y = y0 + py;
                if y >= frame_height {
                    continue;
                }
                for px in 0..bar_width {
                    let x = x0 + px;
                    if x >= frame_width {
                        continue;
                    }
                    let idx = (y * frame_width + x) * 4;
                    pixels[idx] = color.0;
                    pixels[idx + 1] = color.1;
                    pixels[idx + 2] = color.2;
                    pixels[idx + 3] = 255;
                }
            }
        }
        CursorVisualShape::Underline => {
            let line_height =
                ((cell_h.saturating_mul(usize::from(underline_height_pct.clamp(1, 100)))) / 100)
                    .max(1);
            let start_y = y0 + cell_h.saturating_sub(line_height);
            for py in start_y..(start_y + line_height) {
                if py >= frame_height {
                    continue;
                }
                for px in 0..cell_w {
                    let x = x0 + px;
                    if x >= frame_width {
                        continue;
                    }
                    let idx = (py * frame_width + x) * 4;
                    pixels[idx] = color.0;
                    pixels[idx + 1] = color.1;
                    pixels[idx + 2] = color.2;
                    pixels[idx + 3] = 255;
                }
            }
        }
    }
    let _ = cell_foreground;
    (resolved_paint_mode, resolved_text_mode, fallback_reason)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn export_recording_gif(
    events: &[DisplayTrackEnvelope],
    output: &str,
    speed: f64,
    fps: u32,
    max_duration: Option<u64>,
    max_frames: Option<u32>,
    terminal_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    recording_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    host_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    renderer: RecordingRenderMode,
    cell_size: Option<(u16, u16)>,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
    font_family: Option<&str>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    font_path: &[String],
    palette_source: RecordingPaletteSource,
    palette_foreground: Option<&str>,
    palette_background: Option<&str>,
    palette_color: &[String],
    cursor_mode: RecordingCursorMode,
    cursor_shape: RecordingCursorShape,
    cursor_blink: RecordingCursorBlinkMode,
    cursor_blink_period_ms: u32,
    cursor_color: &str,
    cursor_profile: RecordingCursorProfile,
    cursor_solid_after_activity_ms: Option<u32>,
    cursor_solid_after_input_ms: Option<u32>,
    cursor_solid_after_output_ms: Option<u32>,
    cursor_solid_after_cursor_ms: Option<u32>,
    cursor_paint_mode: RecordingCursorPaintMode,
    cursor_text_mode: RecordingCursorTextMode,
    cursor_bar_width_pct: u8,
    cursor_underline_height_pct: u8,
    export_metadata: Option<&str>,
    show_progress: bool,
) -> Result<()> {
    let mut profiler = ExportProfiler::new();
    let speed = if speed <= 0.0 { 1.0 } else { speed };
    let fps = fps.max(1);
    let frame_interval_ns = (1_000_000_000_f64 / f64::from(fps)) as u64;
    let estimate = estimate_export_progress(events, speed, fps, max_duration, max_frames);
    let mut progress = ExportProgress::new(show_progress, estimate);
    let profile_defaults = terminal_profile.map(|profile| &profile.cursor_defaults);
    let resolved_shape = if matches!(cursor_shape, RecordingCursorShape::Auto) {
        profile_defaults
            .and_then(|defaults| defaults.shape)
            .map_or(cursor_shape, |shape| match shape {
                terminal_profile::CursorDefaultShape::Block => RecordingCursorShape::Block,
                terminal_profile::CursorDefaultShape::Bar => RecordingCursorShape::Bar,
                terminal_profile::CursorDefaultShape::Underline => RecordingCursorShape::Underline,
            })
    } else {
        cursor_shape
    };
    let resolved_blink = if matches!(cursor_blink, RecordingCursorBlinkMode::Auto) {
        profile_defaults
            .and_then(|defaults| defaults.blink)
            .map_or(cursor_blink, |blink| match blink {
                terminal_profile::CursorDefaultBlink::On => RecordingCursorBlinkMode::On,
                terminal_profile::CursorDefaultBlink::Off => RecordingCursorBlinkMode::Off,
            })
    } else {
        cursor_blink
    };
    let resolved_profile = if matches!(cursor_profile, RecordingCursorProfile::Auto) {
        profile_defaults
            .and_then(|defaults| defaults.profile)
            .map_or(RecordingCursorProfile::Generic, |profile| match profile {
                terminal_profile::CursorDefaultProfile::Ghostty => RecordingCursorProfile::Ghostty,
                terminal_profile::CursorDefaultProfile::Generic => RecordingCursorProfile::Generic,
            })
    } else {
        cursor_profile
    };
    let resolved_paint_mode = if matches!(cursor_paint_mode, RecordingCursorPaintMode::Auto) {
        profile_defaults
            .and_then(|defaults| defaults.paint_mode)
            .map_or(
                match resolved_profile {
                    RecordingCursorProfile::Ghostty => RecordingCursorPaintMode::Fill,
                    _ => RecordingCursorPaintMode::Invert,
                },
                |mode| match mode {
                    terminal_profile::CursorDefaultPaintMode::Invert => {
                        RecordingCursorPaintMode::Invert
                    }
                    terminal_profile::CursorDefaultPaintMode::Fill => {
                        RecordingCursorPaintMode::Fill
                    }
                    terminal_profile::CursorDefaultPaintMode::Outline => {
                        RecordingCursorPaintMode::Outline
                    }
                },
            )
    } else {
        cursor_paint_mode
    };
    let resolved_text_mode = if matches!(cursor_text_mode, RecordingCursorTextMode::Auto) {
        profile_defaults
            .and_then(|defaults| defaults.text_mode)
            .map_or(
                match resolved_profile {
                    RecordingCursorProfile::Ghostty => RecordingCursorTextMode::SwapFgBg,
                    _ => RecordingCursorTextMode::ForceContrast,
                },
                |mode| match mode {
                    terminal_profile::CursorDefaultTextMode::SwapFgBg => {
                        RecordingCursorTextMode::SwapFgBg
                    }
                    terminal_profile::CursorDefaultTextMode::ForceContrast => {
                        RecordingCursorTextMode::ForceContrast
                    }
                },
            )
    } else {
        cursor_text_mode
    };
    let resolved_bar_width_pct = profile_defaults
        .and_then(|defaults| defaults.bar_width_pct)
        .unwrap_or(cursor_bar_width_pct)
        .clamp(1, 100);
    let resolved_underline_height_pct = profile_defaults
        .and_then(|defaults| defaults.underline_height_pct)
        .unwrap_or(cursor_underline_height_pct)
        .clamp(1, 100);
    let resolved_solid_after_input_ms = cursor_solid_after_input_ms
        .or(cursor_solid_after_activity_ms)
        .or_else(|| profile_defaults.and_then(|defaults| defaults.solid_after_input_ms))
        .unwrap_or(500);
    let resolved_solid_after_output_ms = cursor_solid_after_output_ms
        .or(cursor_solid_after_activity_ms)
        .or_else(|| profile_defaults.and_then(|defaults| defaults.solid_after_output_ms))
        .unwrap_or(500);
    let resolved_solid_after_cursor_ms = cursor_solid_after_cursor_ms
        .or(cursor_solid_after_activity_ms)
        .or_else(|| profile_defaults.and_then(|defaults| defaults.solid_after_cursor_ms))
        .unwrap_or(500);
    let color_input = cursor_color.trim();
    let (resolved_color_label, resolved_color_override) =
        if color_input.is_empty() || color_input.eq_ignore_ascii_case("auto") {
            profile_defaults
                .and_then(|defaults| defaults.color.as_deref())
                .map_or_else(
                    || ("auto".to_string(), None),
                    |profile_color| {
                        let parsed = parse_cursor_color(profile_color).ok().flatten();
                        if parsed.is_some() {
                            (profile_color.to_string(), parsed)
                        } else {
                            ("auto".to_string(), None)
                        }
                    },
                )
        } else {
            (color_input.to_string(), parse_cursor_color(color_input)?)
        };

    let cursor_options = CursorExportOptions {
        mode: cursor_mode,
        shape: resolved_shape,
        blink: resolved_blink,
        profile: resolved_profile,
        blink_period_ns: u64::from(cursor_blink_period_ms.max(1)).saturating_mul(1_000_000),
        solid_after_input_ns: u64::from(resolved_solid_after_input_ms).saturating_mul(1_000_000),
        solid_after_output_ns: u64::from(resolved_solid_after_output_ms).saturating_mul(1_000_000),
        solid_after_cursor_ns: u64::from(resolved_solid_after_cursor_ms).saturating_mul(1_000_000),
        paint_mode: resolved_paint_mode,
        text_mode: resolved_text_mode,
        bar_width_pct: resolved_bar_width_pct,
        underline_height_pct: resolved_underline_height_pct,
        color_label: resolved_color_label,
        color_override: resolved_color_override,
    };

    let (max_cols, max_rows) = infer_export_terminal_bounds(events)?;

    let cell_metrics = resolve_export_cell_metrics(events, cell_size, cell_width, cell_height)?;
    let cell_w = cell_metrics.width;
    let cell_h = cell_metrics.height;
    let width = max_cols.saturating_mul(cell_w).max(8);
    let height = max_rows.saturating_mul(cell_h).max(8);
    let render_options = build_render_options(
        terminal_profile,
        renderer,
        font_family,
        font_size,
        line_height,
        font_path,
    )?;
    let renderer_init_started_at = profiler.stage_started();
    let palette = resolve_export_palette(
        palette_source,
        recording_profile,
        host_profile,
        palette_foreground,
        palette_background,
        palette_color,
    )?;
    let mut glyph_renderer = match render_options.mode {
        RecordingRenderMode::Font => GlyphRenderer::new(cell_w, cell_h, &render_options),
        RecordingRenderMode::Bitmap => None,
    };
    let mut resvg_renderer = match render_options.mode {
        RecordingRenderMode::Font => Some(
            ResvgFrameRenderer::new(max_rows, max_cols, cell_w, cell_h, &render_options)
                .map_err(|error| {
                    profiler.note_resvg_fallback();
                    tracing::warn!(
                        "recording export: resvg renderer init failed, falling back to bitmap: {error:#}"
                    );
                    error
                })
                .ok(),
        ),
        RecordingRenderMode::Bitmap => None,
    }
    .flatten();
    let mut bitmap_cache = BitmapGlyphCache::new(usize::from(cell_w), usize::from(cell_h));
    profiler.record_renderer_init(renderer_init_started_at);

    let output_path = PathBuf::from(output);
    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating export parent directory {}",
                parent.display()
            )
        })?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&output_path)
        .with_context(|| format!("failed opening export output {}", output_path.display()))?;
    let mut encoder =
        GifEncoder::new(file, width, height, &[]).context("failed creating gif encoder")?;
    encoder
        .set_repeat(Repeat::Infinite)
        .context("failed setting gif repeat")?;

    let mut parser = vt100::Parser::new(max_rows, max_cols, 20_000);
    let mut current_cols = max_cols;
    let mut current_rows = max_rows;
    let mut emitted_frames = 0_u32;
    let mut processed_frame_events = 0_u32;
    let mut previous_emit_ns = None::<u64>;
    let mut cursor_state = CursorReplayState::default();
    let mut snapshot_cursor_state = None::<RecordedCursorSnapshot>;
    let mut cursor_frames = export_metadata.map(|_| Vec::<ExportCursorFrame>::new());
    let mut blink_anchor_ns = None::<u64>;
    let mut last_input_activity_ns = None::<u64>;
    let mut last_output_activity_ns = None::<u64>;
    let mut last_cursor_activity_ns = None::<u64>;
    let mut warned_cursor_snapshot_fallback = false;
    let mut previous_visual_state = None::<FrameVisualState>;
    let start_mono_ns = events.iter().map(|event| event.mono_ns).min().unwrap_or(0);
    let frame_cutoff_ns = max_frames.map(|limit| {
        if limit == 0 {
            0_u64
        } else {
            u64::from(limit.saturating_sub(1)).saturating_mul(frame_interval_ns)
        }
    });
    let mut considered_event_count = 0_usize;
    let mut end_scaled_ns = 0_u64;
    for event in events {
        let rel_mono_ns = event.mono_ns.saturating_sub(start_mono_ns);
        if let Some(limit_secs) = max_duration
            && rel_mono_ns / 1_000_000_000 > limit_secs
        {
            break;
        }
        let scaled_ns = ((rel_mono_ns as f64) / speed) as u64;
        if let Some(cutoff) = frame_cutoff_ns
            && scaled_ns > cutoff
        {
            break;
        }
        considered_event_count = considered_event_count.saturating_add(1);
        end_scaled_ns = scaled_ns;
    }

    let max_timeline_frames = if considered_event_count == 0 {
        0_u32
    } else {
        let base = end_scaled_ns
            .saturating_div(frame_interval_ns.max(1))
            .saturating_add(1);
        base.min(u64::from(u32::MAX)) as u32
    };
    let target_frames =
        max_frames.map_or(max_timeline_frames, |limit| limit.min(max_timeline_frames));

    let mut event_index = 0_usize;
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    let mut active_images: Vec<bmux_ipc::AttachPaneImage> = Vec::new();
    for frame_idx in 0..target_frames {
        profiler.record_frame_considered();
        let frame_time_ns = u64::from(frame_idx).saturating_mul(frame_interval_ns);
        let apply_started_at = profiler.stage_started();
        let mut frame_had_display_change = false;
        while event_index < considered_event_count {
            let event = &events[event_index];
            let rel_mono_ns = event.mono_ns.saturating_sub(start_mono_ns);
            let scaled_ns = ((rel_mono_ns as f64) / speed) as u64;
            if scaled_ns > frame_time_ns {
                break;
            }
            match &event.event {
                DisplayTrackEvent::Resize { cols, rows } => {
                    current_cols = (*cols).max(1);
                    current_rows = (*rows).max(1);
                    parser.screen_mut().set_size(current_rows, current_cols);
                    frame_had_display_change = true;
                }
                DisplayTrackEvent::FrameBytes { data } => {
                    update_cursor_replay_state(&mut cursor_state, data);
                    parser.process(data);
                    processed_frame_events = processed_frame_events.saturating_add(1);
                    frame_had_display_change = true;
                }
                DisplayTrackEvent::CursorSnapshot {
                    x,
                    y,
                    visible,
                    shape,
                    blink_enabled,
                } => {
                    snapshot_cursor_state = Some(RecordedCursorSnapshot {
                        x: *x,
                        y: *y,
                        visible: *visible,
                        shape: *shape,
                        blink_enabled: *blink_enabled,
                    });
                    frame_had_display_change = true;
                }
                DisplayTrackEvent::Activity { kind } => match kind {
                    bmux_ipc::DisplayActivityKind::Input => {
                        last_input_activity_ns = Some(scaled_ns);
                    }
                    bmux_ipc::DisplayActivityKind::Output => {
                        last_output_activity_ns = Some(scaled_ns);
                    }
                    bmux_ipc::DisplayActivityKind::Cursor => {
                        last_cursor_activity_ns = Some(scaled_ns);
                    }
                },
                DisplayTrackEvent::StreamOpened { .. } | DisplayTrackEvent::StreamClosed => {}
                DisplayTrackEvent::ImageUpdate { images } => {
                    #[cfg(any(
                        feature = "image-sixel",
                        feature = "image-kitty",
                        feature = "image-iterm2"
                    ))]
                    {
                        active_images.clone_from(images);
                    }
                    let _ = images; // suppress unused warning when no image features
                    frame_had_display_change = true;
                }
            }
            event_index = event_index.saturating_add(1);
        }
        profiler.record_apply_events(apply_started_at);

        if processed_frame_events == 0 {
            progress.update(processed_frame_events, emitted_frames, false);
            continue;
        }

        let (snapshot, cursor_source) = snapshot_cursor_state.map_or_else(
            || {
                if !warned_cursor_snapshot_fallback {
                    tracing::warn!(
                        "recording export: display track missing initial cursor snapshot; using parser cursor fallback until snapshots appear"
                    );
                    warned_cursor_snapshot_fallback = true;
                }
                (
                    cursor_snapshot_from_parser_fallback(&parser, cursor_state),
                    "parser_fallback",
                )
            },
            |snapshot| (snapshot, "snapshot"),
        );
        let cursor_row = snapshot.y;
        let cursor_col = snapshot.x;
        let parser_cursor_visible = snapshot.visible;
        let shape = effective_cursor_shape(&cursor_options, cursor_state, snapshot.shape);
        let (cursor_visible, blink_on, visible_reason) = compute_cursor_visibility(
            &cursor_options,
            cursor_state,
            snapshot.blink_enabled,
            parser_cursor_visible,
            frame_time_ns,
            last_input_activity_ns,
            last_output_activity_ns,
            last_cursor_activity_ns,
            &mut blink_anchor_ns,
        );
        let visual_state = FrameVisualState {
            rows: current_rows,
            cols: current_cols,
            cursor_row,
            cursor_col,
            cursor_visible,
            shape,
            blink_on,
        };
        if !frame_had_display_change && previous_visual_state == Some(visual_state) {
            profiler.record_frame_skipped();
            progress.update(processed_frame_events, emitted_frames, false);
            continue;
        }

        let delay_cs = previous_emit_ns.map_or(1_u16, |previous| {
            let delta_ns = frame_time_ns.saturating_sub(previous);
            ((delta_ns / 10_000_000).max(1).min(u64::from(u16::MAX))) as u16
        });
        let render_started_at = profiler.stage_started();
        let mut pixels = if render_options.mode == RecordingRenderMode::Font {
            if let Some(renderer) = resvg_renderer.as_mut() {
                match renderer.render(parser.screen(), current_rows, current_cols, &palette) {
                    Ok(pixels) => pixels,
                    Err(error) => {
                        profiler.note_resvg_fallback();
                        tracing::warn!(
                            "recording export: resvg frame render failed, falling back to bitmap: {error:#}"
                        );
                        resvg_renderer = None;
                        render_screen_rgba(
                            parser.screen(),
                            current_rows,
                            current_cols,
                            max_rows,
                            max_cols,
                            cell_w,
                            cell_h,
                            &palette,
                            glyph_renderer.as_mut(),
                            &mut bitmap_cache,
                        )
                    }
                }
            } else {
                render_screen_rgba(
                    parser.screen(),
                    current_rows,
                    current_cols,
                    max_rows,
                    max_cols,
                    cell_w,
                    cell_h,
                    &palette,
                    glyph_renderer.as_mut(),
                    &mut bitmap_cache,
                )
            }
        } else {
            render_screen_rgba(
                parser.screen(),
                current_rows,
                current_cols,
                max_rows,
                max_cols,
                cell_w,
                cell_h,
                &palette,
                glyph_renderer.as_mut(),
                &mut bitmap_cache,
            )
        };

        // Overlay decoded images onto the rasterized text frame.
        #[cfg(any(
            feature = "image-sixel",
            feature = "image-kitty",
            feature = "image-iterm2"
        ))]
        if !active_images.is_empty() {
            overlay_display_track_images(
                &mut pixels,
                u32::from(width),
                u32::from(height),
                u32::from(cell_w),
                u32::from(cell_h),
                &active_images,
            );
        }

        if cursor_visible && cursor_row < current_rows && cursor_col < current_cols {
            let (cell_foreground, cell_background) = parser
                .screen()
                .cell(cursor_row, cursor_col)
                .map_or(((255, 255, 255), (0, 0, 0)), |cell| {
                    resolved_cell_colors(cell, &palette)
                });
            let cursor_color_rgb = cursor_options.color_override.unwrap_or(cell_foreground);
            let (paint_mode_used, text_mode_used, paint_fallback_reason) = overlay_cursor_rgba(
                &mut pixels,
                usize::from(width),
                usize::from(height),
                usize::from(cell_w),
                usize::from(cell_h),
                cursor_row,
                cursor_col,
                shape,
                match cursor_options.paint_mode {
                    RecordingCursorPaintMode::Auto | RecordingCursorPaintMode::Invert => {
                        BlockPaintMode::Invert
                    }
                    RecordingCursorPaintMode::Fill => BlockPaintMode::Fill,
                    RecordingCursorPaintMode::Outline => BlockPaintMode::Outline,
                },
                match cursor_options.text_mode {
                    RecordingCursorTextMode::Auto | RecordingCursorTextMode::SwapFgBg => {
                        BlockTextMode::SwapFgBg
                    }
                    RecordingCursorTextMode::ForceContrast => BlockTextMode::ForceContrast,
                },
                cursor_options.bar_width_pct,
                cursor_options.underline_height_pct,
                cell_foreground,
                cell_background,
                cursor_color_rgb,
            );
            if let Some(frames) = cursor_frames.as_mut() {
                frames.push(ExportCursorFrame {
                    mono_ns: frame_time_ns,
                    row: cursor_row,
                    col: cursor_col,
                    visible: cursor_visible,
                    shape: cursor_shape_name(shape),
                    blink_on,
                    cursor_source,
                    visible_reason,
                    paint_mode_used: paint_mode_name(paint_mode_used),
                    text_mode_used: text_mode_name(text_mode_used),
                    paint_fallback_reason,
                    last_input_activity_ns,
                    last_output_activity_ns,
                    last_cursor_activity_ns,
                });
            }
        } else if let Some(frames) = cursor_frames.as_mut() {
            frames.push(ExportCursorFrame {
                mono_ns: frame_time_ns,
                row: cursor_row,
                col: cursor_col,
                visible: cursor_visible,
                shape: cursor_shape_name(shape),
                blink_on,
                cursor_source,
                visible_reason,
                paint_mode_used: paint_mode_name(match cursor_options.paint_mode {
                    RecordingCursorPaintMode::Auto | RecordingCursorPaintMode::Invert => {
                        BlockPaintMode::Invert
                    }
                    RecordingCursorPaintMode::Fill => BlockPaintMode::Fill,
                    RecordingCursorPaintMode::Outline => BlockPaintMode::Outline,
                }),
                text_mode_used: text_mode_name(match cursor_options.text_mode {
                    RecordingCursorTextMode::Auto | RecordingCursorTextMode::SwapFgBg => {
                        BlockTextMode::SwapFgBg
                    }
                    RecordingCursorTextMode::ForceContrast => BlockTextMode::ForceContrast,
                }),
                paint_fallback_reason: None,
                last_input_activity_ns,
                last_output_activity_ns,
                last_cursor_activity_ns,
            });
        }
        profiler.record_render(render_started_at);
        let encode_started_at = profiler.stage_started();
        let mut frame = GifFrame::from_rgba_speed(width, height, &mut pixels, 1);
        frame.delay = delay_cs;
        encoder
            .write_frame(&frame)
            .context("failed writing gif frame")?;
        profiler.record_encode(encode_started_at);
        previous_visual_state = Some(visual_state);
        previous_emit_ns = Some(frame_time_ns);
        emitted_frames = emitted_frames.saturating_add(1);
        profiler.record_frame_emitted();
        progress.update(processed_frame_events, emitted_frames, false);
    }

    progress.finish(processed_frame_events, emitted_frames);
    profiler.finish(processed_frame_events, emitted_frames);

    if emitted_frames == 0 {
        anyhow::bail!("no drawable frame events found in display track")
    }
    if let Some(path) = export_metadata {
        let metadata_path = PathBuf::from(path);
        if let Some(parent) = metadata_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed creating export metadata parent directory {}",
                    parent.display()
                )
            })?;
        }
        let metadata = ExportMetadata {
            format: "gif",
            output,
            fps,
            speed,
            emitted_frames,
            cursor: CursorMetadata {
                mode: match cursor_options.mode {
                    RecordingCursorMode::Auto => "auto",
                    RecordingCursorMode::On => "on",
                    RecordingCursorMode::Off => "off",
                },
                shape: match cursor_options.shape {
                    RecordingCursorShape::Auto => "auto",
                    RecordingCursorShape::Block => "block",
                    RecordingCursorShape::Bar => "bar",
                    RecordingCursorShape::Underline => "underline",
                },
                blink: match cursor_options.blink {
                    RecordingCursorBlinkMode::Auto => "auto",
                    RecordingCursorBlinkMode::On => "on",
                    RecordingCursorBlinkMode::Off => "off",
                },
                profile: match cursor_options.profile {
                    RecordingCursorProfile::Auto => "auto",
                    RecordingCursorProfile::Ghostty => "ghostty",
                    RecordingCursorProfile::Generic => "generic",
                },
                blink_period_ms: cursor_blink_period_ms.max(1),
                solid_after_input_ms: resolved_solid_after_input_ms,
                solid_after_output_ms: resolved_solid_after_output_ms,
                solid_after_cursor_ms: resolved_solid_after_cursor_ms,
                paint_mode: match cursor_options.paint_mode {
                    RecordingCursorPaintMode::Auto => "auto",
                    RecordingCursorPaintMode::Invert => "invert",
                    RecordingCursorPaintMode::Fill => "fill",
                    RecordingCursorPaintMode::Outline => "outline",
                },
                text_mode: match cursor_options.text_mode {
                    RecordingCursorTextMode::Auto => "auto",
                    RecordingCursorTextMode::SwapFgBg => "swap_fg_bg",
                    RecordingCursorTextMode::ForceContrast => "force_contrast",
                },
                bar_width_pct: cursor_options.bar_width_pct,
                underline_height_pct: cursor_options.underline_height_pct,
                color: &cursor_options.color_label,
            },
            frames: cursor_frames.unwrap_or_default(),
        };
        let json = serde_json::to_vec_pretty(&metadata)
            .context("failed serializing export cursor metadata")?;
        std::fs::write(&metadata_path, json).with_context(|| {
            format!("failed writing export metadata {}", metadata_path.display())
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ExportProgressEstimate {
    total_frame_events: u32,
    estimated_emitted_frames: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameVisualState {
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    cursor_visible: bool,
    shape: CursorVisualShape,
    blink_on: bool,
}

#[derive(Debug)]
struct ExportProfiler {
    enabled: bool,
    started_at: Instant,
    renderer_init: std::time::Duration,
    apply_events: std::time::Duration,
    render: std::time::Duration,
    encode: std::time::Duration,
    frames_considered: u32,
    frames_emitted: u32,
    frames_skipped: u32,
    resvg_fallbacks: u32,
}

impl ExportProfiler {
    fn new() -> Self {
        let enabled = std::env::var("BMUX_RECORDING_EXPORT_PROFILE")
            .ok()
            .is_some_and(|value| {
                let normalized = value.trim().to_ascii_lowercase();
                matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
            });
        Self {
            enabled,
            started_at: Instant::now(),
            renderer_init: std::time::Duration::ZERO,
            apply_events: std::time::Duration::ZERO,
            render: std::time::Duration::ZERO,
            encode: std::time::Duration::ZERO,
            frames_considered: 0,
            frames_emitted: 0,
            frames_skipped: 0,
            resvg_fallbacks: 0,
        }
    }

    fn stage_started(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    fn record_renderer_init(&mut self, started_at: Option<Instant>) {
        if let Some(started_at) = started_at {
            self.renderer_init += started_at.elapsed();
        }
    }

    fn record_apply_events(&mut self, started_at: Option<Instant>) {
        if let Some(started_at) = started_at {
            self.apply_events += started_at.elapsed();
        }
    }

    fn record_render(&mut self, started_at: Option<Instant>) {
        if let Some(started_at) = started_at {
            self.render += started_at.elapsed();
        }
    }

    fn record_encode(&mut self, started_at: Option<Instant>) {
        if let Some(started_at) = started_at {
            self.encode += started_at.elapsed();
        }
    }

    const fn record_frame_considered(&mut self) {
        self.frames_considered = self.frames_considered.saturating_add(1);
    }

    const fn record_frame_emitted(&mut self) {
        self.frames_emitted = self.frames_emitted.saturating_add(1);
    }

    const fn record_frame_skipped(&mut self) {
        self.frames_skipped = self.frames_skipped.saturating_add(1);
    }

    const fn note_resvg_fallback(&mut self) {
        self.resvg_fallbacks = self.resvg_fallbacks.saturating_add(1);
    }

    fn finish(&self, processed_frame_events: u32, emitted_frames: u32) {
        if !self.enabled {
            return;
        }
        let elapsed = self.started_at.elapsed();
        let considered = self.frames_considered.max(1);
        let avg_render_ms = self.render.as_secs_f64() * 1000.0 / f64::from(considered);
        let avg_encode_ms = self.encode.as_secs_f64() * 1000.0 / f64::from(considered);
        tracing::info!(
            "recording export profile: elapsed={} init={} apply={} render={} encode={} frames_considered={} frames_emitted={} frames_skipped={} processed_frame_events={} emitted_frames={} resvg_fallbacks={} avg_render_ms={avg_render_ms:.3} avg_encode_ms={avg_encode_ms:.3}",
            format_duration_compact(elapsed),
            format_duration_compact(self.renderer_init),
            format_duration_compact(self.apply_events),
            format_duration_compact(self.render),
            format_duration_compact(self.encode),
            self.frames_considered,
            self.frames_emitted,
            self.frames_skipped,
            processed_frame_events,
            emitted_frames,
            self.resvg_fallbacks,
        );
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn estimate_export_progress(
    events: &[DisplayTrackEnvelope],
    speed: f64,
    fps: u32,
    max_duration: Option<u64>,
    max_frames: Option<u32>,
) -> ExportProgressEstimate {
    let speed = if speed <= 0.0 { 1.0 } else { speed };
    let frame_interval_ns = (1_000_000_000_f64 / f64::from(fps.max(1))) as u64;
    let mut total_frame_events = 0_u32;
    let mut considered_event_count = 0_u32;
    let start_mono_ns = events.iter().map(|event| event.mono_ns).min().unwrap_or(0);
    let frame_cutoff_ns = max_frames.map(|limit| {
        if limit == 0 {
            0_u64
        } else {
            u64::from(limit.saturating_sub(1)).saturating_mul(frame_interval_ns)
        }
    });
    let mut end_scaled_ns = 0_u64;

    for event in events {
        let rel_mono_ns = event.mono_ns.saturating_sub(start_mono_ns);
        if let Some(limit_secs) = max_duration
            && rel_mono_ns / 1_000_000_000 > limit_secs
        {
            break;
        }
        let scaled_ns = ((rel_mono_ns as f64) / speed) as u64;
        if let Some(cutoff) = frame_cutoff_ns
            && scaled_ns > cutoff
        {
            break;
        }
        considered_event_count = considered_event_count.saturating_add(1);
        end_scaled_ns = scaled_ns;
        if let DisplayTrackEvent::FrameBytes { .. } = event.event {
            total_frame_events = total_frame_events.saturating_add(1);
        }
    }

    let base_emitted_frames = if considered_event_count == 0 || total_frame_events == 0 {
        0_u32
    } else {
        end_scaled_ns
            .saturating_div(frame_interval_ns.max(1))
            .saturating_add(1)
            .min(u64::from(u32::MAX)) as u32
    };
    let estimated_emitted_frames =
        max_frames.map_or(base_emitted_frames, |limit| limit.min(base_emitted_frames));

    ExportProgressEstimate {
        total_frame_events,
        estimated_emitted_frames,
    }
}

struct ExportProgress {
    enabled: bool,
    tty: bool,
    started_at: Instant,
    last_update_at: Instant,
    last_line_len: usize,
    last_non_tty_bucket: Option<u32>,
    estimate: ExportProgressEstimate,
}

impl ExportProgress {
    #[allow(clippy::cast_possible_truncation)]
    fn new(show_progress: bool, estimate: ExportProgressEstimate) -> Self {
        Self {
            enabled: show_progress,
            tty: show_progress && io::stderr().is_terminal(),
            started_at: Instant::now(),
            last_update_at: Instant::now(),
            last_line_len: 0,
            last_non_tty_bucket: None,
            estimate,
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn update(&mut self, processed_frame_events: u32, emitted_frames: u32, force: bool) {
        if !self.enabled || self.estimate.total_frame_events == 0 {
            return;
        }

        let now = Instant::now();
        if !force && now.duration_since(self.last_update_at) < std::time::Duration::from_millis(300)
        {
            return;
        }

        let percent = (f64::from(processed_frame_events)
            / f64::from(self.estimate.total_frame_events.max(1))
            * 100.0)
            .clamp(0.0, 100.0);
        let elapsed = now.duration_since(self.started_at);
        let eta = estimate_eta(
            elapsed,
            processed_frame_events,
            self.estimate.total_frame_events,
        );
        let estimated_emitted = self.estimate.estimated_emitted_frames.max(emitted_frames);
        let line = format!(
            "export {percent:5.1}% events {processed_frame_events}/{} frames {emitted_frames}/{} elapsed {} eta {}",
            self.estimate.total_frame_events,
            estimated_emitted,
            format_duration_compact(elapsed),
            eta.map_or_else(|| "--:--".to_string(), format_duration_compact),
        );

        if self.tty {
            let mut padded = line;
            if self.last_line_len > padded.len() {
                padded.push_str(&" ".repeat(self.last_line_len - padded.len()));
            }
            eprint!("\r{padded}");
            let _ = io::stderr().flush();
            self.last_line_len = padded.len();
            self.last_update_at = now;
            return;
        }

        let bucket = percent.floor() as u32 / 10;
        if force
            || self
                .last_non_tty_bucket
                .is_none_or(|previous| bucket > previous)
        {
            eprintln!("{line}");
            self.last_non_tty_bucket = Some(bucket);
            self.last_update_at = now;
        }
    }

    fn finish(&mut self, processed_frame_events: u32, emitted_frames: u32) {
        self.update(processed_frame_events, emitted_frames, true);
        if self.enabled && self.tty {
            eprintln!();
        }
    }
}

fn estimate_eta(
    elapsed: std::time::Duration,
    completed: u32,
    total: u32,
) -> Option<std::time::Duration> {
    if completed == 0 || completed >= total {
        return (completed >= total).then_some(std::time::Duration::from_secs(0));
    }
    let remaining_ratio = f64::from(total.saturating_sub(completed)) / f64::from(completed);
    Some(elapsed.mul_f64(remaining_ratio))
}

fn format_duration_compact(duration: std::time::Duration) -> String {
    let total_secs = duration.as_secs();
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hours = total_secs / 3600;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins:02}:{secs:02}")
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn render_screen_rgba(
    screen: &vt100::Screen,
    rows: u16,
    cols: u16,
    max_rows: u16,
    max_cols: u16,
    cell_w: u16,
    cell_h: u16,
    palette: &ExportPalette,
    mut glyph_renderer: Option<&mut GlyphRenderer>,
    bitmap_cache: &mut BitmapGlyphCache,
) -> Vec<u8> {
    let width = usize::from(max_cols.saturating_mul(cell_w));
    let height = usize::from(max_rows.saturating_mul(cell_h));
    let mut pixels = vec![0_u8; width.saturating_mul(height).saturating_mul(4)];
    let cw = usize::from(cell_w);
    let cell_height_px = usize::from(cell_h);

    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let ((fg_r, fg_g, fg_b), (bg_r, bg_g, bg_b)) = resolved_cell_colors(cell, palette);
            let x0 = usize::from(col).saturating_mul(cw);
            let y0 = usize::from(row).saturating_mul(cell_height_px);
            for py in 0..cell_height_px {
                let y = y0 + py;
                if y >= height {
                    continue;
                }
                let row_start = y.saturating_mul(width);
                for px in 0..cw {
                    let x = x0 + px;
                    if x >= width {
                        continue;
                    }
                    let idx = (row_start + x).saturating_mul(4);
                    pixels[idx] = bg_r;
                    pixels[idx + 1] = bg_g;
                    pixels[idx + 2] = bg_b;
                    pixels[idx + 3] = 255;
                }
            }

            let glyph_char = if cell.has_contents() {
                cell.contents().chars().next().unwrap_or(' ')
            } else {
                ' '
            };
            if glyph_char == ' ' {
                continue;
            }

            let drawn_with_font = glyph_renderer.as_deref_mut().is_some_and(|renderer| {
                renderer.draw_cell(
                    &mut pixels,
                    width,
                    height,
                    x0,
                    y0,
                    glyph_char,
                    (fg_r, fg_g, fg_b),
                    (bg_r, bg_g, bg_b),
                )
            });
            if !drawn_with_font {
                draw_bitmap_glyph_rgba(
                    &mut pixels,
                    width,
                    height,
                    x0,
                    y0,
                    cw,
                    cell_height_px,
                    glyph_char,
                    (fg_r, fg_g, fg_b),
                    bitmap_cache,
                );
            }
        }
    }

    pixels
}

struct ResvgFrameRenderer {
    width: usize,
    height: usize,
    width_u32: u32,
    height_u32: u32,
    cell_width_px: usize,
    cell_height_px: usize,
    background_opacity: f32,
    backdrop_rgb: (u8, u8, u8),
    top_to_baseline: f32,
    font_size: f32,
    font_family_attr: String,
    options_usvg: usvg::Options<'static>,
    svg: String,
}

impl ResvgFrameRenderer {
    fn new(
        max_rows: u16,
        max_cols: u16,
        cell_w: u16,
        cell_h: u16,
        options: &RenderOptions,
    ) -> Result<Self> {
        let width = usize::from(max_cols.saturating_mul(cell_w));
        let height = usize::from(max_rows.saturating_mul(cell_h));
        let width_u32 = u32::try_from(width).context("render width exceeds u32")?;
        let height_u32 = u32::try_from(height).context("render height exceeds u32")?;
        let cell_width_px = usize::from(cell_w);
        let cell_height_px = usize::from(cell_h);
        let preset = font_preset_for_options(options);

        let mut families = if options.font_families.is_empty() {
            bmux_fonts::default_families_for_preset(preset)
        } else {
            options.font_families.clone()
        };
        if families.is_empty() {
            families.push("monospace".to_string());
        }

        let metrics = compute_font_grid_metrics(cell_w, cell_h, options);
        let font_size = options
            .font_size_px
            .or_else(|| metrics.as_ref().map(|value| value.font_size_px))
            .unwrap_or_else(|| (f32::from(cell_h) * 0.9).max(8.0));
        let top_to_baseline = metrics
            .as_ref()
            .map_or_else(|| f32::from(cell_h) * 0.8, |value| value.top_to_baseline_px);
        let font_family_attr = svg_font_family_list(&families);

        let font_family = families
            .first()
            .cloned()
            .unwrap_or_else(|| "monospace".to_string());
        let mut options_usvg = usvg::Options {
            font_family,
            font_size,
            ..usvg::Options::default()
        };
        let fontdb = options_usvg.fontdb_mut();
        let _ = bmux_fonts::register_preset_fonts(fontdb, preset);
        fontdb.load_system_fonts();
        for path in &options.font_paths {
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            if meta.is_dir() {
                fontdb.load_fonts_dir(path);
            } else if meta.is_file() {
                let _ = fontdb.load_font_file(path);
            }
        }

        Ok(Self {
            width,
            height,
            width_u32,
            height_u32,
            cell_width_px,
            cell_height_px,
            background_opacity: options.background_opacity,
            backdrop_rgb: options.backdrop_rgb,
            top_to_baseline,
            font_size,
            font_family_attr,
            options_usvg,
            svg: String::with_capacity(width.saturating_mul(height / 4).max(1024)),
        })
    }

    #[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
    fn render(
        &mut self,
        screen: &vt100::Screen,
        rows: u16,
        cols: u16,
        palette: &ExportPalette,
    ) -> Result<Vec<u8>> {
        self.svg.clear();
        write!(
            &mut self.svg,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">",
            self.width, self.height, self.width, self.height
        )
        .expect("svg write cannot fail");
        write!(
            &mut self.svg,
            "<g font-family=\"{}\" font-size=\"{:.3}\" text-rendering=\"optimizeLegibility\" dominant-baseline=\"alphabetic\" font-kerning=\"none\" font-variant-ligatures=\"none\">",
            xml_escape_attr(&self.font_family_attr),
            self.font_size
        )
        .expect("svg write cannot fail");

        for row in 0..rows {
            let mut row_runs = Vec::<TextRun>::new();
            let mut current_run = None::<TextRun>;
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                let (mut fg_rgb, bg_rgb) = resolved_cell_colors(cell, palette);
                if cell.dim() {
                    fg_rgb = dim_rgb(fg_rgb);
                }
                let bg_rgb =
                    composite_with_backdrop(bg_rgb, self.background_opacity, self.backdrop_rgb);
                let x0 = usize::from(col).saturating_mul(self.cell_width_px);
                let y0 = usize::from(row).saturating_mul(self.cell_height_px);
                write!(
                    &mut self.svg,
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"rgb({},{},{})\"/>",
                    x0, y0, self.cell_width_px, self.cell_height_px, bg_rgb.0, bg_rgb.1, bg_rgb.2
                )
                .expect("svg write cannot fail");

                let cell_text = if cell.has_contents() {
                    let text = cell.contents();
                    if text.is_empty() { " " } else { text }
                } else {
                    " "
                };
                let style = TextStyle {
                    fg_rgb,
                    bold: cell.bold(),
                    italic: cell.italic(),
                    underline: cell.underline(),
                };
                match current_run.take() {
                    Some(mut run) if run.style == style => {
                        run.text.push_str(cell_text);
                        run.cell_count = run.cell_count.saturating_add(1);
                        current_run = Some(run);
                    }
                    Some(run) => {
                        row_runs.push(run);
                        current_run = Some(TextRun {
                            start_col: col,
                            text: cell_text.to_string(),
                            cell_count: 1,
                            style,
                        });
                    }
                    None => {
                        current_run = Some(TextRun {
                            start_col: col,
                            text: cell_text.to_string(),
                            cell_count: 1,
                            style,
                        });
                    }
                }
            }
            if let Some(run) = current_run.take() {
                row_runs.push(run);
            }
            for run in row_runs {
                let x0 = usize::from(run.start_col).saturating_mul(self.cell_width_px);
                let y0 = usize::from(row).saturating_mul(self.cell_height_px);
                let text_y = y0 as f32 + self.top_to_baseline;
                let style_attrs = svg_style_attrs(run.style);
                let text_length = usize::from(run.cell_count).saturating_mul(self.cell_width_px);
                write!(
                    &mut self.svg,
                    "<text x=\"{}\" y=\"{:.3}\" fill=\"rgb({},{},{})\" xml:space=\"preserve\" textLength=\"{}\" lengthAdjust=\"spacingAndGlyphs\"{}>{}</text>",
                    x0,
                    text_y,
                    run.style.fg_rgb.0,
                    run.style.fg_rgb.1,
                    run.style.fg_rgb.2,
                    text_length,
                    style_attrs,
                    xml_escape_text(&run.text)
                )
                .expect("svg write cannot fail");
            }
        }

        self.svg.push_str("</g></svg>");

        let tree = usvg::Tree::from_str(&self.svg, &self.options_usvg)
            .context("failed to parse SVG frame")?;
        let mut pixmap = tiny_skia::Pixmap::new(self.width_u32, self.height_u32)
            .ok_or_else(|| anyhow::anyhow!("failed to allocate pixmap for SVG frame"))?;
        resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
        Ok(pixmap.take())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextStyle {
    fg_rgb: (u8, u8, u8),
    bold: bool,
    italic: bool,
    underline: bool,
}

#[derive(Debug, Clone)]
struct TextRun {
    start_col: u16,
    text: String,
    cell_count: u16,
    style: TextStyle,
}

fn resolved_cell_colors(
    cell: &vt100::Cell,
    palette: &ExportPalette,
) -> ((u8, u8, u8), (u8, u8, u8)) {
    let mut fg = resolve_vt100_color(cell.fgcolor(), true, palette);
    let mut bg = resolve_vt100_color(cell.bgcolor(), false, palette);
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn dim_rgb(rgb: (u8, u8, u8)) -> (u8, u8, u8) {
    (
        (f32::from(rgb.0) * 0.72).round() as u8,
        (f32::from(rgb.1) * 0.72).round() as u8,
        (f32::from(rgb.2) * 0.72).round() as u8,
    )
}

fn composite_with_backdrop(
    rgb: (u8, u8, u8),
    opacity: f32,
    backdrop_rgb: (u8, u8, u8),
) -> (u8, u8, u8) {
    if opacity >= 0.999 {
        return rgb;
    }
    let alpha = opacity.clamp(0.0, 1.0);
    (
        blend_channel(rgb.0, backdrop_rgb.0, alpha),
        blend_channel(rgb.1, backdrop_rgb.1, alpha),
        blend_channel(rgb.2, backdrop_rgb.2, alpha),
    )
}

fn svg_style_attrs(style: TextStyle) -> String {
    let mut attrs = String::new();
    if style.bold {
        attrs.push_str(" font-weight=\"700\"");
    }
    if style.italic {
        attrs.push_str(" font-style=\"italic\"");
    }
    if style.underline {
        attrs.push_str(" text-decoration=\"underline\"");
    }
    attrs
}

struct FontGridMetrics {
    font_size_px: f32,
    top_to_baseline_px: f32,
}

struct PrimaryFontSource {
    font: FontArc,
    bytes: Vec<u8>,
    face_index: u32,
}

fn compute_font_grid_metrics(
    cell_w: u16,
    cell_h: u16,
    options: &RenderOptions,
) -> Option<FontGridMetrics> {
    let primary = primary_font_source_for_metrics(options)?;
    let unit_scale = PxScale { x: 1.0, y: 1.0 };
    let unit_face_width = ascii_cell_width(&primary.font, unit_scale).max(0.0001);
    let (unit_ascent, unit_descent, unit_line_gap) =
        font_vertical_metrics_px(&primary.bytes, primary.face_index, 1.0).unwrap_or_else(|| {
            let scaled = primary.font.as_scaled(unit_scale);
            (scaled.ascent(), scaled.descent(), scaled.line_gap())
        });
    let unit_face_height = (unit_ascent - unit_descent + unit_line_gap).max(0.0001);
    let target_w = f32::from(cell_w).max(1.0);
    let target_h = f32::from(cell_h).max(1.0);
    let font_size =
        solve_font_size_for_target_cells(unit_face_width, unit_face_height, target_w, target_h)?;

    let (ascent, descent, line_gap) =
        font_vertical_metrics_px(&primary.bytes, primary.face_index, font_size).unwrap_or_else(
            || {
                let scaled = primary.font.as_scaled(PxScale {
                    x: font_size,
                    y: font_size,
                });
                (scaled.ascent(), scaled.descent(), scaled.line_gap())
            },
        );
    let face_height = (ascent - descent + line_gap).max(0.0001);
    let half_line_gap = line_gap / 2.0;
    let face_baseline = half_line_gap - descent;
    let cell_height = target_h;
    let cell_baseline = (face_baseline - (cell_height - face_height) / 2.0).round();
    let top_to_baseline = (cell_height - cell_baseline).max(0.0);

    Some(FontGridMetrics {
        font_size_px: font_size,
        top_to_baseline_px: top_to_baseline,
    })
}

fn font_vertical_metrics_px(
    font_data: &[u8],
    face_index: u32,
    size_px: f32,
) -> Option<(f32, f32, f32)> {
    if !(size_px.is_finite() && size_px > 0.0) {
        return None;
    }
    let face = ttf_parser::Face::parse(font_data, face_index).ok()?;
    let units_per_em = f32::from(face.units_per_em()).max(1.0);
    let px_per_unit = size_px / units_per_em;
    let ascent = f32::from(face.ascender()) * px_per_unit;
    let descent = f32::from(face.descender()) * px_per_unit;
    let line_gap = f32::from(face.line_gap()) * px_per_unit;
    Some((ascent, descent, line_gap))
}

fn ascii_cell_width(font: &FontArc, scale: PxScale) -> f32 {
    let scaled = font.as_scaled(scale);
    let mut max_advance = 0.0_f32;
    for codepoint in 32_u32..127_u32 {
        let Some(ch) = char::from_u32(codepoint) else {
            continue;
        };
        let glyph_id = font.glyph_id(ch);
        if glyph_id.0 == 0 {
            continue;
        }
        max_advance = max_advance.max(scaled.h_advance(glyph_id));
    }
    if max_advance <= 0.0 {
        scaled.h_advance(font.glyph_id('M')).max(0.0001)
    } else {
        max_advance
    }
}

fn solve_font_size_for_target_cells(
    unit_w: f32,
    unit_h: f32,
    target_w: f32,
    target_h: f32,
) -> Option<f32> {
    if !(unit_w.is_finite() && unit_h.is_finite() && unit_w > 0.0 && unit_h > 0.0) {
        return None;
    }

    let h_lo = ((target_h - 0.5) / unit_h).max(0.001);
    let h_hi = (target_h + 0.5) / unit_h;
    if h_lo < h_hi {
        let preferred = target_w / unit_w;
        let size = preferred.clamp(h_lo, h_hi - f32::EPSILON);
        return Some(size.max(0.001));
    }

    let mut candidates = Vec::new();
    candidates.push((target_w / unit_w).max(0.001));
    candidates.push((target_h / unit_h).max(0.001));
    let w_lo = ((target_w - 0.5) / unit_w).max(0.001);
    let w_hi = (target_w + 0.5) / unit_w;
    candidates.push(w_lo);
    candidates.push(w_hi.max(0.001));
    candidates.push(h_lo);
    candidates.push(h_hi.max(0.001));

    let mut best = None::<(f32, f32)>;
    for candidate in candidates {
        if !candidate.is_finite() || candidate <= 0.0 {
            continue;
        }
        let width_err = (unit_w * candidate).round() - target_w;
        let height_err = (unit_h * candidate).round() - target_h;
        let score = height_err.abs().mul_add(2.0, width_err.abs());
        if best.is_none_or(|(_, best_score)| score < best_score) {
            best = Some((candidate, score));
        }
    }

    best.map(|(value, _)| value)
}

fn primary_font_source_for_metrics(options: &RenderOptions) -> Option<PrimaryFontSource> {
    let preset = font_preset_for_options(options);

    let mut db = fontdb::Database::new();
    let _ = bmux_fonts::register_preset_fonts(&mut db, preset);
    db.load_system_fonts();
    for path in &options.font_paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if meta.is_dir() {
            db.load_fonts_dir(path);
        } else if meta.is_file() {
            let _ = db.load_font_file(path);
        }
    }

    let mut families = Vec::<String>::new();
    if !options.font_families.is_empty() {
        families.extend(options.font_families.iter().cloned());
    }
    families.extend(bmux_fonts::default_families_for_preset(preset));
    let mut seen = HashSet::<String>::new();
    for family in families {
        let normalized = family.trim().to_ascii_lowercase();
        if normalized.is_empty() || !seen.insert(normalized) {
            continue;
        }
        if let Some(source) = load_font_family_source_from_db(&db, &family) {
            return Some(source);
        }
    }

    for path in &options.font_paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if let Ok(bytes) = std::fs::read(path)
            && let Some(source) = primary_font_source_from_bytes(bytes, None)
        {
            return Some(source);
        }
    }

    for embedded in bmux_fonts::bundled_fonts_for_preset(preset) {
        if let Some(source) = primary_font_source_from_bytes(embedded.data.to_vec(), None) {
            return Some(source);
        }
    }

    None
}

fn primary_font_source_from_bytes(
    bytes: Vec<u8>,
    preferred_face_index: Option<u32>,
) -> Option<PrimaryFontSource> {
    if let Some(face_index) = preferred_face_index
        && let Ok(font) = FontVec::try_from_vec_and_index(bytes.clone(), face_index)
    {
        return Some(PrimaryFontSource {
            font: FontArc::new(font),
            bytes,
            face_index,
        });
    }

    let face_count = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
    for face_index in 0..face_count {
        let Ok(font) = FontVec::try_from_vec_and_index(bytes.clone(), face_index) else {
            continue;
        };
        return Some(PrimaryFontSource {
            font: FontArc::new(font),
            bytes,
            face_index,
        });
    }

    None
}

fn load_font_family_source_from_db(
    db: &fontdb::Database,
    family: &str,
) -> Option<PrimaryFontSource> {
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    let face_id = db.query(&query)?;
    db.with_face_data(face_id, |font_data, face_index| {
        let bytes = font_data.to_vec();
        let Ok(font) = FontVec::try_from_vec_and_index(bytes.clone(), face_index) else {
            return None;
        };
        Some(PrimaryFontSource {
            font: FontArc::new(font),
            bytes,
            face_index,
        })
    })?
}

fn svg_font_family_list(families: &[String]) -> String {
    families
        .iter()
        .map(|value| format!("'{}'", value.replace('\'', "\\'")))
        .collect::<Vec<_>>()
        .join(", ")
}

fn xml_escape_attr(input: &str) -> String {
    input
        .chars()
        .flat_map(|ch| match ch {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect::<Vec<_>>(),
            '>' => "&gt;".chars().collect::<Vec<_>>(),
            '\"' => "&quot;".chars().collect::<Vec<_>>(),
            '\'' => "&apos;".chars().collect::<Vec<_>>(),
            _ => vec![ch],
        })
        .collect()
}

fn xml_escape_text(input: &str) -> String {
    input
        .chars()
        .flat_map(|ch| match ch {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect::<Vec<_>>(),
            '>' => "&gt;".chars().collect::<Vec<_>>(),
            '"' => "&quot;".chars().collect::<Vec<_>>(),
            '\'' => "&apos;".chars().collect::<Vec<_>>(),
            _ => vec![ch],
        })
        .collect()
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn draw_bitmap_glyph_rgba(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    cell_w: usize,
    cell_h: usize,
    glyph_char: char,
    fg_rgb: (u8, u8, u8),
    bitmap_cache: &mut BitmapGlyphCache,
) {
    let Some(mask) = bitmap_cache.mask_for(glyph_char) else {
        return;
    };
    for py in 0..cell_h {
        let y = y0 + py;
        if y >= height {
            continue;
        }
        let row_start = y.saturating_mul(width);
        let mask_row = py.saturating_mul(cell_w);
        for px in 0..cell_w {
            let x = x0 + px;
            if x >= width {
                continue;
            }
            if mask[mask_row + px] == 1 {
                let idx = (row_start + x).saturating_mul(4);
                pixels[idx] = fg_rgb.0;
                pixels[idx + 1] = fg_rgb.1;
                pixels[idx + 2] = fg_rgb.2;
                pixels[idx + 3] = 255;
            }
        }
    }
}

struct BitmapGlyphCache {
    cell_w: usize,
    cell_h: usize,
    masks: HashMap<char, Option<Vec<u8>>>,
}

impl BitmapGlyphCache {
    fn new(cell_w: usize, cell_h: usize) -> Self {
        Self {
            cell_w,
            cell_h,
            masks: HashMap::new(),
        }
    }

    fn mask_for(&mut self, glyph_char: char) -> Option<&[u8]> {
        let entry = self
            .masks
            .entry(glyph_char)
            .or_insert_with(|| build_bitmap_mask(glyph_char, self.cell_w, self.cell_h));
        entry.as_deref()
    }
}

fn build_bitmap_mask(glyph_char: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    if cell_w == 0 || cell_h == 0 {
        return None;
    }
    if let Some(mask) = block_element_mask(glyph_char, cell_w, cell_h) {
        return Some(mask);
    }
    let glyph = resolve_bitmap_glyph(glyph_char)?;
    let mut mask = vec![0_u8; cell_w.saturating_mul(cell_h)];
    let mut any_set = false;
    for py in 0..cell_h {
        let glyph_row = ((py.saturating_mul(8)) / cell_h).min(7);
        let bits = glyph[glyph_row];
        let row_start = py.saturating_mul(cell_w);
        for px in 0..cell_w {
            let glyph_col = ((px.saturating_mul(8)) / cell_w).min(7);
            if ((bits >> glyph_col) & 1) == 1 {
                mask[row_start + px] = 1;
                any_set = true;
            }
        }
    }
    any_set.then_some(mask)
}

fn resolve_bitmap_glyph(glyph_char: char) -> Option<[u8; 8]> {
    font8x8::BASIC_FONTS
        .get(glyph_char)
        .or_else(|| font8x8::LATIN_FONTS.get(glyph_char))
        .or_else(|| font8x8::BOX_FONTS.get(glyph_char))
        .or_else(|| font8x8::BLOCK_FONTS.get(glyph_char))
        .or_else(|| font8x8::GREEK_FONTS.get(glyph_char))
        .or_else(|| font8x8::MISC_FONTS.get(glyph_char))
        .or_else(|| font8x8::BASIC_FONTS.get('?'))
}

fn block_element_mask(glyph_char: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    let mut mask = vec![0_u8; cell_w.saturating_mul(cell_h)];
    match glyph_char {
        '█' => mask.fill(1),
        '▀' => {
            let cutoff = cell_h.div_ceil(2);
            for y in 0..cutoff {
                let row = y.saturating_mul(cell_w);
                for x in 0..cell_w {
                    mask[row + x] = 1;
                }
            }
        }
        '▄' => {
            let start = cell_h / 2;
            for y in start..cell_h {
                let row = y.saturating_mul(cell_w);
                for x in 0..cell_w {
                    mask[row + x] = 1;
                }
            }
        }
        '▌' => {
            let cutoff = cell_w.div_ceil(2);
            for y in 0..cell_h {
                let row = y.saturating_mul(cell_w);
                for x in 0..cutoff {
                    mask[row + x] = 1;
                }
            }
        }
        '▐' => {
            let start = cell_w / 2;
            for y in 0..cell_h {
                let row = y.saturating_mul(cell_w);
                for x in start..cell_w {
                    mask[row + x] = 1;
                }
            }
        }
        '░' => fill_shade_mask(&mut mask, cell_w, 1),
        '▒' => fill_shade_mask(&mut mask, cell_w, 2),
        '▓' => fill_shade_mask(&mut mask, cell_w, 3),
        _ => return None,
    }
    Some(mask)
}

fn fill_shade_mask(mask: &mut [u8], cell_w: usize, threshold: usize) {
    let threshold = threshold.min(4);
    for (idx, value) in mask.iter_mut().enumerate() {
        let y = idx / cell_w;
        let x = idx % cell_w;
        let matrix_value = (x & 1) + ((y & 1) << 1);
        if matrix_value < threshold {
            *value = 1;
        }
    }
}

struct RenderOptions {
    mode: RecordingRenderMode,
    font_families: Vec<String>,
    font_paths: Vec<String>,
    font_size_px: Option<f32>,
    line_height_mult: f32,
    background_opacity: f32,
    backdrop_rgb: (u8, u8, u8),
}

fn build_render_options(
    terminal_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    renderer: RecordingRenderMode,
    font_family: Option<&str>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    font_path: &[String],
) -> Result<RenderOptions> {
    if font_size.is_some_and(|value| value <= 0.0) {
        anyhow::bail!("--font-size must be greater than zero")
    }
    if line_height.is_some_and(|value| value <= 0.0) {
        anyhow::bail!("--line-height must be greater than zero")
    }
    let font_families = font_family
        .map(parse_csv_values)
        .or_else(|| terminal_profile.map(|profile| profile.font_families.clone()))
        .unwrap_or_default();
    let font_paths = if font_path.is_empty() {
        Vec::new()
    } else {
        font_path.to_vec()
    };
    Ok(RenderOptions {
        mode: renderer,
        font_families,
        font_paths,
        font_size_px: font_size
            .or_else(|| terminal_profile.and_then(|profile| profile.font_size_px.map(f32::from))),
        line_height_mult: line_height.unwrap_or(1.0),
        background_opacity: terminal_profile
            .and_then(|profile| profile.background_opacity_permille)
            .map_or(1.0, |permille| {
                (f32::from(permille) / 1000.0).clamp(0.0, 1.0)
            }),
        backdrop_rgb: (0, 0, 0),
    })
}

fn parse_csv_values(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
}

struct GlyphRenderer {
    fonts: Vec<FontArc>,
    scale: PxScale,
    baseline_offset: f32,
    glyph_font_index: HashMap<char, Option<usize>>,
}

impl GlyphRenderer {
    fn new(cell_w: u16, cell_h: u16, options: &RenderOptions) -> Option<Self> {
        let fonts = load_monospace_fonts(options);
        let font = fonts.first()?;
        let base_font_size = options
            .font_size_px
            .unwrap_or_else(|| f32::from(cell_h).max(8.0));
        let base_scale = PxScale {
            x: base_font_size,
            y: base_font_size,
        };
        let scaled_base = font.as_scaled(base_scale);
        let measured_advance = scaled_base.h_advance(font.glyph_id('M')).max(0.01);
        let target_advance = (f32::from(cell_w) * 0.92).max(1.0);
        let x_scale = base_scale.x * (target_advance / measured_advance);
        let scale = PxScale {
            x: x_scale,
            y: base_scale.y,
        };
        let scaled = font.as_scaled(scale);
        let text_height = (scaled.ascent() - scaled.descent()).max(1.0);
        let line_height = (text_height * options.line_height_mult.max(1.0)).max(text_height);
        let baseline_offset = ((f32::from(cell_h) - line_height) / 2.0).max(0.0) + scaled.ascent();
        Some(Self {
            fonts,
            scale,
            baseline_offset,
            glyph_font_index: HashMap::new(),
        })
    }

    fn resolve_font_index(&mut self, glyph_char: char) -> Option<usize> {
        if let Some(cached) = self.glyph_font_index.get(&glyph_char) {
            return *cached;
        }
        let resolved = self
            .fonts
            .iter()
            .enumerate()
            .find_map(|(index, font)| (font.glyph_id(glyph_char).0 != 0).then_some(index));
        self.glyph_font_index.insert(glyph_char, resolved);
        resolved
    }

    #[allow(clippy::too_many_arguments, clippy::cast_precision_loss)]
    fn draw_cell(
        &mut self,
        rgba: &mut [u8],
        width: usize,
        height: usize,
        x0: usize,
        y0: usize,
        glyph_char: char,
        fg_rgb: (u8, u8, u8),
        bg_rgb: (u8, u8, u8),
    ) -> bool {
        if glyph_char == ' ' {
            return false;
        }
        let Some(font_index) = self.resolve_font_index(glyph_char) else {
            return false;
        };
        let font = &self.fonts[font_index];
        let glyph = font.glyph_id(glyph_char).with_scale_and_position(
            self.scale,
            point(x0 as f32, y0 as f32 + self.baseline_offset),
        );
        let Some(outlined) = font.outline_glyph(glyph) else {
            return false;
        };
        outlined.draw(|gx, gy, coverage| {
            if coverage <= 0.0 {
                return;
            }
            let x = x0.saturating_add(gx as usize);
            let y = y0.saturating_add(gy as usize);
            if x >= width || y >= height {
                return;
            }
            let alpha = coverage;
            if alpha <= 0.0 {
                return;
            }
            let idx = (y.saturating_mul(width) + x).saturating_mul(4);
            rgba[idx] = blend_channel(fg_rgb.0, bg_rgb.0, alpha);
            rgba[idx + 1] = blend_channel(fg_rgb.1, bg_rgb.1, alpha);
            rgba[idx + 2] = blend_channel(fg_rgb.2, bg_rgb.2, alpha);
            rgba[idx + 3] = 255;
        });
        true
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn blend_channel(fg: u8, bg: u8, alpha: f32) -> u8 {
    f32::from(fg)
        .mul_add(alpha, f32::from(bg) * (1.0 - alpha))
        .round() as u8
}

fn load_monospace_fonts(options: &RenderOptions) -> Vec<FontArc> {
    let preset = font_preset_for_options(options);
    let mut fonts = Vec::<FontArc>::new();

    for path in &options.font_paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if let Ok(font) = FontVec::try_from_vec_and_index(bytes, 0) {
            fonts.push(FontArc::new(font));
        }
    }

    fonts.extend(bmux_fonts::load_preset_fonts_for_ab_glyph(preset));

    let mut db = fontdb::Database::new();
    let _ = bmux_fonts::register_preset_fonts(&mut db, preset);
    db.load_system_fonts();
    for path in &options.font_paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if meta.is_dir() {
            db.load_fonts_dir(path);
        } else if meta.is_file() {
            let _ = db.load_font_file(path);
        }
    }

    let mut families = Vec::<String>::new();
    if !options.font_families.is_empty() {
        families.extend(options.font_families.iter().cloned());
    }
    families.extend(bmux_fonts::default_families_for_preset(preset));
    let mut seen = HashSet::<String>::new();
    for family in families {
        let normalized = family.trim().to_ascii_lowercase();
        if normalized.is_empty() || !seen.insert(normalized) {
            continue;
        }
        if let Some(font) = load_font_family_from_db(&db, &family) {
            fonts.push(font);
        }
    }

    fonts
}

fn load_font_family_from_db(db: &fontdb::Database, family: &str) -> Option<FontArc> {
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    let face_id = db.query(&query)?;
    db.with_face_data(face_id, |font_data, face_index| {
        let Ok(font) = FontVec::try_from_vec_and_index(font_data.to_vec(), face_index) else {
            return None;
        };
        Some(FontArc::new(font))
    })?
}

const fn font_preset_for_options(_options: &RenderOptions) -> FontPreset {
    FontPreset::GhosttyNerd
}

#[derive(Debug, Clone)]
struct ExportPalette {
    colors: [(u8, u8, u8); 256],
    default_fg: (u8, u8, u8),
    default_bg: (u8, u8, u8),
}

type PaletteRgb = (u8, u8, u8);
type PaletteOverride = (u8, PaletteRgb);

impl ExportPalette {
    fn xterm() -> Self {
        let colors = xterm_256_palette();
        Self {
            colors,
            default_fg: colors[15],
            default_bg: colors[0],
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ResolvedPaletteSource {
    Recording,
    Terminal,
    Xterm,
}

fn resolve_export_palette(
    source: RecordingPaletteSource,
    recording_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    host_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    palette_foreground: Option<&str>,
    palette_background: Option<&str>,
    palette_color: &[String],
) -> Result<ExportPalette> {
    let mut palette = ExportPalette::xterm();
    let resolved_source = match source {
        RecordingPaletteSource::Auto => {
            if recording_profile.is_some_and(profile_has_palette_data) {
                ResolvedPaletteSource::Recording
            } else if host_profile.is_some_and(profile_has_palette_data) {
                ResolvedPaletteSource::Terminal
            } else {
                ResolvedPaletteSource::Xterm
            }
        }
        RecordingPaletteSource::Recording => {
            if recording_profile.is_some_and(profile_has_palette_data) {
                ResolvedPaletteSource::Recording
            } else {
                ResolvedPaletteSource::Xterm
            }
        }
        RecordingPaletteSource::Terminal => {
            if host_profile.is_some_and(profile_has_palette_data) {
                ResolvedPaletteSource::Terminal
            } else {
                ResolvedPaletteSource::Xterm
            }
        }
        RecordingPaletteSource::Xterm => ResolvedPaletteSource::Xterm,
    };

    match resolved_source {
        ResolvedPaletteSource::Recording => {
            if let Some(profile) = recording_profile {
                apply_profile_palette(&mut palette, profile);
            }
        }
        ResolvedPaletteSource::Terminal => {
            if let Some(profile) = host_profile {
                apply_profile_palette(&mut palette, profile);
            }
        }
        ResolvedPaletteSource::Xterm => {}
    }

    if let Some(fg) = parse_palette_default_override(palette_foreground, "palette foreground")? {
        palette.default_fg = fg;
    }
    if let Some(bg) = parse_palette_default_override(palette_background, "palette background")? {
        palette.default_bg = bg;
    }
    let overrides = parse_palette_color_overrides(palette_color)?;
    for (index, rgb) in overrides {
        palette.colors[usize::from(index)] = rgb;
    }

    Ok(palette)
}

const fn profile_has_palette_data(profile: &terminal_profile::DetectedTerminalProfile) -> bool {
    profile.palette_defaults.foreground.is_some()
        || profile.palette_defaults.background.is_some()
        || !profile.palette_defaults.colors.is_empty()
}

fn apply_profile_palette(
    palette: &mut ExportPalette,
    profile: &terminal_profile::DetectedTerminalProfile,
) {
    if let Some(raw) = profile.palette_defaults.foreground.as_deref() {
        if let Some(rgb) = parse_rgb_color(raw) {
            palette.default_fg = rgb;
        } else {
            tracing::warn!(
                "recording export: ignoring invalid terminal profile foreground color '{raw}'"
            );
        }
    }
    if let Some(raw) = profile.palette_defaults.background.as_deref() {
        if let Some(rgb) = parse_rgb_color(raw) {
            palette.default_bg = rgb;
        } else {
            tracing::warn!(
                "recording export: ignoring invalid terminal profile background color '{raw}'"
            );
        }
    }
    for entry in &profile.palette_defaults.colors {
        if let Some(rgb) = parse_rgb_color(&entry.color) {
            palette.colors[usize::from(entry.index)] = rgb;
        } else {
            tracing::warn!(
                "recording export: ignoring invalid terminal profile palette entry {}='{}'",
                entry.index,
                entry.color
            );
        }
    }
}

fn parse_palette_default_override(
    value: Option<&str>,
    field_name: &str,
) -> Result<Option<(u8, u8, u8)>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let Some(rgb) = parse_rgb_color(trimmed) else {
        anyhow::bail!("invalid {field_name} '{raw}'; expected auto or a color value")
    };
    Ok(Some(rgb))
}

fn parse_palette_color_overrides(values: &[String]) -> Result<Vec<PaletteOverride>> {
    values
        .iter()
        .map(|value| parse_palette_color_override(value))
        .collect()
}

fn parse_palette_color_override(value: &str) -> Result<PaletteOverride> {
    let (index_raw, color_raw) = value.split_once('=').ok_or_else(|| {
        anyhow::anyhow!("invalid palette override '{value}'; expected INDEX=COLOR")
    })?;
    let index = parse_palette_index(index_raw.trim())
        .ok_or_else(|| anyhow::anyhow!("invalid palette index '{index_raw}'; expected 0..255"))?;
    let color = color_raw.trim();
    let rgb = parse_rgb_color(color).ok_or_else(|| {
        anyhow::anyhow!("invalid palette color '{color_raw}'; expected a color value")
    })?;
    Ok((index, rgb))
}

fn parse_palette_index(value: &str) -> Option<u8> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            if digits.is_empty() {
                return None;
            }
            let parsed = u16::from_str_radix(digits, radix).ok()?;
            return u8::try_from(parsed).ok();
        }
    }
    let parsed = trimmed.parse::<u16>().ok()?;
    u8::try_from(parsed).ok()
}

fn resolve_vt100_color(
    color: vt100::Color,
    foreground: bool,
    palette: &ExportPalette,
) -> (u8, u8, u8) {
    match color {
        vt100::Color::Default => {
            if foreground {
                palette.default_fg
            } else {
                palette.default_bg
            }
        }
        vt100::Color::Idx(idx) => palette.colors[usize::from(idx)],
        vt100::Color::Rgb(r, g, b) => (r, g, b),
    }
}

fn parse_rgb_color(value: &str) -> Option<(u8, u8, u8)> {
    parse_hex_rgb(value).or_else(|| parse_osc_rgb(value))
}

fn parse_hex_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let trimmed = value.trim();
    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn parse_osc_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let trimmed = value.trim();
    let body = trimmed
        .strip_prefix("rgb:")
        .or_else(|| trimmed.strip_prefix("RGB:"))?;
    let mut channels = body.split('/');
    let r = channels.next().and_then(hex_component_to_u8)?;
    let g = channels.next().and_then(hex_component_to_u8)?;
    let b = channels.next().and_then(hex_component_to_u8)?;
    if channels.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

fn hex_component_to_u8(value: &str) -> Option<u8> {
    if !(1..=4).contains(&value.len()) || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let parsed = u16::from_str_radix(value, 16).ok()?;
    let bits = u32::try_from(value.len()).ok()?.saturating_mul(4);
    let max = (1_u32 << bits).saturating_sub(1);
    if max == 0 {
        return None;
    }
    let scaled = (u32::from(parsed)
        .saturating_mul(255)
        .saturating_add(max / 2))
        / max;
    u8::try_from(scaled).ok()
}

fn xterm_256_palette() -> [(u8, u8, u8); 256] {
    let mut colors = [(0_u8, 0_u8, 0_u8); 256];
    let base = [
        (0x00, 0x00, 0x00),
        (0x80, 0x00, 0x00),
        (0x00, 0x80, 0x00),
        (0x80, 0x80, 0x00),
        (0x00, 0x00, 0x80),
        (0x80, 0x00, 0x80),
        (0x00, 0x80, 0x80),
        (0xc0, 0xc0, 0xc0),
        (0x80, 0x80, 0x80),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x00, 0x00, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    colors[..16].copy_from_slice(&base);

    let steps = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    let mut index = 16_usize;
    for r in steps {
        for g in steps {
            for b in steps {
                colors[index] = (r, g, b);
                index = index.saturating_add(1);
            }
        }
    }

    for i in 0..24_u8 {
        let value = 8 + i * 10;
        colors[index] = (value, value, value);
        index = index.saturating_add(1);
    }
    colors
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
        let result = bmux_ipc::read_frames(&bytes).map_err(|e| {
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

    recordings.sort_by(|a, b| b.started_epoch_ms.cmp(&a.started_epoch_ms));
    Ok(recordings)
}

pub(super) const fn offline_recording_status() -> RecordingStatus {
    RecordingStatus {
        active: None,
        queue_len: 0,
    }
}

// Display track types are defined in bmux_ipc for cross-module sharing.
use bmux_ipc::{DisplayTrackEnvelope, DisplayTrackEvent};

// ── Image overlay for GIF export ─────────────────────────────────────────────

/// Decode an `AttachPaneImage` to an RGBA pixel buffer suitable for overlay.
///
/// Returns `(width, height, rgba_pixels)` or `None` if decoding fails or the
/// protocol is not supported at compile time.
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
fn decode_attach_image_to_rgba(image: &bmux_ipc::AttachPaneImage) -> Option<(u32, u32, Vec<u8>)> {
    // Decompress raw_data if it was compressed during IPC transport.
    let raw_data =
        bmux_ipc::compression::decompress_by_id(&image.raw_data, image.compression).ok()?;

    match image.protocol {
        #[cfg(feature = "image-sixel")]
        bmux_ipc::AttachImageProtocol::Sixel => {
            let pb = bmux_image::codec::sixel::decode(&raw_data)?;
            debug_assert!(
                matches!(pb.format, bmux_image::PixelFormat::Rgba8),
                "sixel decode should produce RGBA"
            );
            Some((pb.width, pb.height, pb.data))
        }

        #[cfg(feature = "image-kitty")]
        bmux_ipc::AttachImageProtocol::KittyGraphics => {
            let w = image.pixel_width;
            let h = image.pixel_height;
            if w == 0 || h == 0 {
                return None;
            }
            let expected_rgba = (w as usize) * (h as usize) * 4;
            let expected_rgb = (w as usize) * (h as usize) * 3;
            if raw_data.len() == expected_rgba {
                // Raw RGBA pixels
                Some((w, h, raw_data))
            } else if raw_data.len() == expected_rgb {
                // Raw RGB → expand to RGBA
                let mut rgba = Vec::with_capacity(expected_rgba);
                for chunk in raw_data.chunks_exact(3) {
                    rgba.extend_from_slice(chunk);
                    rgba.push(255);
                }
                Some((w, h, rgba))
            } else {
                // Likely PNG-compressed — decode via image crate
                decode_image_bytes_to_rgba(&raw_data)
            }
        }

        #[cfg(feature = "image-iterm2")]
        bmux_ipc::AttachImageProtocol::ITerm2 => {
            // iTerm2 raw_data is the OSC body (params + base64-encoded file).
            // Parse the body to extract decoded image file bytes, then decode
            // the image format (PNG, JPEG, GIF, etc.) to RGBA pixels.
            let (_params, file_bytes) = bmux_image::codec::iterm2::parse_body(&raw_data)?;
            decode_image_bytes_to_rgba(&file_bytes)
        }

        // When a protocol feature is disabled, we can't decode.
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// Decode image bytes (PNG, JPEG, GIF, BMP, etc.) to RGBA pixels using the
/// `image` crate.  This is the primary decoder for kitty PNG payloads and
/// all iTerm2 inline image formats.
#[cfg(any(feature = "image-kitty", feature = "image-iterm2"))]
fn decode_image_bytes_to_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let img = ::image::load_from_memory(data).ok()?;
    let rgba = img.to_rgba8();
    let w = rgba.width();
    let h = rgba.height();
    Some((w, h, rgba.into_raw()))
}

/// Composite decoded images onto an RGBA pixel frame buffer.
///
/// Each image's position is in cell coordinates; we convert to pixels using
/// the provided cell dimensions.
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
fn overlay_display_track_images(
    frame: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    cell_w: u32,
    cell_h: u32,
    images: &[bmux_ipc::AttachPaneImage],
) {
    for image in images {
        let Some((img_w, img_h, rgba)) = decode_attach_image_to_rgba(image) else {
            continue;
        };
        let x_px = u32::from(image.position_col) * cell_w;
        let y_px = u32::from(image.position_row) * cell_h;
        blit_rgba(
            frame,
            frame_width,
            frame_height,
            &rgba,
            img_w,
            img_h,
            x_px,
            y_px,
        );
    }
}

/// Alpha-blend `src` RGBA pixels onto `dst` RGBA frame at pixel offset (x, y).
///
/// Pixels outside the frame bounds are silently clipped.
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
#[allow(clippy::cast_possible_truncation, clippy::too_many_arguments)]
fn blit_rgba(
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    src: &[u8],
    src_w: u32,
    src_h: u32,
    x_off: u32,
    y_off: u32,
) {
    let dst_stride = (dst_w as usize) * 4;
    let src_stride = (src_w as usize) * 4;
    for sy in 0..src_h {
        let dy = y_off + sy;
        if dy >= dst_h {
            break;
        }
        let dst_row_start = (dy as usize) * dst_stride;
        let src_row_start = (sy as usize) * src_stride;
        for sx in 0..src_w {
            let dx = x_off + sx;
            if dx >= dst_w {
                break;
            }
            let si = src_row_start + (sx as usize) * 4;
            let di = dst_row_start + (dx as usize) * 4;
            if si + 3 >= src.len() || di + 3 >= dst.len() {
                continue;
            }
            let sa = src[si + 3];
            if sa == 0 {
                continue; // Fully transparent — skip.
            }
            if sa == 255 {
                // Fully opaque — overwrite.
                dst[di] = src[si];
                dst[di + 1] = src[si + 1];
                dst[di + 2] = src[si + 2];
                dst[di + 3] = 255;
            } else {
                // Alpha blend: out = src * sa + dst * (1 - sa)
                let inv_a = 255 - u16::from(sa);
                dst[di] =
                    ((u16::from(src[si]) * u16::from(sa) + u16::from(dst[di]) * inv_a) / 255) as u8;
                dst[di + 1] = ((u16::from(src[si + 1]) * u16::from(sa)
                    + u16::from(dst[di + 1]) * inv_a)
                    / 255) as u8;
                dst[di + 2] = ((u16::from(src[si + 2]) * u16::from(sa)
                    + u16::from(dst[di + 2]) * inv_a)
                    / 255) as u8;
                dst[di + 3] = (u16::from(sa) + u16::from(dst[di + 3]) * inv_a / 255).min(255) as u8;
            }
        }
    }
}

pub(super) struct DisplayCaptureWriter {
    started_at: Instant,
    writer: BufWriter<std::fs::File>,
    cursor_replay_state: CursorReplayState,
    /// Whether the last recorded `ImageUpdate` had any images.
    /// Used to avoid writing redundant empty `ImageUpdate` events on every
    /// frame for sessions that never use images.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    last_image_count: usize,
}

impl DisplayCaptureWriter {
    /// Create a new display capture writer that records terminal frames into
    /// the given recording directory.  Returns the writer directly (not wrapped
    /// in `Option`) — callers decide whether to create one.
    pub(super) fn open(recording_id: Uuid, recording_path: &Path, client_id: Uuid) -> Result<Self> {
        std::fs::create_dir_all(recording_path).with_context(|| {
            format!(
                "failed creating recording path {}",
                recording_path.display()
            )
        })?;
        let display_track_path = display_track_path(recording_path, client_id);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&display_track_path)
            .with_context(|| {
                format!(
                    "failed opening display track {}",
                    display_track_path.display()
                )
            })?;
        let mut capture = Self {
            started_at: Instant::now(),
            writer: BufWriter::new(file),
            cursor_replay_state: CursorReplayState::default(),
            #[cfg(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            ))]
            last_image_count: 0,
        };
        let (cell_width_px, cell_height_px, window_width_px, window_height_px) =
            capture_stream_open_metrics();
        let terminal_profile = terminal_profile::detect_render_profile();
        let terminal_profile_bytes = terminal_profile
            .as_ref()
            .and_then(|p| bmux_ipc::encode(p).ok());
        capture.record(DisplayTrackEvent::StreamOpened {
            client_id,
            recording_id,
            cell_width_px,
            cell_height_px,
            window_width_px,
            window_height_px,
            terminal_profile: terminal_profile_bytes,
        })?;
        if let Ok((cols, rows)) = terminal::size()
            && cols > 0
            && rows > 0
        {
            capture.record(DisplayTrackEvent::Resize { cols, rows })?;
        }
        Ok(capture)
    }

    pub(super) fn record_resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.record(DisplayTrackEvent::Resize { cols, rows })
    }

    pub(super) fn record_frame_bytes(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        update_cursor_replay_state(&mut self.cursor_replay_state, data);
        self.record(DisplayTrackEvent::FrameBytes {
            data: data.to_vec(),
        })
    }

    pub(super) fn record_activity(&mut self, kind: bmux_ipc::DisplayActivityKind) -> Result<()> {
        self.record(DisplayTrackEvent::Activity { kind })
    }

    pub(super) fn record_cursor_snapshot(
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

    pub(super) fn record_stream_closed(&mut self) -> Result<()> {
        self.record(DisplayTrackEvent::StreamClosed)
    }

    /// Record a snapshot of all visible pane images at the current frame time.
    /// The GIF exporter uses these to decode and overlay images onto the
    /// rasterized text cell grid.
    ///
    /// Skips the write when both the previous and current frames have no
    /// images (avoids ~15 bytes/frame overhead for sessions without images).
    /// An empty list IS recorded when transitioning from non-empty to empty,
    /// which signals the GIF exporter to clear stale overlays.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub(super) fn record_images(&mut self, images: &[bmux_ipc::AttachPaneImage]) -> Result<()> {
        let count = images.len();
        if count == 0 && self.last_image_count == 0 {
            return Ok(());
        }
        self.last_image_count = count;
        self.record(DisplayTrackEvent::ImageUpdate {
            images: images.to_vec(),
        })
    }

    pub(super) fn flush(&mut self) -> Result<()> {
        self.writer
            .flush()
            .context("failed flushing display capture writer")
    }

    #[allow(clippy::cast_possible_truncation)] // Epoch millis won't exceed u64
    fn record(&mut self, event: DisplayTrackEvent) -> Result<()> {
        let envelope = DisplayTrackEnvelope {
            mono_ns: self
                .started_at
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
            event,
        };
        bmux_ipc::write_frame(&mut self.writer, &envelope)
            .map_err(|e| anyhow::anyhow!("display track write_frame failed: {e}"))?;
        Ok(())
    }
}

fn display_track_path(recording_path: &Path, client_id: Uuid) -> PathBuf {
    recording_path.join(format!("display-{client_id}.bin"))
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    fn stream_opened(
        cell_width_px: Option<u16>,
        cell_height_px: Option<u16>,
        window_width_px: Option<u16>,
        window_height_px: Option<u16>,
    ) -> DisplayTrackEnvelope {
        DisplayTrackEnvelope {
            mono_ns: 1,
            event: DisplayTrackEvent::StreamOpened {
                client_id: Uuid::nil(),
                recording_id: Uuid::nil(),
                cell_width_px,
                cell_height_px,
                window_width_px,
                window_height_px,
                terminal_profile: None,
            },
        }
    }

    fn frame_bytes(mono_ns: u64) -> DisplayTrackEnvelope {
        DisplayTrackEnvelope {
            mono_ns,
            event: DisplayTrackEvent::FrameBytes { data: vec![b'x'] },
        }
    }

    fn cursor_snapshot(mono_ns: u64, x: u16, y: u16) -> DisplayTrackEnvelope {
        DisplayTrackEnvelope {
            mono_ns,
            event: DisplayTrackEvent::CursorSnapshot {
                x,
                y,
                visible: true,
                shape: bmux_ipc::DisplayCursorShape::Block,
                blink_enabled: true,
            },
        }
    }

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
        bmux_ipc::write_frame(&mut buf, &envelope).expect("write should succeed");
        let result =
            bmux_ipc::read_frames::<DisplayTrackEnvelope>(&buf).expect("read should succeed");
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
    fn resolve_export_cell_metrics_prefers_cli_then_recording() {
        let events = vec![stream_opened(Some(7), Some(14), Some(700), Some(350))];
        let resolved = resolve_export_cell_metrics(&events, Some((9, 18)), Some(10), None)
            .expect("metrics should resolve");
        assert_eq!(resolved.width, 10);
        assert_eq!(resolved.height, 18);
    }

    #[test]
    fn resolve_export_cell_metrics_can_infer_from_recorded_window_and_resize() {
        let events = vec![
            stream_opened(None, None, Some(1200), Some(600)),
            DisplayTrackEnvelope {
                mono_ns: 2,
                event: DisplayTrackEvent::Resize {
                    cols: 120,
                    rows: 30,
                },
            },
        ];
        let resolved =
            resolve_export_cell_metrics(&events, None, None, None).expect("metrics should resolve");
        assert_eq!(resolved.width, 10);
        assert_eq!(resolved.height, 20);
    }

    #[test]
    fn infer_export_terminal_bounds_prefers_resize_events() {
        let events = vec![
            stream_opened(Some(16), Some(35), Some(3440), Some(2150)),
            DisplayTrackEnvelope {
                mono_ns: 2,
                event: DisplayTrackEvent::Resize {
                    cols: 120,
                    rows: 40,
                },
            },
            cursor_snapshot(3, 213, 57),
        ];
        assert_eq!(infer_export_terminal_bounds(&events).unwrap(), (120, 40));
    }

    #[test]
    fn infer_export_terminal_bounds_falls_back_to_stream_metrics_without_resize() {
        let events = vec![
            stream_opened(Some(16), Some(35), Some(3440), Some(2150)),
            frame_bytes(2),
        ];
        assert_eq!(infer_export_terminal_bounds(&events).unwrap(), (215, 61));
    }

    #[test]
    fn infer_export_terminal_bounds_expands_stream_bounds_with_cursor_extent() {
        let events = vec![
            stream_opened(Some(10), Some(25), Some(800), Some(600)),
            cursor_snapshot(2, 100, 30),
        ];
        assert_eq!(infer_export_terminal_bounds(&events).unwrap(), (101, 31));
    }

    #[test]
    fn infer_export_terminal_bounds_errors_when_resize_and_stream_grid_missing() {
        let events = vec![
            stream_opened(None, None, Some(3440), Some(2150)),
            cursor_snapshot(2, 189, 28),
        ];
        let err = infer_export_terminal_bounds(&events).expect_err("missing bounds should fail");
        assert!(
            err.to_string().contains("cannot infer terminal bounds"),
            "error should explain why export cannot proceed"
        );
    }

    #[test]
    #[allow(clippy::float_cmp)] // Test assertions with exact expected values
    fn build_render_options_uses_terminal_profile_defaults() {
        let profile = terminal_profile::DetectedTerminalProfile {
            terminal_id: "ghostty".to_string(),
            font_families: vec!["JetBrains Mono".to_string()],
            font_size_px: Some(15),
            background_opacity_permille: Some(900),
            cursor_defaults: terminal_profile::CursorDefaults::default(),
            palette_defaults: terminal_profile::PaletteDefaults::default(),
            source: "test".to_string(),
        };
        let options = build_render_options(
            Some(&profile),
            RecordingRenderMode::Bitmap,
            None,
            None,
            None,
            &[],
        )
        .expect("options should resolve");
        assert_eq!(options.font_families, vec!["JetBrains Mono".to_string()]);
        assert_eq!(options.font_size_px, Some(15.0));
        assert_eq!(options.line_height_mult, 1.0);
        assert_eq!(options.background_opacity, 0.9);
        assert_eq!(options.backdrop_rgb, (0, 0, 0));
    }

    #[test]
    fn recording_terminal_profile_reads_stream_opened_profile() {
        let profile = terminal_profile::DetectedTerminalProfile {
            terminal_id: "ghostty".to_string(),
            font_families: vec!["Iosevka".to_string()],
            font_size_px: Some(14),
            background_opacity_permille: None,
            cursor_defaults: terminal_profile::CursorDefaults::default(),
            palette_defaults: terminal_profile::PaletteDefaults::default(),
            source: "ghostty-config:/tmp/config".to_string(),
        };
        let events = vec![DisplayTrackEnvelope {
            mono_ns: 1,
            event: DisplayTrackEvent::StreamOpened {
                client_id: Uuid::nil(),
                recording_id: Uuid::nil(),
                cell_width_px: Some(8),
                cell_height_px: Some(16),
                window_width_px: Some(800),
                window_height_px: Some(600),
                terminal_profile: Some(bmux_ipc::encode(&profile).unwrap()),
            },
        }];
        let resolved = recording_terminal_profile(&events).expect("profile should be resolved");
        assert_eq!(resolved, profile);
    }

    #[test]
    fn bitmap_glyph_cache_reuses_computed_mask() {
        let mut cache = BitmapGlyphCache::new(8, 16);
        let first = cache.mask_for('A').expect("mask should exist").to_vec();
        let second = cache.mask_for('A').expect("mask should exist").to_vec();
        assert_eq!(first, second);
        assert_eq!(cache.masks.len(), 1);
    }

    #[test]
    fn estimate_export_progress_counts_events_and_emitted_frames() {
        let events = vec![
            stream_opened(Some(8), Some(16), Some(800), Some(600)),
            frame_bytes(0),
            frame_bytes(50_000_000),
            frame_bytes(100_000_000),
            frame_bytes(200_000_000),
        ];
        let estimate = estimate_export_progress(&events, 1.0, 10, None, None);
        assert_eq!(estimate.total_frame_events, 4);
        assert_eq!(estimate.estimated_emitted_frames, 3);
    }

    #[test]
    fn estimate_export_progress_respects_max_frames_limit() {
        let events = vec![
            stream_opened(Some(8), Some(16), Some(800), Some(600)),
            frame_bytes(0),
            frame_bytes(50_000_000),
            frame_bytes(100_000_000),
            frame_bytes(200_000_000),
        ];
        let estimate = estimate_export_progress(&events, 1.0, 10, None, Some(2));
        assert_eq!(estimate.total_frame_events, 3);
        assert_eq!(estimate.estimated_emitted_frames, 2);
    }

    #[test]
    fn estimate_export_progress_uses_timeline_frames_for_sparse_events() {
        let events = vec![
            stream_opened(Some(8), Some(16), Some(800), Some(600)),
            frame_bytes(0),
            frame_bytes(450_000_000),
        ];
        let estimate = estimate_export_progress(&events, 1.0, 10, None, None);
        assert_eq!(estimate.total_frame_events, 2);
        assert_eq!(estimate.estimated_emitted_frames, 5);
    }

    #[test]
    fn compute_cursor_visibility_blinks_on_timeline_clock() {
        let options = CursorExportOptions {
            mode: RecordingCursorMode::Auto,
            shape: RecordingCursorShape::Auto,
            blink: RecordingCursorBlinkMode::On,
            profile: RecordingCursorProfile::Generic,
            blink_period_ns: 500_000_000,
            solid_after_input_ns: 0,
            solid_after_output_ns: 0,
            solid_after_cursor_ns: 0,
            paint_mode: RecordingCursorPaintMode::Invert,
            text_mode: RecordingCursorTextMode::SwapFgBg,
            bar_width_pct: 16,
            underline_height_pct: 12,
            color_label: "auto".to_string(),
            color_override: None,
        };
        let state = CursorReplayState::default();
        let mut blink_anchor_ns = None;
        let (on_a, blink_a, _) = compute_cursor_visibility(
            &options,
            state,
            true,
            true,
            0,
            None,
            None,
            None,
            &mut blink_anchor_ns,
        );
        let (on_b, blink_b, _) = compute_cursor_visibility(
            &options,
            state,
            true,
            true,
            510_000_000,
            None,
            None,
            None,
            &mut blink_anchor_ns,
        );
        let (on_c, blink_c, _) = compute_cursor_visibility(
            &options,
            state,
            true,
            true,
            1_020_000_000,
            None,
            None,
            None,
            &mut blink_anchor_ns,
        );
        assert!(on_a && blink_a);
        assert!(!on_b && !blink_b);
        assert!(on_c && blink_c);
    }

    #[test]
    fn compute_cursor_visibility_aligns_phase_to_first_visible_frame() {
        let options = CursorExportOptions {
            mode: RecordingCursorMode::Auto,
            shape: RecordingCursorShape::Auto,
            blink: RecordingCursorBlinkMode::On,
            profile: RecordingCursorProfile::Generic,
            blink_period_ns: 500_000_000,
            solid_after_input_ns: 0,
            solid_after_output_ns: 0,
            solid_after_cursor_ns: 0,
            paint_mode: RecordingCursorPaintMode::Invert,
            text_mode: RecordingCursorTextMode::SwapFgBg,
            bar_width_pct: 16,
            underline_height_pct: 12,
            color_label: "auto".to_string(),
            color_override: None,
        };
        let state = CursorReplayState::default();
        let mut blink_anchor_ns = None;
        let _ = compute_cursor_visibility(
            &options,
            state,
            false,
            true,
            700_000_000,
            None,
            None,
            None,
            &mut blink_anchor_ns,
        );
        let (on_a, blink_a, _) = compute_cursor_visibility(
            &options,
            state,
            true,
            true,
            700_000_000,
            None,
            None,
            None,
            &mut blink_anchor_ns,
        );
        let (on_b, blink_b, _) = compute_cursor_visibility(
            &options,
            state,
            true,
            true,
            1_210_000_000,
            None,
            None,
            None,
            &mut blink_anchor_ns,
        );
        assert!(on_a && blink_a);
        assert!(!on_b && !blink_b);
    }

    #[test]
    fn compute_cursor_visibility_stays_solid_while_recent_activity() {
        let options = CursorExportOptions {
            mode: RecordingCursorMode::Auto,
            shape: RecordingCursorShape::Auto,
            blink: RecordingCursorBlinkMode::On,
            profile: RecordingCursorProfile::Ghostty,
            blink_period_ns: 500_000_000,
            solid_after_input_ns: 500_000_000,
            solid_after_output_ns: 500_000_000,
            solid_after_cursor_ns: 500_000_000,
            paint_mode: RecordingCursorPaintMode::Invert,
            text_mode: RecordingCursorTextMode::SwapFgBg,
            bar_width_pct: 16,
            underline_height_pct: 12,
            color_label: "auto".to_string(),
            color_override: None,
        };
        let state = CursorReplayState::default();
        let mut blink_anchor_ns = None;
        let (on_a, blink_a, _) = compute_cursor_visibility(
            &options,
            state,
            true,
            true,
            300_000_000,
            Some(250_000_000),
            None,
            None,
            &mut blink_anchor_ns,
        );
        let (on_b, blink_b, _) = compute_cursor_visibility(
            &options,
            state,
            true,
            true,
            900_000_000,
            Some(250_000_000),
            None,
            None,
            &mut blink_anchor_ns,
        );
        assert!(on_a && blink_a);
        assert!(!on_b && !blink_b);
    }

    #[test]
    fn format_duration_compact_uses_mm_ss_and_hh_mm_ss() {
        assert_eq!(
            format_duration_compact(std::time::Duration::from_secs(65)),
            "01:05"
        );
        assert_eq!(
            format_duration_compact(std::time::Duration::from_secs(3_665)),
            "1:01:05"
        );
    }

    #[test]
    fn parse_palette_color_override_supports_prefixed_index_radix() {
        assert_eq!(
            parse_palette_color_override("0x0a=#010203").expect("hex index should parse"),
            (10, (1, 2, 3))
        );
        assert_eq!(
            parse_palette_color_override("0b1010=#010203").expect("binary index should parse"),
            (10, (1, 2, 3))
        );
        assert_eq!(
            parse_palette_color_override("0o12=#010203").expect("octal index should parse"),
            (10, (1, 2, 3))
        );
    }

    #[test]
    fn parse_rgb_color_accepts_osc_rgb_format() {
        assert_eq!(parse_rgb_color("rgb:ff/00/7f"), Some((255, 0, 127)));
        assert_eq!(parse_rgb_color("RGB:ffff/0000/7fff"), Some((255, 0, 127)));
    }

    #[test]
    fn resolve_vt100_color_preserves_truecolor_rgb() {
        let palette = ExportPalette::xterm();
        assert_eq!(
            resolve_vt100_color(vt100::Color::Rgb(1, 2, 3), true, &palette),
            (1, 2, 3)
        );
    }

    #[test]
    fn resolve_export_palette_auto_prefers_recording_profile_palette() {
        let recording_profile = terminal_profile::DetectedTerminalProfile {
            terminal_id: "ghostty".to_string(),
            font_families: Vec::new(),
            font_size_px: None,
            background_opacity_permille: None,
            cursor_defaults: terminal_profile::CursorDefaults::default(),
            palette_defaults: terminal_profile::PaletteDefaults {
                foreground: Some("#f0f0f0".to_string()),
                background: Some("#101010".to_string()),
                colors: vec![terminal_profile::PaletteColorEntry {
                    index: 5,
                    color: "#bb78d9".to_string(),
                }],
            },
            source: "recording".to_string(),
        };
        let host_profile = terminal_profile::DetectedTerminalProfile {
            terminal_id: "ghostty".to_string(),
            font_families: Vec::new(),
            font_size_px: None,
            background_opacity_permille: None,
            cursor_defaults: terminal_profile::CursorDefaults::default(),
            palette_defaults: terminal_profile::PaletteDefaults {
                foreground: Some("#ffffff".to_string()),
                background: Some("#000000".to_string()),
                colors: vec![terminal_profile::PaletteColorEntry {
                    index: 5,
                    color: "#00ff00".to_string(),
                }],
            },
            source: "host".to_string(),
        };

        let resolved = resolve_export_palette(
            RecordingPaletteSource::Auto,
            Some(&recording_profile),
            Some(&host_profile),
            None,
            None,
            &[],
        )
        .expect("palette should resolve");

        assert_eq!(resolved.default_fg, (0xf0, 0xf0, 0xf0));
        assert_eq!(resolved.default_bg, (0x10, 0x10, 0x10));
        assert_eq!(resolved.colors[5], (0xbb, 0x78, 0xd9));
    }

    #[test]
    fn parse_cursor_color_accepts_auto_and_hex() {
        assert_eq!(parse_cursor_color("auto").expect("auto should parse"), None);
        assert_eq!(
            parse_cursor_color("#11AAee").expect("hex should parse"),
            Some((0x11, 0xaa, 0xee))
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
            bmux_ipc::DisplayCursorShape::Block
        );
        assert_eq!(
            display_cursor_shape_from_visual(CursorVisualShape::Bar),
            bmux_ipc::DisplayCursorShape::Bar
        );
        assert_eq!(
            display_cursor_shape_from_visual(CursorVisualShape::Underline),
            bmux_ipc::DisplayCursorShape::Underline
        );
    }

    #[test]
    fn cursor_snapshot_from_parser_fallback_uses_parser_cursor_state() {
        let mut parser = vt100::Parser::new(24, 80, 1024);
        parser.process(b"\x1b[6;11H");
        let snapshot = cursor_snapshot_from_parser_fallback(
            &parser,
            CursorReplayState {
                shape: CursorVisualShape::Bar,
                blink_enabled: false,
            },
        );
        assert_eq!(snapshot.x, 10);
        assert_eq!(snapshot.y, 5);
        assert!(snapshot.visible);
        assert_eq!(snapshot.shape, bmux_ipc::DisplayCursorShape::Bar);
        assert!(!snapshot.blink_enabled);
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
            format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
            session_id: None,
            capture_input: true,
            profile: bmux_ipc::RecordingProfile::Functional,
            event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
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
        assert_eq!(kinds, vec![bmux_ipc::RecordingEventKind::PaneOutputRaw]);
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
                format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: bmux_ipc::RecordingProfile::Functional,
                event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
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
                format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: bmux_ipc::RecordingProfile::Functional,
                event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
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
                format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: bmux_ipc::RecordingProfile::Functional,
                event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
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
                format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: bmux_ipc::RecordingProfile::Functional,
                event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
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
            format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
            session_id: None,
            capture_input: true,
            profile: bmux_ipc::RecordingProfile::Functional,
            event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
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
                format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: bmux_ipc::RecordingProfile::Functional,
                event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
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
                format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: bmux_ipc::RecordingProfile::Functional,
                event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
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
