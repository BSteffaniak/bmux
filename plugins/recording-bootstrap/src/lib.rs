//! Recording policy applied before activation, including when no capture backend is installed.
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_recording_plugin_api::{
    ManualRecordingStartOptions, RecordingPluginConfig, RollingRecordingSettings,
};
use bmux_recording_protocol::RecordingRollingStartOptions;

/// Resolve capture intent separately from server startup and process ownership.
pub fn rolling_auto_start(
    configured_enabled: bool,
    rolling_enabled_override: Option<bool>,
    rolling_options: &RecordingRollingStartOptions,
    effective_rolling_settings: &RollingRecordingSettings,
) -> anyhow::Result<bool> {
    if let Some(window_secs) = rolling_options.window_secs
        && window_secs == 0
    {
        anyhow::bail!("--rolling-window-secs must be greater than 0")
    }
    let explicit_rolling_event_selection = rolling_options.event_kinds.is_some()
        || rolling_options.capture_input.is_some()
        || rolling_options.capture_output.is_some()
        || rolling_options.capture_events.is_some()
        || rolling_options.capture_protocol_replies.is_some()
        || rolling_options.capture_images.is_some();
    let rolling_requested = rolling_enabled_override == Some(true)
        || rolling_options.window_secs.is_some()
        || explicit_rolling_event_selection
        || (rolling_enabled_override.is_none() && configured_enabled);
    if rolling_requested
        && effective_rolling_settings.window_secs == 0
        && (rolling_enabled_override == Some(true)
            || rolling_options.window_secs.is_some()
            || explicit_rolling_event_selection)
    {
        anyhow::bail!(
            "rolling recording was explicitly enabled but window is 0s; set `recording.rolling_window_secs` in config or pass `--rolling-window-secs <secs>`"
        )
    }
    let effective_rolling_enabled = if rolling_enabled_override == Some(false) {
        false
    } else if rolling_options.window_secs.is_some() || explicit_rolling_event_selection {
        true
    } else {
        rolling_enabled_override.unwrap_or(configured_enabled)
    } && effective_rolling_settings.window_secs > 0;
    if effective_rolling_enabled && effective_rolling_settings.event_kinds.is_empty() {
        anyhow::bail!(
            "rolling recording is enabled but no rolling event kinds are selected; enable rolling capture flags or pass --rolling-event-kind/--rolling-event-kind-all"
        )
    }
    Ok(effective_rolling_enabled)
}

/// Build the same recording configuration for daemon and standalone command hosts.
#[must_use]
pub fn recording_plugin_config(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    rolling_auto_start: bool,
    rolling_settings: &RollingRecordingSettings,
    startup_recording: Option<ManualRecordingStartOptions>,
) -> RecordingPluginConfig {
    let auto_export_dir = config.recording_auto_export_dir(paths);
    RecordingPluginConfig {
        recordings_dir: config.recordings_dir(paths),
        rolling_recordings_dir: paths.rolling_recordings_dir(),
        rolling_segment_mb: config.recording.segment_mb,
        retention_days: config.recording.retention_days,
        rolling_defaults: rolling_settings.clone(),
        rolling_auto_start,
        auto_export: config.recording.auto_export || auto_export_dir.is_some(),
        auto_export_dir,
        auto_export_fps: config.recording.export.fps.max(1),
        startup_recording,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_recording_protocol::RecordingEventKind;

    #[test]
    fn explicit_capture_selects_rolling_but_explicit_off_wins() {
        let options = RecordingRollingStartOptions {
            capture_images: Some(false),
            ..Default::default()
        };
        let settings = RollingRecordingSettings {
            window_secs: 20,
            event_kinds: vec![RecordingEventKind::ServerEvent],
        };
        assert!(rolling_auto_start(false, None, &options, &settings).unwrap());
        assert!(!rolling_auto_start(true, Some(false), &options, &settings).unwrap());
    }

    #[test]
    fn implicit_zero_window_is_dormant_but_explicit_start_requires_capacity() {
        let settings = RollingRecordingSettings {
            window_secs: 0,
            event_kinds: vec![],
        };
        let options = RecordingRollingStartOptions::default();
        assert!(!rolling_auto_start(true, None, &options, &settings).unwrap());
        assert!(
            rolling_auto_start(false, Some(true), &options, &settings)
                .unwrap_err()
                .to_string()
                .contains("window is 0s")
        );
        let options = RecordingRollingStartOptions {
            window_secs: Some(0),
            ..options
        };
        assert_eq!(
            rolling_auto_start(true, Some(false), &options, &settings)
                .unwrap_err()
                .to_string(),
            "--rolling-window-secs must be greater than 0"
        );
    }
}
