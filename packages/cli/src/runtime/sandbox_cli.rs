use anyhow::{Context, Result};
use bmux_cli_schema::SandboxEnvModeArg;
use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SANDBOX_PREFIX: &str = "bmux-sbx-";
const MANIFEST_FILE: &str = "sandbox.json";
const PID_MARKER_FILE: &str = "sandbox.pid";
const LOCK_FILE: &str = "sandbox.lock";
const DEFAULT_CLEANUP_MIN_AGE: Duration = Duration::from_secs(300);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const LOCK_FRESHNESS: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct CleanupEntry {
    path: String,
    age_secs: u64,
    status: String,
    removed: bool,
}

#[derive(Debug, Clone, Copy)]
struct CleanupSkipped {
    running: usize,
    recent: usize,
    not_failed: usize,
}

#[derive(Debug)]
struct CleanupScan {
    scanned: usize,
    entries: Vec<CleanupEntry>,
    skipped: CleanupSkipped,
}

#[derive(Debug, Clone, Serialize)]
struct SandboxListEntry {
    id: String,
    root: String,
    status: String,
    age_secs: u64,
    exit_code: Option<i32>,
    kept: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheck {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Clone)]
struct SandboxPaths {
    root_dir: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    runtime_dir: PathBuf,
    state_dir: PathBuf,
    log_dir: PathBuf,
    tmp_dir: PathBuf,
    home_dir: PathBuf,
    config_paths: ConfigPaths,
}

impl SandboxPaths {
    fn new(name: Option<&str>) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let suffix = name
            .map(sanitize_component)
            .filter(|value| !value.is_empty())
            .map_or_else(String::new, |value| format!("-{value}"));
        let root_dir = std::env::temp_dir().join(format!(
            "{SANDBOX_PREFIX}{nanos:x}-{}{}",
            std::process::id(),
            suffix
        ));

        let config_home = root_dir.join("config");
        let data_home = root_dir.join("data");
        let runtime_dir = root_dir.join("runtime");
        let state_dir = root_dir.join("state");
        let log_dir = root_dir.join("logs");
        let tmp_dir = root_dir.join("tmp");
        let home_dir = root_dir.join("home");

        let config_paths = ConfigPaths::new(
            config_home.join("bmux"),
            runtime_dir.clone(),
            data_home.join("bmux"),
            state_dir.clone(),
        );

