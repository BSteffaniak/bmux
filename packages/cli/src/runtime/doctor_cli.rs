use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_plugin::PluginRegistry;
use std::path::PathBuf;

use super::{
    ConnectionContext, check_terminfo_available, current_cli_build_id, effective_enabled_plugins,
    fetch_server_status, plugin_host_metadata, read_server_runtime_metadata, resolve_pane_term,
    scan_available_plugins, terminal_profile_name,
};

pub(super) async fn run_doctor(as_json: bool, hosted: bool) -> Result<u8> {
    if hosted {
        return run_hosted_doctor(as_json).await;
    }
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

    Ok(u8::from(has_warnings))
}

async fn run_hosted_doctor(as_json: bool) -> Result<u8> {
    let paths = ConfigPaths::default();
    let config = BmuxConfig::load().unwrap_or_default();
    let control_plane_url = std::env::var("BMUX_CONTROL_PLANE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.connections.control_plane_url.clone())
        .unwrap_or_else(|| "https://api.bmux.run".to_string());
    let auth_path = paths.runtime_dir.join("auth-state.json");
    let auth_token = std::fs::read_to_string(&auth_path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|json| {
            json.get("access_token")
                .and_then(|value| value.as_str())
                .map(ToString::to_string)
        });

    let auth_ok = auth_token.is_some();
    let control_plane_ok = if let Some(token) = auth_token.as_deref() {
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{control_plane_url}/v1/auth/whoami"))
            .bearer_auth(token)
            .send()
            .await;
        matches!(response, Ok(resp) if resp.status().is_success())
    } else {
        false
    };

    let host_state_path = paths.runtime_dir.join("host-state.json");
    let host_runtime_ok = hosted_runtime_is_running(&host_state_path);
    let share_lookup_ok = !config.connections.share_links.is_empty();

    let lines = vec![
        (
            "auth",
            auth_ok,
            "bmux auth login",
            format!("state: {}", auth_path.display()),
        ),
        (
            "control-plane",
            control_plane_ok,
            "check network or BMUX_CONTROL_PLANE_URL",
            control_plane_url.clone(),
        ),
        (
            "host-runtime",
            host_runtime_ok,
            "bmux host --daemon",
            format!("state: {}", host_state_path.display()),
        ),
        (
            "share-lookup",
            share_lookup_ok,
            "bmux share <target> --name <name>",
            format!("known links: {}", config.connections.share_links.len()),
        ),
    ];

    let has_failures = lines.iter().any(|(_, ok, _, _)| !*ok);

    if as_json {
        let checks: serde_json::Map<String, serde_json::Value> = lines
            .iter()
            .map(|(name, ok, hint, detail)| {
                (
                    (*name).to_string(),
                    serde_json::json!({ "ok": ok, "detail": detail, "fix": if *ok { serde_json::Value::Null } else { serde_json::Value::String("bmux setup".to_string()) }, "advanced": if *ok { serde_json::Value::Null } else { serde_json::Value::String((*hint).to_string()) }}),
                )
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"hosted": checks}))
                .context("failed to encode hosted doctor json")?
        );
    } else {
        if has_failures {
            println!("Status: not ready");
            println!("Fix: bmux setup");
        } else {
            println!("Status: ready");
            println!("Next: bmux hosts");
        }
        for (name, ok, hint, detail) in &lines {
            if *ok {
                println!("{name}: ok ({detail})");
            } else {
                println!("{name}: fail ({detail}) | Advanced: {hint}");
            }
        }
    }

    Ok(u8::from(has_failures))
}

fn hosted_runtime_is_running(path: &PathBuf) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    let Some(pid) = json
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
    else {
        return false;
    };

    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        return std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}")])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
            });
    }
}

// ── text output ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
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
    match fetch_server_status(ConnectionContext::default()).await {
        Ok(Some(status)) if status.running => {
            let meta = read_server_runtime_metadata();
            let version_str = meta
                .as_ref()
                .ok()
                .and_then(|m| m.as_ref())
                .map_or_else(|| "unknown".to_string(), |m| m.version.clone());
            let pid_str = meta
                .as_ref()
                .ok()
                .and_then(|m| m.as_ref())
                .map_or_else(|| "?".to_string(), |m| m.pid.to_string());

            print_ok(
                "server",
                &format!("running (pid {pid_str}, v{version_str})"),
            );

            // Check for stale build
            if let Ok(Some(meta)) = &meta
                && let Ok(cli_build_id) = current_cli_build_id()
                && meta.build_id != cli_build_id
            {
                print_warn(
                    "server",
                    "stale build detected (server build differs from CLI)",
                );
                warnings = true;
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

#[allow(clippy::too_many_lines)]
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
        match fetch_server_status(ConnectionContext::default()).await {
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
    let config_ref = config.clone().unwrap_or_default();
    let configured_term = config_ref.behavior.pane_term;
    let resolution = resolve_pane_term(&configured_term);
    let profile_name = terminal_profile_name(resolution.profile).to_string();
    if !resolution.warnings.is_empty() {
        has_warnings = true;
    }

    // Plugins
    #[allow(clippy::option_if_let_else)]
    // Nested match with side effects reads better than map_or_else
    let (plugin_enabled, plugin_incompatible, plugin_scan_error) = match &config {
        Some(cfg) => match scan_available_plugins(cfg, paths) {
            Ok(registry) => {
                let enabled = effective_enabled_plugins(cfg, &registry);
                let host = plugin_host_metadata();
                let incompatible: Vec<String> = enabled
                    .iter()
                    .filter(|id| {
                        registry.get(id).is_some_and(|p| {
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
