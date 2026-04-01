use super::*;

pub(super) async fn run_doctor(as_json: bool) -> Result<u8> {
    let paths = ConfigPaths::default();

    let has_warnings = if as_json {
        let report = build_doctor_report(&paths).await;
        let w = report.has_warnings;
        println!(
            "{}",
            serde_json::to_string_pretty(&report.to_json())
                .context("failed to encode doctor report as json")?
        );
        w
    } else {
        run_doctor_text(&paths).await?
    };

    Ok(if has_warnings { 1 } else { 0 })
}

// ── text output ─────────────────────────────────────────────────────────

async fn run_doctor_text(paths: &ConfigPaths) -> Result<bool> {
    let mut warnings = false;

    // 1. Config file
    let config_path = paths.config_file();
    let config = if config_path.exists() {
        match BmuxConfig::load() {
            Ok(config) => {
                print_ok("config", &format!("{}", config_path.display()));
                Some(config)
            }
            Err(e) => {
                print_warn(
                    "config",
                    &format!("{} (parse error: {e})", config_path.display()),
                );
                warnings = true;
                None
            }
        }
    } else {
        print_info(
            "config",
            &format!("{} (does not exist)", config_path.display()),
        );
        Some(BmuxConfig::default())
    };

    // 2. Config paths
    let dir_checks = [
        ("config", paths.config_dir.clone()),
        ("data", paths.data_dir.clone()),
        ("runtime", paths.runtime_dir.clone()),
        ("state", paths.state_dir()),
        ("logs", paths.logs_dir()),
    ];
    let mut missing_dirs = Vec::new();
    for (label, dir) in &dir_checks {
        if !dir.exists() {
            missing_dirs.push(*label);
        }
    }
    if missing_dirs.is_empty() {
        print_ok("paths", "config, data, runtime, state, logs");
    } else {
        print_warn(
            "paths",
            &format!("missing directories: {}", missing_dirs.join(", ")),
        );
        warnings = true;
    }

    // 3. Server status
    match fetch_server_status().await {
        Ok(Some(status)) if status.running => {
            let meta = read_server_runtime_metadata();
            let version_str = meta
                .as_ref()
                .ok()
                .and_then(|m| m.as_ref())
                .map_or("unknown".to_string(), |m| m.version.clone());
            let pid_str = meta
                .as_ref()
                .ok()
                .and_then(|m| m.as_ref())
                .map_or("?".to_string(), |m| m.pid.to_string());

            print_ok(
                "server",
                &format!("running (pid {pid_str}, v{version_str})"),
            );

            // Check for stale build
            if let Ok(Some(meta)) = &meta {
                if let Ok(cli_build_id) = current_cli_build_id() {
                    if meta.build_id != cli_build_id {
                        print_warn(
                            "server",
                            "stale build detected (server build differs from CLI)",
                        );
                        warnings = true;
                    }
                }
            }
        }
        _ => {
            print_info("server", "not running");
        }
    }

    // 4. Terminfo
    match check_terminfo_available("bmux-256color") {
        Some(true) => print_ok("terminfo", "bmux-256color installed"),
        Some(false) => {
            print_warn(
                "terminfo",
                "bmux-256color not installed (run: bmux terminal install-terminfo)",
            );
            warnings = true;
        }
        None => {
            print_warn("terminfo", "could not check (infocmp not available)");
            warnings = true;
        }
    }

    // 5. Terminal profile
    if let Some(config) = &config {
        let configured_term = &config.behavior.pane_term;
        let resolution = resolve_pane_term(configured_term);
        let profile_name = terminal_profile_name(resolution.profile);

        if resolution.warnings.is_empty() {
            print_ok(
                "terminal",
                &format!("{} (profile: {profile_name})", resolution.pane_term),
            );
        } else {
            print_warn(
                "terminal",
                &format!(
                    "{} (profile: {profile_name}) -- {}",
                    resolution.pane_term,
                    resolution.warnings.join("; ")
                ),
            );
            warnings = true;
        }
    }

    // 6. Plugin health
    if let Some(config) = &config {
        match scan_available_plugins(config, paths) {
            Ok(registry) => {
                let enabled = effective_enabled_plugins(config, &registry);
                let host = plugin_host_metadata();
                let mut compat_issues = Vec::new();
                for plugin_id in &enabled {
                    if let Some(plugin) = registry.get(plugin_id) {
                        let report = PluginRegistry::compatibility_report(plugin, &host);
                        if !report.is_loadable() {
                            compat_issues.push(plugin_id.clone());
                        }
                    }
                }
                if compat_issues.is_empty() {
                    if enabled.is_empty() {
                        print_info("plugins", "none enabled");
                    } else {
                        print_ok(
                            "plugins",
                            &format!("{} enabled ({})", enabled.len(), enabled.join(", ")),
                        );
                    }
                } else {
                    print_warn(
                        "plugins",
                        &format!(
                            "{} enabled, {} incompatible ({})",
                            enabled.len(),
                            compat_issues.len(),
                            compat_issues.join(", ")
                        ),
                    );
                    warnings = true;
                }
            }
            Err(e) => {
                print_warn("plugins", &format!("failed to scan: {e:#}"));
                warnings = true;
            }
        }
    }

    Ok(warnings)
}