        Self {
            root_dir,
            config_home,
            data_home,
            runtime_dir,
            state_dir,
            log_dir,
            tmp_dir,
            home_dir,
            config_paths,
        }
    }

    fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_home)
            .with_context(|| format!("failed creating {}", self.config_home.display()))?;
        std::fs::create_dir_all(&self.data_home)
            .with_context(|| format!("failed creating {}", self.data_home.display()))?;
        std::fs::create_dir_all(&self.runtime_dir)
            .with_context(|| format!("failed creating {}", self.runtime_dir.display()))?;
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("failed creating {}", self.state_dir.display()))?;
        std::fs::create_dir_all(&self.log_dir)
            .with_context(|| format!("failed creating {}", self.log_dir.display()))?;
        std::fs::create_dir_all(&self.tmp_dir)
            .with_context(|| format!("failed creating {}", self.tmp_dir.display()))?;
        std::fs::create_dir_all(&self.home_dir)
            .with_context(|| format!("failed creating {}", self.home_dir.display()))?;
        std::fs::create_dir_all(&self.config_paths.config_dir).with_context(|| {
            format!("failed creating {}", self.config_paths.config_dir.display())
        })?;
        std::fs::create_dir_all(&self.config_paths.data_dir)
            .with_context(|| format!("failed creating {}", self.config_paths.data_dir.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SandboxManifestPaths {
    root: String,
    logs: String,
    runtime: String,
    state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SandboxManifest {
    id: String,
    created_at_unix_ms: u128,
    updated_at_unix_ms: u128,
    pid: u32,
    bmux_bin: String,
    command: Vec<String>,
    env_mode: String,
    status: String,
    exit_code: Option<i32>,
    kept: bool,
    paths: SandboxManifestPaths,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SandboxLock {
    pid: u32,
    updated_at_unix_ms: u128,
}

#[derive(Debug, Serialize)]
struct SandboxRunReport {
    sandbox_id: String,
    sandbox_root: String,
    bmux_bin: String,
    env_mode: String,
    status: String,
    keep_requested: bool,
    kept: bool,
    exit_code: i32,
    duration_ms: u128,
}

struct RunOutcome<'a> {
    binary: &'a Path,
    options: &'a RunSandboxOptions<'a>,
    kept: bool,
    keep_on_failure: bool,
    raw_exit_code: i32,
    final_status: &'a str,
    duration_ms: u128,
}

pub(super) struct RunSandboxOptions<'a> {
    pub(super) bmux_bin: Option<&'a str>,
    pub(super) env_mode: SandboxEnvModeArg,
    pub(super) keep: bool,
    pub(super) json: bool,
    pub(super) print_env: bool,
    pub(super) timeout_secs: Option<u64>,
    pub(super) name: Option<&'a str>,
}

pub(super) async fn run_sandbox_run(
    options: RunSandboxOptions<'_>,
    command_args: &[String],
) -> Result<u8> {
    let sandbox = SandboxPaths::new(options.name);
    sandbox.ensure_dirs()?;
    write_pid_marker(&sandbox.root_dir)?;

    let binary = resolve_bmux_binary(options.bmux_bin)?;
    let mut manifest = build_manifest(
        &sandbox,
        &binary,
        command_args,
        options.env_mode,
        options.keep,
    );
    write_manifest(&sandbox.root_dir, &manifest)?;
    write_lock(&sandbox.root_dir)?;

    if options.print_env {
        let env = collect_effective_sandbox_env(&sandbox, options.env_mode);
        println!("{}", serde_json::to_string_pretty(&env)?);
    }

    let (status, timed_out, duration_ms) = match spawn_and_wait(
        &binary,
        &sandbox,
        options.env_mode,
        command_args,
        options.timeout_secs.map(Duration::from_secs),
    )
    .await
    {
        Ok(values) => values,
        Err(error) => {
            clear_lock(&sandbox.root_dir);
            return Err(error);
        }
    };

    let raw_exit_code = status.code().unwrap_or(if timed_out { 124 } else { 1 });
    let keep_on_failure = !status.success() || timed_out;
    let kept = options.keep || keep_on_failure;
    let final_status = if timed_out {
        "timed_out"
    } else if status.success() {
        "succeeded"
    } else {
        "failed"
    };

    manifest.updated_at_unix_ms = unix_millis_now();
    manifest.status = final_status.to_string();
    manifest.exit_code = Some(raw_exit_code);
    manifest.kept = kept;
    write_manifest(&sandbox.root_dir, &manifest)?;
    clear_lock(&sandbox.root_dir);

    emit_run_output(
        &sandbox,
        &RunOutcome {
            binary: &binary,
            options: &options,
            kept,
            keep_on_failure,
            raw_exit_code,
            final_status,
            duration_ms,
        },
        &manifest,
    )?;

    if !kept {
        let _ = std::fs::remove_dir_all(&sandbox.root_dir);
    }

    Ok(exit_code_to_u8(raw_exit_code))
}

fn build_manifest(
    sandbox: &SandboxPaths,
    binary: &Path,
    command_args: &[String],
    env_mode: SandboxEnvModeArg,
    keep: bool,
) -> SandboxManifest {
    SandboxManifest {
        id: sandbox_id_from_root(&sandbox.root_dir),
        created_at_unix_ms: unix_millis_now(),
        updated_at_unix_ms: unix_millis_now(),
        pid: std::process::id(),
        bmux_bin: binary.to_string_lossy().to_string(),
        command: command_args.to_vec(),
        env_mode: sandbox_env_mode_name(env_mode).to_string(),
        status: "running".to_string(),
        exit_code: None,
        kept: keep,
        paths: SandboxManifestPaths {
            root: sandbox.root_dir.to_string_lossy().to_string(),
            logs: sandbox.log_dir.to_string_lossy().to_string(),
            runtime: sandbox.runtime_dir.to_string_lossy().to_string(),
            state: sandbox.state_dir.to_string_lossy().to_string(),
        },
    }
}

async fn spawn_and_wait(
    binary: &Path,
    sandbox: &SandboxPaths,
    env_mode: SandboxEnvModeArg,
    command_args: &[String],
    timeout: Option<Duration>,
) -> Result<(ExitStatus, bool, u128)> {
    let mut command = ProcessCommand::new(binary);
    command.args(command_args);
    command.stdin(Stdio::inherit());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    apply_sandbox_env(&mut command, sandbox, env_mode);

    let start = Instant::now();
    let mut last_heartbeat = Instant::now();
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed spawning sandbox command with {}",
            binary.to_string_lossy()
        )
    })?;

    let mut timed_out = false;
    let status = loop {
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            let _ = write_lock(&sandbox.root_dir);
            last_heartbeat = Instant::now();
        }

        if let Some(status) = child
            .try_wait()
            .context("failed waiting for sandbox child")?
        {
            break status;
        }

        if timeout.is_some_and(|limit| start.elapsed() >= limit) {
            timed_out = true;
            let _ = child.kill();
            break child
                .wait()
                .context("failed waiting after sandbox timeout")?;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    Ok((status, timed_out, start.elapsed().as_millis()))
}

fn emit_run_output(
    sandbox: &SandboxPaths,
    outcome: &RunOutcome<'_>,
    manifest: &SandboxManifest,
) -> Result<()> {
    let repro = format_repro_command(outcome.options, manifest.command.as_slice());
    if outcome.options.json {
        let report = SandboxRunReport {
            sandbox_id: manifest.id.clone(),
            sandbox_root: sandbox.root_dir.to_string_lossy().to_string(),
            bmux_bin: outcome.binary.to_string_lossy().to_string(),
            env_mode: sandbox_env_mode_name(outcome.options.env_mode).to_string(),
            status: outcome.final_status.to_string(),
            keep_requested: outcome.options.keep,
            kept: outcome.kept,
            exit_code: outcome.raw_exit_code,
            duration_ms: outcome.duration_ms,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if outcome.keep_on_failure {
            eprintln!(
                "sandbox command exited with {}; keeping sandbox at {}",
                outcome.raw_exit_code,
                sandbox.root_dir.display()
            );
            eprintln!("repro: {repro}");
        } else if outcome.kept {
            println!("kept sandbox at {}", sandbox.root_dir.display());
        }

        if outcome.kept {
            eprintln!("sandbox logs: {}", sandbox.log_dir.display());
        }
    }
    Ok(())
}

pub(super) fn run_sandbox_list(
    status_filter: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<u8> {
    let mut entries = collect_sandbox_directories()
        .into_iter()
        .map(|root| sandbox_list_entry(&root))
        .collect::<Vec<_>>();

    entries.sort_by(|left, right| left.age_secs.cmp(&right.age_secs));

    if let Some(filter) = status_filter {
        entries.retain(|entry| entry.status == filter);
    }
    if limit > 0 {
        entries.truncate(limit);
    }

    if json {
        let payload = serde_json::json!({ "sandboxes": entries });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if entries.is_empty() {
        println!("no sandboxes found");
    } else {
        println!("ID                               STATUS   AGE   EXIT  KEPT  ROOT");
        for entry in entries {
            println!(
                "{:<32} {:<8} {:>4}s {:>5} {:>5}  {}",
                entry.id,
                entry.status,
                entry.age_secs,
                entry
                    .exit_code
                    .map_or_else(|| "-".to_string(), |value| value.to_string()),
                entry.kept.map_or_else(
                    || "-".to_string(),
                    |value| if value { "yes" } else { "no" }.to_string()
                ),
                entry.root,
            );
        }
    }

    Ok(0)
}

pub(super) fn run_sandbox_inspect(
    target: Option<&str>,
    latest: bool,
    latest_failed: bool,
    tail: usize,
    json: bool,
) -> Result<u8> {
    let root = resolve_inspect_target(target, latest, latest_failed)?;
    let manifest = read_manifest(&root)?;
    let log_tail = read_log_tail(&root, tail);
    let running = sandbox_process_alive(&root) || sandbox_socket_alive(&root);

    if json {
        let payload = serde_json::json!({
            "manifest": manifest,
            "running": running,
            "log_tail": log_tail,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("id: {}", manifest.id);
        println!("status: {}", manifest.status);
        println!("running: {running}");
        println!(
            "exit_code: {}",
            manifest
                .exit_code
                .map_or_else(|| "-".to_string(), |value| value.to_string())
        );
        println!("kept: {}", if manifest.kept { "yes" } else { "no" });
        println!("env_mode: {}", manifest.env_mode);
        println!("root: {}", root.display());
        println!("bmux_bin: {}", manifest.bmux_bin);
        println!("command: {}", manifest.command.join(" "));
        println!("repro: {}", format_repro_command_from_manifest(&manifest));
        println!("logs:");
        for line in log_tail {
            println!("  {line}");
        }
    }

    Ok(0)
}

fn resolve_inspect_target(
    target: Option<&str>,
    latest: bool,
    latest_failed: bool,
) -> Result<PathBuf> {
    if let Some(target) = target {
        return resolve_sandbox_target(target);
    }

    if latest || latest_failed {
        return resolve_latest_sandbox(latest_failed);
    }

    anyhow::bail!("inspect target required (provide <id|path>, --latest, or --latest-failed)")
}

fn resolve_latest_sandbox(failed_only: bool) -> Result<PathBuf> {
    let mut candidates = collect_sandbox_directories()
        .into_iter()
        .map(|path| {
            let age = path
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .unwrap_or_default();
            (path, age)
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| left.1.cmp(&right.1));
    for (path, _) in candidates {
        if !failed_only || matches!(sandbox_status_for_dir(&path), "failed") {
            return Ok(path);
        }
    }

    if failed_only {
        anyhow::bail!("no failed sandboxes found");
    }

    anyhow::bail!("no sandboxes found")
}

pub(super) fn run_sandbox_doctor(id: Option<&str>, json: bool) -> Result<u8> {
    let mut checks = Vec::new();

    let temp_dir = std::env::temp_dir();
    checks.push(DoctorCheck {
        name: "temp_dir_writable".to_string(),
        ok: std::fs::create_dir_all(&temp_dir).is_ok(),
        detail: temp_dir.display().to_string(),
    });

    let current_exe = std::env::current_exe();
    checks.push(DoctorCheck {
        name: "current_bmux_executable".to_string(),
        ok: current_exe.is_ok(),
        detail: current_exe.ok().map_or_else(
            || "unavailable".to_string(),
            |path| path.display().to_string(),
        ),
    });

    if let Some(target) = id {
        append_target_doctor_checks(&mut checks, target);
    }

    let ok = checks.iter().all(|check| check.ok);
    if json {
        let payload = serde_json::json!({
            "ok": ok,
            "checks": checks,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("sandbox doctor: {}", if ok { "ok" } else { "issues found" });
        for check in checks {
            println!(
                "  {} {} ({})",
                if check.ok { "OK" } else { "FAIL" },
                check.name,
                check.detail
            );
        }
    }

    Ok(u8::from(!ok))
}

pub(super) fn run_sandbox_bundle(target: &str, output: Option<&str>, json: bool) -> Result<u8> {
    let root = resolve_sandbox_target(target)?;
    let manifest = read_manifest(&root)?;

    let output_root = output.map_or_else(
        || {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("sandbox-bundles")
        },
        PathBuf::from,
    );
    std::fs::create_dir_all(&output_root)
        .with_context(|| format!("failed creating {}", output_root.display()))?;

    let bundle_dir = output_root.join(format!("{}-{}", manifest.id, unix_millis_now()));
    std::fs::create_dir_all(&bundle_dir)
        .with_context(|| format!("failed creating {}", bundle_dir.display()))?;

    copy_if_exists(&root.join(MANIFEST_FILE), &bundle_dir.join(MANIFEST_FILE))?;
    copy_if_exists(
        &root.join(PID_MARKER_FILE),
        &bundle_dir.join(PID_MARKER_FILE),
    )?;
    copy_if_exists(&root.join(LOCK_FILE), &bundle_dir.join(LOCK_FILE))?;

    let logs_src = root.join("logs");
    if logs_src.exists() {
        copy_directory_recursive(&logs_src, &bundle_dir.join("logs"))?;
    }

    let repro_path = bundle_dir.join("repro.txt");
    std::fs::write(&repro_path, format_repro_command_from_manifest(&manifest))
        .with_context(|| format!("failed writing {}", repro_path.display()))?;

    if json {
        let payload = serde_json::json!({
            "bundle_dir": bundle_dir.to_string_lossy().to_string(),
            "sandbox_root": root.to_string_lossy().to_string(),
            "sandbox_id": manifest.id,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("sandbox bundle created: {}", bundle_dir.display());
    }

    Ok(0)
}

fn append_target_doctor_checks(checks: &mut Vec<DoctorCheck>, target: &str) {
    match resolve_sandbox_target(target) {
        Ok(root) => {
            let manifest_exists = root.join(MANIFEST_FILE).exists();
            let runtime_exists = root.join("runtime").exists();
            checks.push(DoctorCheck {
                name: "target_exists".to_string(),
                ok: root.exists(),
                detail: root.display().to_string(),
            });
            checks.push(DoctorCheck {
                name: "target_manifest_present".to_string(),
                ok: manifest_exists,
                detail: root.join(MANIFEST_FILE).display().to_string(),
            });
            checks.push(DoctorCheck {
                name: "target_runtime_dir_present".to_string(),
                ok: runtime_exists,
                detail: root.join("runtime").display().to_string(),
            });
        }
        Err(error) => checks.push(DoctorCheck {
            name: "target_resolve".to_string(),
            ok: false,
            detail: error.to_string(),
        }),
    }
}

fn copy_if_exists(source: &Path, destination: &Path) -> Result<()> {
    if source.exists() {
        std::fs::copy(source, destination)
            .with_context(|| format!("failed copying {}", source.display()))?;
    }
    Ok(())
}

fn copy_directory_recursive(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)
        .with_context(|| format!("failed creating {}", destination.display()))?;

    for entry in
        std::fs::read_dir(source).with_context(|| format!("failed reading {}", source.display()))?
    {
        let entry = entry.with_context(|| "failed reading directory entry".to_string())?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory_recursive(&source_path, &destination_path)?;
        } else {
            std::fs::copy(&source_path, &destination_path)
                .with_context(|| format!("failed copying {}", source_path.display()))?;
        }
    }

    Ok(())
}

pub(super) fn run_sandbox_cleanup(
    dry_run: bool,
    failed_only: bool,
    older_than_secs: Option<u64>,
    json: bool,
) -> Result<u8> {
    let older_than = older_than_secs.map_or(DEFAULT_CLEANUP_MIN_AGE, Duration::from_secs);
    let scan = cleanup_orphaned_sandboxes(dry_run, failed_only, older_than);
    let orphaned = scan.entries.len();

    if json {
        let report = serde_json::json!({
            "scanned": scan.scanned,
            "orphaned": orphaned,
            "dry_run": dry_run,
            "failed_only": failed_only,
            "older_than_secs": older_than.as_secs(),
            "skipped_running": scan.skipped.running,
            "skipped_recent": scan.skipped.recent,
            "skipped_not_failed": scan.skipped.not_failed,
            "entries": scan.entries,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if orphaned > 0 {
        for entry in &scan.entries {
            let status = if entry.removed { "removed" } else { "found" };
            println!(
                "  {status}: {} (status: {}, age: {}s)",
                entry.path, entry.status, entry.age_secs
            );
        }
        if dry_run {
            println!("{orphaned} orphaned sandbox(es) found (dry run, not removed)");
        } else {
            let removed = scan.entries.iter().filter(|entry| entry.removed).count();
            println!("{removed} orphaned sandbox(es) removed");
        }
        println!(
            "skipped: running={}, recent={}, not_failed={}",
            scan.skipped.running, scan.skipped.recent, scan.skipped.not_failed
        );
    } else {
        println!(
            "no orphaned sandboxes found ({} scanned; skipped running={}, recent={}, not_failed={})",
            scan.scanned, scan.skipped.running, scan.skipped.recent, scan.skipped.not_failed
        );
    }

    Ok(0)
}

fn cleanup_orphaned_sandboxes(
    dry_run: bool,
    failed_only: bool,
    older_than: Duration,
) -> CleanupScan {
    let now = SystemTime::now();
    let mut scanned = 0;
    let mut entries = Vec::new();
    let mut skipped = CleanupSkipped {
        running: 0,
        recent: 0,
        not_failed: 0,
    };

    for root_path in collect_sandbox_directories() {
        scanned += 1;
        let age = root_path
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or_default();
        if age < older_than {
            skipped.recent += 1;
            continue;
        }

        let status = sandbox_status_for_dir(&root_path);
        if status == "running" {
            skipped.running += 1;
            continue;
        }
        if failed_only && status != "failed" {
            skipped.not_failed += 1;
            continue;
        }

        let removed = if dry_run {
            false
        } else {
            std::fs::remove_dir_all(&root_path).is_ok()
        };

        entries.push(CleanupEntry {
            path: root_path.to_string_lossy().to_string(),
            age_secs: age.as_secs(),
            status: status.to_string(),
            removed,
        });
    }

    CleanupScan {
        scanned,
        entries,
        skipped,
    }
}

fn collect_sandbox_directories() -> Vec<PathBuf> {
    let Ok(dir_entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return Vec::new();
    };

    dir_entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(SANDBOX_PREFIX))
        })
        .collect()
}

fn sandbox_list_entry(root: &Path) -> SandboxListEntry {
    let now = SystemTime::now();
    let age = root
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| now.duration_since(modified).ok())
        .unwrap_or_default();

    let manifest = read_manifest(root).ok();
    SandboxListEntry {
        id: sandbox_id_from_root(root),
        root: root.to_string_lossy().to_string(),
        status: sandbox_status_for_dir(root).to_string(),
        age_secs: age.as_secs(),
        exit_code: manifest.as_ref().and_then(|value| value.exit_code),
        kept: manifest.as_ref().map(|value| value.kept),
    }
}

fn sandbox_status_for_dir(root: &Path) -> &'static str {
    if sandbox_lock_is_fresh(root) || sandbox_process_alive(root) || sandbox_socket_alive(root) {
        return "running";
    }

    if let Ok(manifest) = read_manifest(root) {
        if matches!(manifest.status.as_str(), "failed" | "timed_out" | "killed") {
            "failed"
        } else {
            "stopped"
        }
    } else {
        "stopped"
    }
}

fn read_manifest(root: &Path) -> Result<SandboxManifest> {
    let manifest_path = root.join(MANIFEST_FILE);
    let bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("failed reading {}", manifest_path.display()))?;
    serde_json::from_slice::<SandboxManifest>(&bytes)
        .with_context(|| format!("failed parsing {}", manifest_path.display()))
}

fn write_manifest(root: &Path, manifest: &SandboxManifest) -> Result<()> {
    let manifest_path = root.join(MANIFEST_FILE);
    let encoded = serde_json::to_vec_pretty(manifest)?;
    write_atomic_file(&manifest_path, &encoded)
        .with_context(|| format!("failed writing {}", manifest_path.display()))
}

fn resolve_sandbox_target(target: &str) -> Result<PathBuf> {
    let maybe_path = PathBuf::from(target);
    if maybe_path.is_absolute() && maybe_path.exists() {
        return Ok(maybe_path);
    }

    let direct = std::env::temp_dir().join(target);
    if direct.exists() {
        return Ok(direct);
    }

    if let Some(found) = collect_sandbox_directories()
        .iter()
        .find(|candidate| sandbox_id_from_root(candidate).starts_with(target))
    {
        return Ok(found.clone());
    }

    anyhow::bail!("sandbox target not found: {target}")
}

fn read_log_tail(root: &Path, tail: usize) -> Vec<String> {
    let log_dir = root.join("logs");
    let Some(log_path) = newest_regular_file(&log_dir) else {
        return vec!["<no log files found>".to_string()];
    };

    let Ok(contents) = std::fs::read_to_string(&log_path) else {
        return vec![format!("<failed reading {}>", log_path.display())];
    };

    let mut lines = contents.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    let keep = tail.max(1);
    if lines.len() > keep {
        lines.drain(0..(lines.len() - keep));
    }
    lines
}

fn newest_regular_file(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            if path.is_file() {
                Some((path, modified))
            } else {
                None
            }
        })
        .max_by_key(|(_, modified)| *modified)
        .map(|(path, _)| path)
}

fn write_pid_marker(root_dir: &Path) -> Result<()> {
    let marker_path = root_dir.join(PID_MARKER_FILE);
    std::fs::write(&marker_path, std::process::id().to_string())
        .with_context(|| format!("failed writing {}", marker_path.display()))
}

fn write_lock(root_dir: &Path) -> Result<()> {
    let lock_path = root_dir.join(LOCK_FILE);
    let lock = SandboxLock {
        pid: std::process::id(),
        updated_at_unix_ms: unix_millis_now(),
    };
    let bytes = serde_json::to_vec(&lock)?;
    write_atomic_file(&lock_path, &bytes)
}

fn clear_lock(root_dir: &Path) {
    let _ = std::fs::remove_file(root_dir.join(LOCK_FILE));
}

fn sandbox_lock_is_fresh(root_dir: &Path) -> bool {
    let lock_path = root_dir.join(LOCK_FILE);
    let Ok(bytes) = std::fs::read(lock_path) else {
        return false;
    };
    let Ok(lock) = serde_json::from_slice::<SandboxLock>(&bytes) else {
        return false;
    };
    let now = unix_millis_now();
    if now.saturating_sub(lock.updated_at_unix_ms) > LOCK_FRESHNESS.as_millis() {
        return false;
    }
    is_pid_alive(lock.pid)
}

fn write_atomic_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, bytes)
        .with_context(|| format!("failed writing {}", temp_path.display()))?;
    std::fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed renaming {} to {}",
            temp_path.display(),
            path.display()
        )
    })
}