// ── JSON output ─────────────────────────────────────────────────────────

struct DoctorReport {
    has_warnings: bool,
    config: DoctorConfigCheck,
    paths: DoctorPathsCheck,
    server: DoctorServerCheck,
    terminfo: DoctorTerminfoCheck,
    terminal: DoctorTerminalCheck,
    plugins: DoctorPluginsCheck,
}

impl DoctorReport {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "config": self.config.to_json(),
            "paths": self.paths.to_json(),
            "server": self.server.to_json(),
            "terminfo": self.terminfo.to_json(),
            "terminal": self.terminal.to_json(),
            "plugins": self.plugins.to_json(),
        })
    }
}

struct DoctorConfigCheck {
    path: PathBuf,
    exists: bool,
    valid: bool,
    error: Option<String>,
}

impl DoctorConfigCheck {
    fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::json!({
            "path": self.path,
            "exists": self.exists,
            "valid": self.valid,
        });
        if let Some(e) = &self.error {
            obj["error"] = serde_json::Value::String(e.clone());
        }
        obj
    }
}

struct DoctorPathsCheck {
    dirs: Vec<(String, PathBuf, bool)>,
}

impl DoctorPathsCheck {
    fn to_json(&self) -> serde_json::Value {
        let map: serde_json::Map<String, serde_json::Value> = self
            .dirs
            .iter()
            .map(|(label, path, exists)| {
                (
                    label.clone(),
                    serde_json::json!({ "path": path, "exists": exists }),
                )
            })
            .collect();
        serde_json::Value::Object(map)
    }
}

struct DoctorServerCheck {
    running: bool,
    pid: Option<u32>,
    version: Option<String>,
    stale_build: bool,
}

impl DoctorServerCheck {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "running": self.running,
            "pid": self.pid,
            "version": self.version,
            "stale_build": self.stale_build,
        })
    }
}

struct DoctorTerminfoCheck {
    installed: Option<bool>,
}

impl DoctorTerminfoCheck {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "bmux_256color_installed": self.installed,
        })
    }
}

struct DoctorTerminalCheck {
    configured_term: String,
    effective_term: String,
    profile: String,
    warnings: Vec<String>,
}

impl DoctorTerminalCheck {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "configured_term": self.configured_term,
            "effective_term": self.effective_term,
            "profile": self.profile,
            "warnings": self.warnings,
        })
    }
}

struct DoctorPluginsCheck {
    enabled: Vec<String>,
    incompatible: Vec<String>,
    scan_error: Option<String>,
}

impl DoctorPluginsCheck {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled_count": self.enabled.len(),
            "enabled": self.enabled,
            "incompatible": self.incompatible,
            "scan_error": self.scan_error,
        })
    }
}