fn format_repro_command(options: &RunSandboxOptions<'_>, command_args: &[String]) -> String {
    let mut parts = vec![
        "bmux".to_string(),
        "sandbox".to_string(),
        "run".to_string(),
        "--env-mode".to_string(),
        sandbox_env_mode_name(options.env_mode).to_string(),
    ];
    if let Some(bin) = options.bmux_bin {
        parts.push("--bmux-bin".to_string());
        parts.push(shell_quote(bin));
    }
    if options.keep {
        parts.push("--keep".to_string());
    }
    parts.push("--".to_string());
    parts.extend(command_args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn format_repro_command_from_manifest(manifest: &SandboxManifest) -> String {
    let env_mode = if matches!(manifest.env_mode.as_str(), "clean") {
        SandboxEnvModeArg::Clean
    } else if matches!(manifest.env_mode.as_str(), "hermetic") {
        SandboxEnvModeArg::Hermetic
    } else {
        SandboxEnvModeArg::Inherit
    };
    let options = RunSandboxOptions {
        bmux_bin: Some(&manifest.bmux_bin),
        env_mode,
        keep: manifest.kept,
        json: false,
        print_env: false,
        timeout_secs: None,
        name: None,
    };
    format_repro_command(&options, manifest.command.as_slice())
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '/' | '.' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn apply_sandbox_env(
    command: &mut ProcessCommand,
    sandbox: &SandboxPaths,
    env_mode: SandboxEnvModeArg,
) {
    if matches!(
        env_mode,
        SandboxEnvModeArg::Clean | SandboxEnvModeArg::Hermetic
    ) {
        command.env_clear();
    }

    command
        .env("BMUX_CONFIG_DIR", &sandbox.config_paths.config_dir)
        .env("BMUX_RUNTIME_DIR", &sandbox.config_paths.runtime_dir)
        .env("BMUX_DATA_DIR", &sandbox.config_paths.data_dir)
        .env("BMUX_STATE_DIR", &sandbox.config_paths.state_dir)
        .env("BMUX_LOG_DIR", &sandbox.log_dir)
        .env("XDG_CONFIG_HOME", &sandbox.config_home)
        .env("XDG_DATA_HOME", &sandbox.data_home)
        .env("XDG_RUNTIME_DIR", &sandbox.runtime_dir)
        .env("TMPDIR", &sandbox.tmp_dir)
        .env("HOME", &sandbox.home_dir)
        .env("TERM", "xterm-256color")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8");

    if matches!(env_mode, SandboxEnvModeArg::Clean) {
        if let Ok(path) = std::env::var("PATH") {
            command.env("PATH", path);
        }
        if let Ok(user) = std::env::var("USER") {
            command.env("USER", user);
        }
        if let Ok(shell) = std::env::var("SHELL") {
            command.env("SHELL", shell);
        }
    }

    if matches!(env_mode, SandboxEnvModeArg::Hermetic) {
        command.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    }
}

fn collect_effective_sandbox_env(
    sandbox: &SandboxPaths,
    env_mode: SandboxEnvModeArg,
) -> serde_json::Value {
    let mut vars = serde_json::Map::new();
    vars.insert(
        "BMUX_CONFIG_DIR".to_string(),
        serde_json::Value::String(
            sandbox
                .config_paths
                .config_dir
                .to_string_lossy()
                .to_string(),
        ),
    );
    vars.insert(
        "BMUX_RUNTIME_DIR".to_string(),
        serde_json::Value::String(
            sandbox
                .config_paths
                .runtime_dir
                .to_string_lossy()
                .to_string(),
        ),
    );
    vars.insert(
        "BMUX_DATA_DIR".to_string(),
        serde_json::Value::String(sandbox.config_paths.data_dir.to_string_lossy().to_string()),
    );
    vars.insert(
        "BMUX_STATE_DIR".to_string(),
        serde_json::Value::String(sandbox.config_paths.state_dir.to_string_lossy().to_string()),
    );
    vars.insert(
        "BMUX_LOG_DIR".to_string(),
        serde_json::Value::String(sandbox.log_dir.to_string_lossy().to_string()),
    );
    vars.insert(
        "XDG_CONFIG_HOME".to_string(),
        serde_json::Value::String(sandbox.config_home.to_string_lossy().to_string()),
    );
    vars.insert(
        "XDG_DATA_HOME".to_string(),
        serde_json::Value::String(sandbox.data_home.to_string_lossy().to_string()),
    );
    vars.insert(
        "XDG_RUNTIME_DIR".to_string(),
        serde_json::Value::String(sandbox.runtime_dir.to_string_lossy().to_string()),
    );
    vars.insert(
        "TMPDIR".to_string(),
        serde_json::Value::String(sandbox.tmp_dir.to_string_lossy().to_string()),
    );
    vars.insert(
        "HOME".to_string(),
        serde_json::Value::String(sandbox.home_dir.to_string_lossy().to_string()),
    );
    vars.insert(
        "TERM".to_string(),
        serde_json::Value::String("xterm-256color".to_string()),
    );
    vars.insert(
        "LANG".to_string(),
        serde_json::Value::String("C.UTF-8".to_string()),
    );
    vars.insert(
        "LC_ALL".to_string(),
        serde_json::Value::String("C.UTF-8".to_string()),
    );

    if matches!(env_mode, SandboxEnvModeArg::Clean) {
        if let Ok(path) = std::env::var("PATH") {
            vars.insert("PATH".to_string(), serde_json::Value::String(path));
        }
        if let Ok(user) = std::env::var("USER") {
            vars.insert("USER".to_string(), serde_json::Value::String(user));
        }
        if let Ok(shell) = std::env::var("SHELL") {
            vars.insert("SHELL".to_string(), serde_json::Value::String(shell));
        }
    }

    if matches!(env_mode, SandboxEnvModeArg::Hermetic) {
        vars.insert(
            "PATH".to_string(),
            serde_json::Value::String("/usr/bin:/bin:/usr/sbin:/sbin".to_string()),
        );
    }

    serde_json::Value::Object(vars)
}

fn resolve_bmux_binary(bmux_bin: Option<&str>) -> Result<PathBuf> {
    bmux_bin.map_or_else(
        || std::env::current_exe().context("failed resolving current bmux executable"),
        resolve_explicit_binary,
    )
}

fn resolve_explicit_binary(path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        anyhow::bail!("--bmux-bin requires a non-empty path");
    }
    let candidate = PathBuf::from(trimmed);
    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        std::env::current_dir()
            .context("failed resolving current directory for --bmux-bin")?
            .join(candidate)
    };
    if !resolved.exists() {
        anyhow::bail!("--bmux-bin path does not exist: {}", resolved.display());
    }
    Ok(resolved)
}

const fn sandbox_env_mode_name(mode: SandboxEnvModeArg) -> &'static str {
    match mode {
        SandboxEnvModeArg::Clean => "clean",
        SandboxEnvModeArg::Inherit => "inherit",
        SandboxEnvModeArg::Hermetic => "hermetic",
    }
}