async fn build_doctor_report(paths: &ConfigPaths) -> DoctorReport {
    let mut has_warnings = false;

    // Config
    let config_path = paths.config_file();
    let config_exists = config_path.exists();
    let (config, config_valid, config_error) = if config_exists {
        match BmuxConfig::load() {
            Ok(c) => (Some(c), true, None),
            Err(e) => {
                has_warnings = true;
                (None, false, Some(e.to_string()))
            }
        }
    } else {
        (Some(BmuxConfig::default()), true, None)
    };

    // Paths
    let dir_entries = vec![
        ("config".to_string(), paths.config_dir.clone()),
        ("data".to_string(), paths.data_dir.clone()),
        ("runtime".to_string(), paths.runtime_dir.clone()),
        ("state".to_string(), paths.state_dir()),
        ("logs".to_string(), paths.logs_dir()),
    ];
    let dirs: Vec<(String, PathBuf, bool)> = dir_entries
        .into_iter()
        .map(|(label, path)| {
            let exists = path.exists();
            if !exists {
                // Note: we set has_warnings below through a mutable ref pattern
            }
            (label, path, exists)
        })
        .collect();
    if dirs.iter().any(|(_, _, exists)| !exists) {
        has_warnings = true;
    }

    // Server
    let (server_running, server_pid, server_version, stale_build) =
        match fetch_server_status().await {
            Ok(Some(status)) if status.running => {
                let meta = read_server_runtime_metadata();
                let pid = meta.as_ref().ok().and_then(|m| m.as_ref()).map(|m| m.pid);
                let version = meta
                    .as_ref()
                    .ok()
                    .and_then(|m| m.as_ref())
                    .map(|m| m.version.clone());
                let stale = meta
                    .as_ref()
                    .ok()
                    .and_then(|m| m.as_ref())
                    .and_then(|m| {
                        current_cli_build_id()
                            .ok()
                            .map(|cli_id| m.build_id != cli_id)
                    })
                    .unwrap_or(false);
                if stale {
                    has_warnings = true;
                }
                (true, pid, version, stale)
            }
            _ => (false, None, None, false),
        };

    // Terminfo
    let terminfo_installed = check_terminfo_available("bmux-256color");
    if terminfo_installed != Some(true) {
        has_warnings = true;
    }

    // Terminal
    let config_ref = config.as_ref().cloned().unwrap_or_default();
    let configured_term = config_ref.behavior.pane_term.clone();
    let resolution = resolve_pane_term(&configured_term);
    let profile_name = terminal_profile_name(resolution.profile).to_string();
    if !resolution.warnings.is_empty() {
        has_warnings = true;
    }

    // Plugins
    let (plugin_enabled, plugin_incompatible, plugin_scan_error) = match &config {
        Some(cfg) => match scan_available_plugins(cfg, paths) {
            Ok(registry) => {
                let enabled = effective_enabled_plugins(cfg, &registry);
                let host = plugin_host_metadata();
                let incompatible: Vec<String> = enabled
                    .iter()
                    .filter(|id| {
                        registry.get(id).map_or(false, |p| {
                            !PluginRegistry::compatibility_report(p, &host).is_loadable()
                        })
                    })
                    .cloned()
                    .collect();
                if !incompatible.is_empty() {
                    has_warnings = true;
                }
                (enabled, incompatible, None)
            }
            Err(e) => {
                has_warnings = true;
                (Vec::new(), Vec::new(), Some(format!("{e:#}")))
            }
        },
        None => (Vec::new(), Vec::new(), None),
    };

    DoctorReport {
        has_warnings,
        config: DoctorConfigCheck {
            path: config_path,
            exists: config_exists,
            valid: config_valid,
            error: config_error,
        },
        paths: DoctorPathsCheck { dirs },
        server: DoctorServerCheck {
            running: server_running,
            pid: server_pid,
            version: server_version,
            stale_build,
        },
        terminfo: DoctorTerminfoCheck {
            installed: terminfo_installed,
        },
        terminal: DoctorTerminalCheck {
            configured_term,
            effective_term: resolution.pane_term,
            profile: profile_name,
            warnings: resolution.warnings,
        },
        plugins: DoctorPluginsCheck {
            enabled: plugin_enabled,
            incompatible: plugin_incompatible,
            scan_error: plugin_scan_error,
        },
    }
}

// ── output helpers ──────────────────────────────────────────────────────

fn print_ok(step: &str, message: &str) {
    println!("[OK]   {step}: {message}");
}

fn print_warn(step: &str, message: &str) {
    println!("[WARN] {step}: {message}");
}

fn print_info(step: &str, message: &str) {
    println!("[INFO] {step}: {message}");
}