fn sandbox_id_from_root(root: &Path) -> String {
    root.file_name()
        .map_or_else(String::new, |value| value.to_string_lossy().to_string())
}

fn exit_code_to_u8(code: i32) -> u8 {
    if code < 0 {
        1
    } else if code > i32::from(u8::MAX) {
        u8::MAX
    } else {
        u8::try_from(code).unwrap_or(1)
    }
}

fn unix_millis_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn sandbox_process_alive(root_dir: &Path) -> bool {
    let marker_path = root_dir.join(PID_MARKER_FILE);
    std::fs::read_to_string(marker_path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u32>().ok())
        .is_some_and(is_pid_alive)
}

fn sandbox_socket_alive(root_dir: &Path) -> bool {
    #[cfg(unix)]
    {
        let socket_path = root_dir.join("runtime").join("server.sock");
        if !socket_path.exists() {
            return false;
        }
        std::os::unix::net::UnixStream::connect(socket_path).is_ok()
    }

    #[cfg(not(unix))]
    {
        let _ = root_dir;
        false
    }
}

fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    #[cfg(unix)]
    {
        ProcessCommand::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(windows)]
    {
        ProcessCommand::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
            })
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SandboxEnvModeArg, SandboxPaths, apply_sandbox_env, exit_code_to_u8, sanitize_component,
    };

    fn env_value(command: &std::process::Command, key: &str) -> Option<std::ffi::OsString> {
        command.get_envs().find_map(|(name, value)| {
            if name == std::ffi::OsStr::new(key) {
                value.map(std::ffi::OsStr::to_os_string)
            } else {
                None
            }
        })
    }

    #[test]
    fn sanitize_component_rewrites_non_alnum() {
        assert_eq!(sanitize_component("my sandbox/test"), "my-sandbox-test");
    }

    #[test]
    fn sandbox_env_sets_clean_defaults() {
        let sandbox = SandboxPaths::new(None);
        sandbox
            .ensure_dirs()
            .expect("sandbox dirs should be created");
        let mut command = std::process::Command::new("sh");
        apply_sandbox_env(&mut command, &sandbox, SandboxEnvModeArg::Clean);
        let expected_log_dir = sandbox.log_dir.clone();
        let root_dir = sandbox.root_dir.clone();

        assert_eq!(
            env_value(&command, "BMUX_LOG_DIR"),
            Some(expected_log_dir.into_os_string())
        );
        assert_eq!(
            env_value(&command, "TERM"),
            Some(std::ffi::OsString::from("xterm-256color"))
        );

        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn sandbox_env_sets_inherit_defaults() {
        let sandbox = SandboxPaths::new(None);
        sandbox
            .ensure_dirs()
            .expect("sandbox dirs should be created");
        let mut command = std::process::Command::new("sh");
        apply_sandbox_env(&mut command, &sandbox, SandboxEnvModeArg::Inherit);
        let expected_runtime_dir = sandbox.runtime_dir.clone();
        let root_dir = sandbox.root_dir.clone();

        assert_eq!(
            env_value(&command, "XDG_RUNTIME_DIR"),
            Some(expected_runtime_dir.into_os_string())
        );

        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn sandbox_env_sets_hermetic_path() {
        let sandbox = SandboxPaths::new(None);
        sandbox
            .ensure_dirs()
            .expect("sandbox dirs should be created");
        let mut command = std::process::Command::new("sh");
        apply_sandbox_env(&mut command, &sandbox, SandboxEnvModeArg::Hermetic);
        let root_dir = sandbox.root_dir.clone();

        assert_eq!(
            env_value(&command, "PATH"),
            Some(std::ffi::OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"))
        );

        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn exit_code_conversion_clamps() {
        assert_eq!(exit_code_to_u8(-1), 1);
        assert_eq!(exit_code_to_u8(300), 255);
        assert_eq!(exit_code_to_u8(7), 7);
    }
}
