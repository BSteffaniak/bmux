use anyhow::{Context, Result};
use bmux_cli_schema::SandboxEnvModeArg;
use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::sandbox_meta::{
    LOCK_FILE, MANIFEST_FILE, SandboxManifest, SandboxManifestPaths,
    clear_lock as clear_sandbox_lock, prune_missing_index_entries,
    read_index_entries as read_sandbox_index_entries, read_lock as read_sandbox_lock,
    read_manifest as read_sandbox_manifest, remove_index_entry as remove_sandbox_index_entry,
    replace_index_entries as replace_sandbox_index_entries,
    sandbox_id_from_root as sandbox_id_from_root_meta,
    sandbox_index_exists as sandbox_index_exists_meta, unix_millis_now as unix_millis_now_meta,
    upsert_index_entry as upsert_sandbox_index_entry, write_lock as write_sandbox_lock,
    write_manifest as write_sandbox_manifest,
};

const SANDBOX_PREFIX: &str = "bmux-sbx-";
const PID_MARKER_FILE: &str = "sandbox.pid";
const DEFAULT_CLEANUP_MIN_AGE: Duration = Duration::from_secs(300);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const LOCK_FRESHNESS: Duration = Duration::from_secs(15);
const SANDBOX_JSON_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct CleanupEntry {
    path: String,
    source: String,
    age_secs: u64,
    status: String,
    reason: String,
    removed: bool,
}

#[derive(Debug, Clone, Copy)]
struct CleanupSkipped {
    source_mismatch: usize,
    running: usize,
    recent: usize,
    not_failed: usize,
    missing_manifest: usize,
    delete_failed: usize,
}

#[derive(Debug)]
struct CleanupScan {
    scanned: usize,
    entries: Vec<CleanupEntry>,
    skipped: CleanupSkipped,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct IndexRebuildReport {
    scanned: usize,
    rebuilt_count: usize,
    pruned_count: usize,
    missing_manifest: usize,
    scan_fallback_used: bool,
}

#[derive(Debug, Clone)]
struct SandboxCandidate {
    root: PathBuf,
    updated_at_unix_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize)]
struct SandboxListEntry {
    id: String,
    source: String,
    root: String,
    status: String,
    age_secs: u64,
    exit_code: Option<i32>,
    kept: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct StatusCounts {
    total: usize,
    running: usize,
    failed: usize,
    stopped: usize,
}

#[derive(Debug, Clone, Serialize)]
struct SourceStatusCounts {
    source: String,
    total: usize,
    running: usize,
    failed: usize,
    stopped: usize,
}

#[derive(Debug, Clone, Serialize)]
struct SandboxStatusHealth {
    stale_lock_count: usize,
    missing_manifest_count: usize,
    index_exists: bool,
    index_entries: usize,
    index_missing_roots: usize,
    index_read_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SandboxStatusSnapshot {
    totals: StatusCounts,
    by_source: Vec<SourceStatusCounts>,
    health: SandboxStatusHealth,
    reconcile: ReconcileReport,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ReconcileReport {
    healed_entries: usize,
    pruned_entries: usize,
    rebuilt_entries: usize,
    normalized_running: usize,
    cleared_stale_locks: usize,
    scan_fallback_used: bool,
    index_read_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct RecoveryReport {
    normalized_running: usize,
    cleared_stale_locks: usize,
}

#[derive(Debug, Clone)]
struct SandboxCandidateCollection {
    candidates: Vec<SandboxCandidate>,
    reconcile: ReconcileReport,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct TriageSelection {
    defaulted_to_latest_failed: bool,
    latest: bool,
    latest_failed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct TriageTarget {
    id: String,
    source: String,
    status: String,
    running: bool,
    root: String,
    log_dir: String,
    latest_log: Option<String>,
    repro: String,
    log_tail: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct TriageSnapshot {
    totals: StatusCounts,
    health: SandboxStatusHealth,
    reconcile: ReconcileReport,
    selection: TriageSelection,
    source_filter: Option<String>,
    target: TriageTarget,
}

#[derive(Debug, Clone, Serialize)]
struct TriageBundleSnapshot {
    requested: bool,
    executed: bool,
    bundle_dir: Option<String>,
    bundle_manifest: Option<String>,
    verify: Option<BundleVerifyReport>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheck {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorFixReport {
    applied: bool,
    dry_run: bool,
    target: Option<String>,
    scanned: usize,
    normalized_running: usize,
    cleared_stale_locks: usize,
    index_rebuilt: bool,
    rebuilt_count: usize,
    pruned_count: usize,
    missing_manifest: usize,
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

#[derive(Debug, Serialize)]
struct SandboxRunReport {
    schema_version: u32,
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

struct SandboxLockGuard {
    root_dir: PathBuf,
}

impl SandboxLockGuard {
    fn new(root_dir: &Path) -> Self {
        Self {
            root_dir: root_dir.to_path_buf(),
        }
    }
}

impl Drop for SandboxLockGuard {
    fn drop(&mut self) {
        clear_lock(&self.root_dir);
    }
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

pub(super) struct RerunSandboxOptions<'a> {
    pub(super) inspect: InspectTargetOptions<'a>,
    pub(super) run: RunSandboxOptions<'a>,
    pub(super) bmux_bin_override: Option<&'a str>,
    pub(super) env_mode_override: Option<SandboxEnvModeArg>,
}

pub(super) struct InspectTargetOptions<'a> {
    pub(super) target: Option<&'a str>,
    pub(super) latest: bool,
    pub(super) latest_failed: bool,
    pub(super) source_filter: Option<&'a str>,
}

pub(super) struct TriageSandboxOptions<'a> {
    pub(super) inspect: InspectTargetOptions<'a>,
    pub(super) tail: usize,
    pub(super) rerun: bool,
    pub(super) run: RunSandboxOptions<'a>,
    pub(super) bmux_bin_override: Option<&'a str>,
    pub(super) env_mode_override: Option<SandboxEnvModeArg>,
    pub(super) bundle: Option<TriageBundleOptions<'a>>,
    pub(super) json: bool,
}

pub(super) struct TriageBundleOptions<'a> {
    pub(super) output: Option<&'a str>,
    pub(super) strict_verify: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct BundleIncludeOptions {
    pub(super) env: bool,
    pub(super) index_state: bool,
    pub(super) doctor: bool,
}

pub(super) struct BundleSandboxOptions<'a> {
    pub(super) target: &'a str,
    pub(super) output: Option<&'a str>,
    pub(super) include: BundleIncludeOptions,
    pub(super) verify: bool,
    pub(super) json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BundleArtifactMetadata {
    path: String,
    kind: String,
    bytes: u64,
    file_count: usize,
    exists: bool,
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct BundleVerifyManifest {
    #[serde(default)]
    bundle_version: Option<u32>,
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    artifact_metadata: Vec<BundleArtifactMetadata>,
    #[serde(default)]
    artifacts: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct BundleVerifyIssue {
    path: String,
    field: String,
    expected: String,
    actual: String,
}

#[derive(Debug, Clone, Serialize)]
struct BundleVerifyReport {
    ok: bool,
    strict: bool,
    mode: String,
    bundle_dir: String,
    bundle_manifest: String,
    unexpected_artifacts: Vec<String>,
    version_check: BundleVersionCheck,
    checked_artifacts: usize,
    issue_count: usize,
    issues: Vec<BundleVerifyIssue>,
}

#[derive(Debug, Clone, Serialize)]
struct BundleVersionCheck {
    ok: bool,
    bundle_version: Option<u32>,
    schema_version: Option<u32>,
    reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct BundleCreateResult {
    bundle_dir: String,
    sandbox_root: String,
    sandbox_id: String,
    bundle_manifest: String,
    verify: Option<BundleVerifyReport>,
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
    let _ = upsert_sandbox_index_entry(&manifest);
    write_lock(&sandbox.root_dir)?;
    let lock_guard = SandboxLockGuard::new(&sandbox.root_dir);

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
            let _ = mark_manifest_aborted(&sandbox.root_dir, &mut manifest);
            let _ = upsert_sandbox_index_entry(&manifest);
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

    manifest.updated_at_unix_ms = unix_millis_now_meta();
    manifest.status = final_status.to_string();
    manifest.exit_code = Some(raw_exit_code);
    manifest.kept = kept;
    write_manifest(&sandbox.root_dir, &manifest)?;
    let _ = upsert_sandbox_index_entry(&manifest);

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
        drop(lock_guard);
        let _ = std::fs::remove_dir_all(&sandbox.root_dir);
        let _ = remove_sandbox_index_entry(&sandbox.root_dir);
    }

    Ok(exit_code_to_u8(raw_exit_code))
}

fn mark_manifest_aborted(root_dir: &Path, manifest: &mut SandboxManifest) -> Result<()> {
    manifest.updated_at_unix_ms = unix_millis_now_meta();
    manifest.status = "aborted".to_string();
    manifest.exit_code = Some(1);
    manifest.kept = true;
    write_manifest(root_dir, manifest)
}

fn build_manifest(
    sandbox: &SandboxPaths,
    binary: &Path,
    command_args: &[String],
    env_mode: SandboxEnvModeArg,
    keep: bool,
) -> SandboxManifest {
    SandboxManifest {
        id: sandbox_id_from_root_meta(&sandbox.root_dir),
        source: "sandbox-cli".to_string(),
        created_at_unix_ms: unix_millis_now_meta(),
        updated_at_unix_ms: unix_millis_now_meta(),
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
            schema_version: SANDBOX_JSON_SCHEMA_VERSION,
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
    source_filter: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<u8> {
    let collection = collect_sandbox_candidates();
    let mut entries = collection
        .candidates
        .into_iter()
        .map(|candidate| sandbox_list_entry(&candidate))
        .collect::<Vec<_>>();

    entries.sort_by(|left, right| left.age_secs.cmp(&right.age_secs));

    if let Some(filter) = status_filter {
        entries.retain(|entry| entry.status == filter);
    }
    if let Some(filter) = source_filter {
        entries.retain(|entry| entry.source == filter);
    }
    if limit > 0 {
        entries.truncate(limit);
    }

    if json {
        let payload = serde_json::json!({
            "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
            "reconcile": collection.reconcile,
            "sandboxes": entries,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if entries.is_empty() {
        println!("no sandboxes found");
    } else {
        println!(
            "ID                               SOURCE            STATUS   AGE   EXIT  KEPT  ROOT"
        );
        for entry in entries {
            println!(
                "{:<32} {:<17} {:<8} {:>4}s {:>5} {:>5}  {}",
                entry.id,
                entry.source,
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
        print_reconcile_report_text(&collection.reconcile);
    }

    Ok(0)
}

pub(super) fn run_sandbox_status(json: bool) -> Result<u8> {
    let snapshot = collect_sandbox_status_snapshot();

    if json {
        let payload = serde_json::json!({
            "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
            "totals": snapshot.totals,
            "by_source": snapshot.by_source,
            "health": snapshot.health,
            "reconcile": snapshot.reconcile,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print_sandbox_status_text(&snapshot);
        if let Some(error) = snapshot.health.index_read_error.as_deref() {
            println!("health: index_read_error={error}");
        }
    }

    Ok(0)
}

fn collect_sandbox_status_snapshot() -> SandboxStatusSnapshot {
    let collection = collect_sandbox_candidates();
    let entries = collection
        .candidates
        .iter()
        .map(sandbox_list_entry)
        .collect::<Vec<_>>();
    let totals = summarize_status_counts(&entries);
    let by_source = summarize_source_status_counts(&entries);
    let health = summarize_sandbox_health(&collection.candidates);

    SandboxStatusSnapshot {
        totals,
        by_source,
        health,
        reconcile: collection.reconcile,
    }
}

fn summarize_status_counts(entries: &[SandboxListEntry]) -> StatusCounts {
    entries.iter().fold(
        StatusCounts {
            total: 0,
            running: 0,
            failed: 0,
            stopped: 0,
        },
        |mut acc, entry| {
            acc.total += 1;
            match entry.status.as_str() {
                "running" => acc.running += 1,
                "failed" => acc.failed += 1,
                _ => acc.stopped += 1,
            }
            acc
        },
    )
}

fn summarize_source_status_counts(entries: &[SandboxListEntry]) -> Vec<SourceStatusCounts> {
    let mut by_source_map = std::collections::BTreeMap::<String, StatusCounts>::new();
    for entry in entries {
        let counts = by_source_map
            .entry(entry.source.clone())
            .or_insert(StatusCounts {
                total: 0,
                running: 0,
                failed: 0,
                stopped: 0,
            });
        counts.total += 1;
        match entry.status.as_str() {
            "running" => counts.running += 1,
            "failed" => counts.failed += 1,
            _ => counts.stopped += 1,
        }
    }

    by_source_map
        .into_iter()
        .map(|(source, counts)| SourceStatusCounts {
            source,
            total: counts.total,
            running: counts.running,
            failed: counts.failed,
            stopped: counts.stopped,
        })
        .collect::<Vec<_>>()
}

fn summarize_sandbox_health(candidates: &[SandboxCandidate]) -> SandboxStatusHealth {
    let stale_lock_count = candidates
        .iter()
        .filter(|candidate| sandbox_lock_is_stale(&candidate.root))
        .count();
    let missing_manifest_count = candidates
        .iter()
        .filter(|candidate| read_manifest(&candidate.root).is_err())
        .count();
    let index_exists = sandbox_index_exists_meta();
    let (index_entries, index_missing_roots, index_read_error) = match read_sandbox_index_entries()
    {
        Ok(index) => {
            let missing_roots = index
                .iter()
                .filter(|entry| !Path::new(&entry.root).is_dir())
                .count();
            (index.len(), missing_roots, None)
        }
        Err(error) => (0, 0, Some(error.to_string())),
    };

    SandboxStatusHealth {
        stale_lock_count,
        missing_manifest_count,
        index_exists,
        index_entries,
        index_missing_roots,
        index_read_error,
    }
}

fn print_sandbox_status_text(snapshot: &SandboxStatusSnapshot) {
    println!(
        "sandboxes: total={} running={} failed={} stopped={}",
        snapshot.totals.total,
        snapshot.totals.running,
        snapshot.totals.failed,
        snapshot.totals.stopped
    );
    if snapshot.by_source.is_empty() {
        println!("by source: (none)");
    } else {
        println!("by source:");
        for source in &snapshot.by_source {
            println!(
                "  {:<17} total={} running={} failed={} stopped={}",
                source.source, source.total, source.running, source.failed, source.stopped
            );
        }
    }
    println!(
        "health: stale_locks={} missing_manifest={} index_exists={} index_entries={} index_missing_roots={}",
        snapshot.health.stale_lock_count,
        snapshot.health.missing_manifest_count,
        snapshot.health.index_exists,
        snapshot.health.index_entries,
        snapshot.health.index_missing_roots
    );
    print_reconcile_report_text(&snapshot.reconcile);
}

fn print_reconcile_report_text(reconcile: &ReconcileReport) {
    if reconcile.healed_entries > 0 {
        println!(
            "reconcile: healed={} (rebuilt={}, pruned={}, normalized_running={}, cleared_stale_locks={}, fallback={})",
            reconcile.healed_entries,
            reconcile.rebuilt_entries,
            reconcile.pruned_entries,
            reconcile.normalized_running,
            reconcile.cleared_stale_locks,
            reconcile.scan_fallback_used
        );
    }
}

pub(super) fn run_sandbox_inspect(
    target: Option<&str>,
    latest: bool,
    latest_failed: bool,
    source_filter: Option<&str>,
    tail: usize,
    json: bool,
) -> Result<u8> {
    let collection = collect_sandbox_candidates();
    let root = resolve_inspect_target(
        "inspect",
        target,
        latest,
        latest_failed,
        source_filter,
        &collection.candidates,
    )?;
    let manifest = read_manifest(&root)?;
    let log_dir = root.join("logs");
    let latest_log = newest_regular_file(&log_dir);
    let log_tail = read_log_tail(&root, tail);
    let running = sandbox_process_alive(&root) || sandbox_socket_alive(&root);

    if json {
        let payload = serde_json::json!({
            "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
            "reconcile": collection.reconcile,
            "root": root.to_string_lossy().to_string(),
            "log_dir": log_dir.to_string_lossy().to_string(),
            "latest_log": latest_log.as_ref().map(|path| path.to_string_lossy().to_string()),
            "manifest": manifest,
            "running": running,
            "log_tail": log_tail,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("id: {}", manifest.id);
        println!("source: {}", manifest.source);
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
        println!("log_dir: {}", log_dir.display());
        println!(
            "latest_log: {}",
            latest_log
                .as_ref()
                .map_or_else(|| "(none)".to_string(), |path| path.display().to_string())
        );
        println!("bmux_bin: {}", manifest.bmux_bin);
        println!("command: {}", manifest.command.join(" "));
        println!("repro: {}", format_repro_command_from_manifest(&manifest));
        println!("logs:");
        for line in log_tail {
            println!("  {line}");
        }
        print_reconcile_report_text(&collection.reconcile);
    }

    Ok(0)
}

pub(super) fn run_sandbox_tail(
    target: Option<&str>,
    latest: bool,
    latest_failed: bool,
    source_filter: Option<&str>,
    tail: usize,
    json: bool,
) -> Result<u8> {
    let collection = collect_sandbox_candidates();
    let root = resolve_inspect_target(
        "tail",
        target,
        latest,
        latest_failed,
        source_filter,
        &collection.candidates,
    )?;
    let manifest = read_manifest(&root)?;
    let log_dir = root.join("logs");
    let latest_log = newest_regular_file(&log_dir);
    let log_tail = read_log_tail(&root, tail);

    if json {
        let payload = serde_json::json!({
            "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
            "reconcile": collection.reconcile,
            "id": manifest.id,
            "source": manifest.source,
            "status": manifest.status,
            "root": root.to_string_lossy().to_string(),
            "log_dir": log_dir.to_string_lossy().to_string(),
            "latest_log": latest_log.as_ref().map(|path| path.to_string_lossy().to_string()),
            "log_tail": log_tail,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("id: {}", manifest.id);
        println!("source: {}", manifest.source);
        println!("status: {}", manifest.status);
        println!("logs: {}", log_dir.display());
        println!(
            "latest_log: {}",
            latest_log
                .as_ref()
                .map_or_else(|| "(none)".to_string(), |path| path.display().to_string())
        );
        for line in log_tail {
            println!("{line}");
        }
    }

    Ok(0)
}

pub(super) fn run_sandbox_open(
    target: Option<&str>,
    latest: bool,
    latest_failed: bool,
    source_filter: Option<&str>,
    json: bool,
) -> Result<u8> {
    let collection = collect_sandbox_candidates();
    let root = resolve_inspect_target(
        "open",
        target,
        latest,
        latest_failed,
        source_filter,
        &collection.candidates,
    )?;
    let manifest = read_manifest(&root)?;
    let log_dir = root.join("logs");
    let latest_log = newest_regular_file(&log_dir);
    let repro = format_repro_command_from_manifest(&manifest);

    if json {
        let payload = serde_json::json!({
            "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
            "reconcile": collection.reconcile,
            "id": manifest.id,
            "source": manifest.source,
            "status": manifest.status,
            "root": root.to_string_lossy().to_string(),
            "log_dir": log_dir.to_string_lossy().to_string(),
            "latest_log": latest_log.as_ref().map(|path| path.to_string_lossy().to_string()),
            "repro": repro,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("id: {}", manifest.id);
        println!("source: {}", manifest.source);
        println!("status: {}", manifest.status);
        println!("root: {}", root.display());
        println!("logs: {}", log_dir.display());
        println!(
            "latest_log: {}",
            latest_log
                .as_ref()
                .map_or_else(|| "(none)".to_string(), |path| path.display().to_string())
        );
        println!("repro: {repro}");
    }

    Ok(0)
}

pub(super) async fn run_sandbox_triage(options: TriageSandboxOptions<'_>) -> Result<u8> {
    anyhow::ensure!(
        !(options.json && options.rerun),
        "sandbox triage --json cannot be combined with --rerun"
    );

    let collection = collect_sandbox_candidates();
    let selection = resolve_triage_selector(&options);

    let root = resolve_inspect_target(
        "triage",
        options.inspect.target,
        selection.latest,
        selection.latest_failed,
        options.inspect.source_filter,
        &collection.candidates,
    )?;
    let manifest = read_manifest(&root)?;
    let snapshot = build_triage_snapshot(&options, &collection, selection, &root, &manifest);
    let (bundle_snapshot, bundle_exit_code) =
        triage_bundle_snapshot(&options, &root, &manifest, &snapshot)?;

    if options.json {
        let payload = triage_json_payload(&snapshot, &bundle_snapshot);
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(bundle_exit_code);
    }

    print_triage_text(&snapshot);
    print_triage_bundle_text(&bundle_snapshot);

    if options.rerun {
        println!("rerun: executing manifest command");
        let target = snapshot.target.root.clone();
        let rerun_code = run_sandbox_rerun(RerunSandboxOptions {
            inspect: InspectTargetOptions {
                target: Some(target.as_str()),
                latest: false,
                latest_failed: false,
                source_filter: None,
            },
            run: RunSandboxOptions {
                bmux_bin: None,
                env_mode: SandboxEnvModeArg::Inherit,
                keep: options.run.keep,
                json: false,
                print_env: options.run.print_env,
                timeout_secs: options.run.timeout_secs,
                name: options.run.name,
            },
            bmux_bin_override: options.bmux_bin_override,
            env_mode_override: options.env_mode_override,
        })
        .await?;
        println!("rerun_exit_code: {rerun_code}");
        return Ok(std::cmp::max(bundle_exit_code, rerun_code));
    }

    Ok(bundle_exit_code)
}

fn build_triage_snapshot(
    options: &TriageSandboxOptions<'_>,
    collection: &SandboxCandidateCollection,
    selection: TriageSelection,
    root: &Path,
    manifest: &SandboxManifest,
) -> TriageSnapshot {
    let running = sandbox_process_alive(root) || sandbox_socket_alive(root);
    let log_dir = root.join("logs");
    let latest_log = newest_regular_file(&log_dir);
    let log_tail = read_log_tail(root, options.tail);
    let repro = format_repro_command_from_manifest(manifest);

    let entries = collection
        .candidates
        .iter()
        .map(sandbox_list_entry)
        .collect::<Vec<_>>();

    TriageSnapshot {
        totals: summarize_status_counts(&entries),
        health: summarize_sandbox_health(&collection.candidates),
        reconcile: collection.reconcile.clone(),
        selection,
        source_filter: options.inspect.source_filter.map(ToOwned::to_owned),
        target: TriageTarget {
            id: manifest.id.clone(),
            source: manifest.source.clone(),
            status: manifest.status.clone(),
            running,
            root: root.to_string_lossy().to_string(),
            log_dir: log_dir.to_string_lossy().to_string(),
            latest_log: latest_log
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            repro,
            log_tail,
        },
    }
}

fn triage_bundle_snapshot(
    options: &TriageSandboxOptions<'_>,
    root: &Path,
    manifest: &SandboxManifest,
    snapshot: &TriageSnapshot,
) -> Result<(TriageBundleSnapshot, u8)> {
    let mut bundle_snapshot = TriageBundleSnapshot {
        requested: options.bundle.is_some(),
        executed: false,
        bundle_dir: None,
        bundle_manifest: None,
        verify: None,
    };

    let bundle_exit_code = if let Some(bundle_options) = &options.bundle {
        let created = create_bundle_from_manifest(
            root,
            manifest,
            &snapshot.target.root,
            bundle_options.output,
            BundleIncludeOptions {
                env: false,
                index_state: false,
                doctor: false,
            },
            true,
            bundle_options.strict_verify,
        )?;
        bundle_snapshot.executed = true;
        bundle_snapshot.bundle_dir = Some(created.bundle_dir);
        bundle_snapshot.bundle_manifest = Some(created.bundle_manifest);
        bundle_snapshot.verify = created.verify;
        u8::from(
            bundle_snapshot
                .verify
                .as_ref()
                .is_some_and(|report| !report.ok),
        )
    } else {
        0
    };

    Ok((bundle_snapshot, bundle_exit_code))
}

fn print_triage_bundle_text(bundle: &TriageBundleSnapshot) {
    if !bundle.executed {
        return;
    }

    println!(
        "bundle: {}",
        bundle.bundle_dir.as_deref().unwrap_or("(none)")
    );
    if let Some(verify) = &bundle.verify {
        print_bundle_verify_text(verify);
    }
}

const fn resolve_triage_selector(options: &TriageSandboxOptions<'_>) -> TriageSelection {
    let defaulted = options.inspect.target.is_none()
        && !options.inspect.latest
        && !options.inspect.latest_failed;
    if defaulted {
        TriageSelection {
            defaulted_to_latest_failed: true,
            latest: false,
            latest_failed: true,
        }
    } else {
        TriageSelection {
            defaulted_to_latest_failed: false,
            latest: options.inspect.latest,
            latest_failed: options.inspect.latest_failed,
        }
    }
}

fn triage_json_payload(
    snapshot: &TriageSnapshot,
    bundle: &TriageBundleSnapshot,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
        "totals": snapshot.totals,
        "health": snapshot.health,
        "reconcile": snapshot.reconcile,
        "selection": {
            "defaulted_to_latest_failed": snapshot.selection.defaulted_to_latest_failed,
            "latest": snapshot.selection.latest,
            "latest_failed": snapshot.selection.latest_failed,
            "source_filter": snapshot.source_filter,
        },
        "target": snapshot.target,
        "bundle": bundle,
        "rerun": {
            "requested": false,
            "executed": false,
        },
    })
}

fn print_triage_text(snapshot: &TriageSnapshot) {
    println!(
        "sandboxes: total={} running={} failed={} stopped={}",
        snapshot.totals.total,
        snapshot.totals.running,
        snapshot.totals.failed,
        snapshot.totals.stopped
    );
    println!(
        "triage_target: {} source={} status={} running={}",
        snapshot.target.id, snapshot.target.source, snapshot.target.status, snapshot.target.running
    );
    if snapshot.selection.defaulted_to_latest_failed {
        println!("selection: defaulted to --latest-failed");
    }
    println!("root: {}", snapshot.target.root);
    println!("log_dir: {}", snapshot.target.log_dir);
    println!(
        "latest_log: {}",
        snapshot.target.latest_log.as_deref().unwrap_or("(none)")
    );
    println!("repro: {}", snapshot.target.repro);
    println!("logs:");
    for line in &snapshot.target.log_tail {
        println!("  {line}");
    }
    print_reconcile_report_text(&snapshot.reconcile);
}

pub(super) async fn run_sandbox_rerun(options: RerunSandboxOptions<'_>) -> Result<u8> {
    let collection = collect_sandbox_candidates();
    let root = resolve_inspect_target(
        "rerun",
        options.inspect.target,
        options.inspect.latest,
        options.inspect.latest_failed,
        options.inspect.source_filter,
        &collection.candidates,
    )?;
    let manifest = read_manifest(&root)?;
    anyhow::ensure!(
        !manifest.command.is_empty(),
        "sandbox manifest '{}' has no command to rerun",
        manifest.id
    );

    let resolved_env_mode = options
        .env_mode_override
        .unwrap_or_else(|| manifest_env_mode(&manifest));
    let bmux_bin_owned = options
        .bmux_bin_override
        .map_or_else(|| manifest.bmux_bin.clone(), ToOwned::to_owned);

    run_sandbox_run(
        RunSandboxOptions {
            bmux_bin: Some(bmux_bin_owned.as_str()),
            env_mode: resolved_env_mode,
            keep: options.run.keep,
            json: options.run.json,
            print_env: options.run.print_env,
            timeout_secs: options.run.timeout_secs,
            name: options.run.name,
        },
        &manifest.command,
    )
    .await
}

fn manifest_env_mode(manifest: &SandboxManifest) -> SandboxEnvModeArg {
    match manifest.env_mode.as_str() {
        "clean" => SandboxEnvModeArg::Clean,
        "hermetic" => SandboxEnvModeArg::Hermetic,
        _ => SandboxEnvModeArg::Inherit,
    }
}

fn resolve_inspect_target(
    command: &str,
    target: Option<&str>,
    latest: bool,
    latest_failed: bool,
    source_filter: Option<&str>,
    candidates: &[SandboxCandidate],
) -> Result<PathBuf> {
    if let Some(target) = target {
        return resolve_sandbox_target(target);
    }

    if latest || latest_failed {
        return resolve_latest_sandbox(latest_failed, source_filter, candidates);
    }

    anyhow::bail!("{command} target required (provide <id|path>, --latest, or --latest-failed)")
}

fn resolve_latest_sandbox(
    failed_only: bool,
    source_filter: Option<&str>,
    candidates: &[SandboxCandidate],
) -> Result<PathBuf> {
    let mut sorted = candidates.to_vec();
    sorted.sort_by_key(|candidate| std::cmp::Reverse(candidate_sort_key(candidate)));

    let mut source_total = 0usize;

    for candidate in sorted {
        let path = candidate.root;
        if let Some(source) = source_filter {
            if sandbox_source_for_dir(&path) != source {
                continue;
            }
            source_total += 1;
        }
        if !failed_only || matches!(sandbox_status_for_dir(&path), "failed") {
            return Ok(path);
        }
    }

    if let Some(source) = source_filter {
        let known_sources = format_known_sources(candidates);
        if source_total == 0 {
            if known_sources.is_empty() {
                anyhow::bail!("no sandboxes found for source {source}");
            }
            anyhow::bail!(
                "no sandboxes found for source {source}; available sources: {known_sources}"
            );
        }
        if failed_only {
            anyhow::bail!(
                "no failed sandboxes found for source {source}; try --latest --source {source}"
            );
        }
        anyhow::bail!("no sandboxes found for source {source}");
    }

    if failed_only {
        anyhow::bail!("no failed sandboxes found");
    }

    anyhow::bail!("no sandboxes found")
}

pub(super) fn run_sandbox_doctor(
    id: Option<&str>,
    fix: bool,
    dry_run: bool,
    json: bool,
) -> Result<u8> {
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

    let fix_report = if fix {
        Some(run_sandbox_doctor_fix(id, dry_run)?)
    } else {
        None
    };

    let ok = checks.iter().all(|check| check.ok);
    if json {
        let payload = serde_json::json!({
            "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
            "ok": ok,
            "checks": checks,
            "fix": fix_report,
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
        if let Some(report) = fix_report {
            println!(
                "doctor fix: applied={} dry_run={} scanned={} normalized_running={} cleared_stale_locks={} index_rebuilt={} rebuilt={} pruned={} missing_manifest={}",
                report.applied,
                report.dry_run,
                report.scanned,
                report.normalized_running,
                report.cleared_stale_locks,
                report.index_rebuilt,
                report.rebuilt_count,
                report.pruned_count,
                report.missing_manifest
            );
        }
    }

    Ok(u8::from(!ok))
}

fn run_sandbox_doctor_fix(target: Option<&str>, dry_run: bool) -> Result<DoctorFixReport> {
    let (roots, target_value) = if let Some(target) = target {
        (
            vec![resolve_sandbox_target(target)?],
            Some(target.to_string()),
        )
    } else {
        (collect_sandbox_directories(), None)
    };

    let candidates = roots
        .iter()
        .cloned()
        .map(|root| SandboxCandidate {
            root,
            updated_at_unix_ms: None,
        })
        .collect::<Vec<_>>();

    let recovery = if dry_run {
        preview_candidate_lifecycle_recovery(&candidates)
    } else {
        recover_candidate_lifecycle_state(&candidates)
    };

    let mut report = DoctorFixReport {
        applied: !dry_run,
        dry_run,
        target: target_value,
        scanned: roots.len(),
        normalized_running: recovery.normalized_running,
        cleared_stale_locks: recovery.cleared_stale_locks,
        index_rebuilt: false,
        rebuilt_count: 0,
        pruned_count: 0,
        missing_manifest: 0,
    };

    if target.is_none() {
        let rebuild = if dry_run {
            preview_rebuild_sandbox_index_from_roots(&roots)
        } else {
            rebuild_sandbox_index_from_roots(&roots, !sandbox_index_exists_meta())
        };
        report.index_rebuilt = true;
        report.rebuilt_count = rebuild.rebuilt_count;
        report.pruned_count = rebuild.pruned_count;
        report.missing_manifest = rebuild.missing_manifest;
    } else if !dry_run {
        for root in &roots {
            if let Ok(manifest) = read_manifest(root) {
                let _ = upsert_sandbox_index_entry(&manifest);
            }
        }
    }

    Ok(report)
}

fn preview_candidate_lifecycle_recovery(candidates: &[SandboxCandidate]) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    for candidate in candidates {
        let root = &candidate.root;
        if sandbox_lock_is_fresh(root) || sandbox_process_alive(root) || sandbox_socket_alive(root)
        {
            continue;
        }

        if let Ok(manifest) = read_manifest(root)
            && manifest.status == "running"
        {
            report.normalized_running += 1;
        }

        if sandbox_lock_is_stale(root) {
            report.cleared_stale_locks += 1;
        }
    }
    report
}

pub(super) fn run_sandbox_bundle(options: &BundleSandboxOptions<'_>) -> Result<u8> {
    let root = resolve_sandbox_target(options.target)?;
    let manifest = read_manifest(&root)?;

    let created = create_bundle_from_manifest(
        &root,
        &manifest,
        options.target,
        options.output,
        options.include,
        options.verify,
        false,
    )?;
    let exit_code = u8::from(created.verify.as_ref().is_some_and(|report| !report.ok));

    if options.json {
        let payload = serde_json::json!({
            "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
            "bundle_dir": created.bundle_dir,
            "sandbox_root": created.sandbox_root,
            "sandbox_id": created.sandbox_id,
            "bundle_manifest": created.bundle_manifest,
            "verify": created.verify,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("sandbox bundle created: {}", created.bundle_dir);
        if let Some(report) = &created.verify {
            print_bundle_verify_text(report);
        }
    }

    Ok(exit_code)
}

fn create_bundle_from_manifest(
    root: &Path,
    manifest: &SandboxManifest,
    doctor_target: &str,
    output: Option<&str>,
    include: BundleIncludeOptions,
    verify: bool,
    verify_strict: bool,
) -> Result<BundleCreateResult> {
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

    let bundle_dir = output_root.join(format!("{}-{}", manifest.id, unix_millis_now_meta()));
    std::fs::create_dir_all(&bundle_dir)
        .with_context(|| format!("failed creating {}", bundle_dir.display()))?;

    let mut artifacts = write_bundle_core_artifacts(root, manifest, &bundle_dir)?;
    let index_status = write_bundle_optional_artifacts(
        manifest,
        doctor_target,
        &bundle_dir,
        include,
        &mut artifacts,
    )?;
    let artifact_metadata = bundle_artifact_metadata(&bundle_dir, &artifacts);

    let bundle_manifest_path = bundle_dir.join("bundle_manifest.json");
    let bundle_manifest_payload = serde_json::json!({
        "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
        "bundle_version": 1,
        "created_at_unix_ms": unix_millis_now_meta(),
        "sandbox": {
            "id": manifest.id,
            "source": manifest.source,
            "status": manifest.status,
            "root": root.to_string_lossy().to_string(),
        },
        "includes": {
            "env": include.env,
            "index_state": include.index_state,
            "doctor": include.doctor,
        },
        "index_state": {
            "status": index_status,
        },
        "artifacts": artifacts,
        "artifact_metadata": artifact_metadata,
    });
    std::fs::write(
        &bundle_manifest_path,
        serde_json::to_vec_pretty(&bundle_manifest_payload)?,
    )
    .with_context(|| format!("failed writing {}", bundle_manifest_path.display()))?;

    let verification_report = if verify {
        Some(load_bundle_verify_report(&bundle_dir, verify_strict)?)
    } else {
        None
    };

    Ok(BundleCreateResult {
        bundle_dir: bundle_dir.to_string_lossy().to_string(),
        sandbox_root: root.to_string_lossy().to_string(),
        sandbox_id: manifest.id.clone(),
        bundle_manifest: bundle_manifest_path.to_string_lossy().to_string(),
        verify: verification_report,
    })
}

pub(super) fn run_sandbox_verify_bundle(bundle_dir: &str, strict: bool, json: bool) -> Result<u8> {
    let report = load_bundle_verify_report(Path::new(bundle_dir), strict)?;
    if json {
        let payload = serde_json::json!({
            "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
            "ok": report.ok,
            "strict": report.strict,
            "mode": report.mode,
            "bundle_dir": report.bundle_dir,
            "bundle_manifest": report.bundle_manifest,
            "unexpected_artifacts": report.unexpected_artifacts,
            "version_check": report.version_check,
            "checked_artifacts": report.checked_artifacts,
            "issue_count": report.issue_count,
            "issues": report.issues,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print_bundle_verify_text(&report);
    }

    Ok(u8::from(!report.ok))
}

fn load_bundle_verify_report(bundle_root: &Path, strict: bool) -> Result<BundleVerifyReport> {
    anyhow::ensure!(
        bundle_root.is_dir(),
        "sandbox bundle directory not found: {}",
        bundle_root.display()
    );

    let bundle_manifest_path = bundle_root.join("bundle_manifest.json");
    let manifest_bytes = std::fs::read(&bundle_manifest_path)
        .with_context(|| format!("failed reading {}", bundle_manifest_path.display()))?;
    let manifest: BundleVerifyManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("failed parsing {}", bundle_manifest_path.display()))?;

    let version_check = validate_bundle_version(&manifest);

    let (mode, issues, checked_artifacts) = if manifest.artifact_metadata.is_empty() {
        (
            "existence_only".to_string(),
            verify_bundle_artifacts_existence(bundle_root, &manifest.artifacts),
            manifest.artifacts.len(),
        )
    } else {
        (
            "strict_metadata".to_string(),
            verify_bundle_artifacts_metadata(bundle_root, &manifest.artifact_metadata),
            manifest.artifact_metadata.len(),
        )
    };

    let expected_artifacts = expected_bundle_artifact_paths(&manifest);
    let unexpected_artifacts = bundle_unexpected_artifacts(bundle_root, &expected_artifacts)?;

    let mut issues = issues;
    if strict {
        for artifact in &unexpected_artifacts {
            issues.push(BundleVerifyIssue {
                path: artifact.clone(),
                field: "unexpected_artifact".to_string(),
                expected: "absent".to_string(),
                actual: "present".to_string(),
            });
        }
    }

    if !version_check.ok {
        issues.push(BundleVerifyIssue {
            path: "bundle_manifest.json".to_string(),
            field: "version_check".to_string(),
            expected: "supported".to_string(),
            actual: version_check
                .reason
                .clone()
                .unwrap_or_else(|| "unsupported version".to_string()),
        });
    }

    let issue_count = issues.len();
    let ok = issue_count == 0;
    Ok(BundleVerifyReport {
        ok,
        strict,
        mode,
        bundle_dir: bundle_root.to_string_lossy().to_string(),
        bundle_manifest: bundle_manifest_path.to_string_lossy().to_string(),
        unexpected_artifacts,
        version_check,
        checked_artifacts,
        issue_count,
        issues,
    })
}

fn validate_bundle_version(manifest: &BundleVerifyManifest) -> BundleVersionCheck {
    let bundle_version = manifest.bundle_version;
    let schema_version = manifest.schema_version;

    if bundle_version != Some(1) {
        return BundleVersionCheck {
            ok: false,
            bundle_version,
            schema_version,
            reason: Some(format!(
                "unsupported bundle_version={}; expected 1",
                bundle_version.map_or_else(|| "missing".to_string(), |value| value.to_string())
            )),
        };
    }

    if schema_version != Some(SANDBOX_JSON_SCHEMA_VERSION) {
        return BundleVersionCheck {
            ok: false,
            bundle_version,
            schema_version,
            reason: Some(format!(
                "unsupported schema_version={}; expected {}",
                schema_version.map_or_else(|| "missing".to_string(), |value| value.to_string()),
                SANDBOX_JSON_SCHEMA_VERSION
            )),
        };
    }

    BundleVersionCheck {
        ok: true,
        bundle_version,
        schema_version,
        reason: None,
    }
}

fn expected_bundle_artifact_paths(manifest: &BundleVerifyManifest) -> Vec<String> {
    if manifest.artifact_metadata.is_empty() {
        return manifest.artifacts.clone();
    }

    manifest
        .artifact_metadata
        .iter()
        .map(|entry| entry.path.clone())
        .collect()
}

fn bundle_unexpected_artifacts(bundle_root: &Path, expected: &[String]) -> Result<Vec<String>> {
    let mut expected_set = std::collections::BTreeSet::new();
    expected_set.insert("bundle_manifest.json".to_string());
    for artifact in expected {
        expected_set.insert(artifact.clone());
    }

    let mut unexpected = Vec::new();
    for entry in std::fs::read_dir(bundle_root)
        .with_context(|| format!("failed reading {}", bundle_root.display()))?
    {
        let entry = entry.with_context(|| format!("failed reading {}", bundle_root.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed reading type for {}", entry.path().display()))?;

        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };

        let normalized = if file_type.is_dir() {
            format!("{name}/")
        } else {
            name
        };

        if !expected_set.contains(&normalized) {
            unexpected.push(normalized);
        }
    }

    unexpected.sort();
    Ok(unexpected)
}

fn print_bundle_verify_text(report: &BundleVerifyReport) {
    println!(
        "bundle verify: {}",
        if report.ok { "ok" } else { "drift detected" }
    );
    println!("mode: {}", report.mode);
    println!("strict: {}", report.strict);
    println!("checked_artifacts: {}", report.checked_artifacts);
    if !report.unexpected_artifacts.is_empty() {
        println!("unexpected_artifacts:");
        for artifact in &report.unexpected_artifacts {
            println!("  {artifact}");
        }
    }
    if !report.version_check.ok {
        println!(
            "version_check: unsupported ({})",
            report
                .version_check
                .reason
                .as_deref()
                .unwrap_or("unknown reason")
        );
    }
    if !report.issues.is_empty() {
        println!("issues:");
        for issue in &report.issues {
            println!(
                "  path={} field={} expected={} actual={}",
                issue.path, issue.field, issue.expected, issue.actual
            );
        }
    }
}

fn verify_bundle_artifacts_metadata(
    bundle_root: &Path,
    expected: &[BundleArtifactMetadata],
) -> Vec<BundleVerifyIssue> {
    let mut issues = Vec::new();
    for expected_entry in expected {
        let actual = bundle_artifact_metadata_for_expected(bundle_root, expected_entry);
        if expected_entry.kind != actual.kind {
            issues.push(BundleVerifyIssue {
                path: expected_entry.path.clone(),
                field: "kind".to_string(),
                expected: expected_entry.kind.clone(),
                actual: actual.kind,
            });
        }
        if expected_entry.exists != actual.exists {
            issues.push(BundleVerifyIssue {
                path: expected_entry.path.clone(),
                field: "exists".to_string(),
                expected: expected_entry.exists.to_string(),
                actual: actual.exists.to_string(),
            });
        }
        if expected_entry.bytes != actual.bytes {
            issues.push(BundleVerifyIssue {
                path: expected_entry.path.clone(),
                field: "bytes".to_string(),
                expected: expected_entry.bytes.to_string(),
                actual: actual.bytes.to_string(),
            });
        }
        if expected_entry.file_count != actual.file_count {
            issues.push(BundleVerifyIssue {
                path: expected_entry.path.clone(),
                field: "file_count".to_string(),
                expected: expected_entry.file_count.to_string(),
                actual: actual.file_count.to_string(),
            });
        }
        if let Some(expected_hash) = expected_entry.sha256.as_ref()
            && actual.sha256.as_deref() != Some(expected_hash.as_str())
        {
            issues.push(BundleVerifyIssue {
                path: expected_entry.path.clone(),
                field: "sha256".to_string(),
                expected: expected_hash.clone(),
                actual: actual.sha256.unwrap_or_else(|| "missing".to_string()),
            });
        }
    }

    issues
}

fn verify_bundle_artifacts_existence(
    bundle_root: &Path,
    artifacts: &[String],
) -> Vec<BundleVerifyIssue> {
    let mut issues = Vec::new();
    for artifact in artifacts {
        let expected_kind = if artifact.ends_with('/') {
            "directory"
        } else {
            "file"
        };
        let expected = BundleArtifactMetadata {
            path: artifact.clone(),
            kind: expected_kind.to_string(),
            bytes: 0,
            file_count: 0,
            exists: true,
            sha256: None,
        };
        let actual = bundle_artifact_metadata_for_expected(bundle_root, &expected);
        if !actual.exists {
            issues.push(BundleVerifyIssue {
                path: artifact.clone(),
                field: "exists".to_string(),
                expected: "true".to_string(),
                actual: "false".to_string(),
            });
        }
        if actual.kind != expected_kind {
            issues.push(BundleVerifyIssue {
                path: artifact.clone(),
                field: "kind".to_string(),
                expected: expected_kind.to_string(),
                actual: actual.kind,
            });
        }
    }

    issues
}

fn bundle_artifact_metadata_for_expected(
    bundle_root: &Path,
    expected: &BundleArtifactMetadata,
) -> BundleArtifactMetadata {
    let relative = expected.path.trim_end_matches('/');
    let path = bundle_root.join(relative);
    if expected.kind == "directory" {
        let (bytes, file_count, exists) = directory_stats(&path);
        let sha256 = if expected.sha256.is_some() {
            directory_sha256_hex(&path)
        } else {
            None
        };
        return BundleArtifactMetadata {
            path: expected.path.clone(),
            kind: "directory".to_string(),
            bytes,
            file_count,
            exists,
            sha256,
        };
    }

    let (bytes, exists) = file_bytes(&path);
    let sha256 = if expected.sha256.is_some() {
        file_sha256_hex(&path)
    } else {
        None
    };
    BundleArtifactMetadata {
        path: expected.path.clone(),
        kind: "file".to_string(),
        bytes,
        file_count: usize::from(exists),
        exists,
        sha256,
    }
}

fn write_bundle_core_artifacts(
    root: &Path,
    manifest: &SandboxManifest,
    bundle_dir: &Path,
) -> Result<Vec<String>> {
    copy_if_exists(&root.join(MANIFEST_FILE), &bundle_dir.join(MANIFEST_FILE))?;
    copy_if_exists(
        &root.join(PID_MARKER_FILE),
        &bundle_dir.join(PID_MARKER_FILE),
    )?;
    copy_if_exists(&root.join(LOCK_FILE), &bundle_dir.join(LOCK_FILE))?;

    let mut artifacts = vec![
        MANIFEST_FILE.to_string(),
        PID_MARKER_FILE.to_string(),
        LOCK_FILE.to_string(),
    ];

    let logs_src = root.join("logs");
    if logs_src.exists() {
        copy_directory_recursive(&logs_src, &bundle_dir.join("logs"))?;
        artifacts.push("logs/".to_string());
    }

    let repro_path = bundle_dir.join("repro.txt");
    std::fs::write(&repro_path, format_repro_command_from_manifest(manifest))
        .with_context(|| format!("failed writing {}", repro_path.display()))?;
    artifacts.push("repro.txt".to_string());

    Ok(artifacts)
}

fn write_bundle_optional_artifacts(
    manifest: &SandboxManifest,
    target: &str,
    bundle_dir: &Path,
    include: BundleIncludeOptions,
    artifacts: &mut Vec<String>,
) -> Result<String> {
    let mut index_status = "not_included".to_string();

    if include.env {
        let env_path = bundle_dir.join("env.json");
        let payload = bundle_env_payload(manifest);
        std::fs::write(&env_path, serde_json::to_vec_pretty(&payload)?)
            .with_context(|| format!("failed writing {}", env_path.display()))?;
        artifacts.push("env.json".to_string());
    }

    if include.index_state {
        index_status = write_bundle_index_state(manifest, bundle_dir, artifacts)?;
    }

    if include.doctor {
        let doctor_path = bundle_dir.join("doctor.json");
        let doctor_payload = bundle_doctor_payload(target);
        std::fs::write(&doctor_path, serde_json::to_vec_pretty(&doctor_payload)?)
            .with_context(|| format!("failed writing {}", doctor_path.display()))?;
        artifacts.push("doctor.json".to_string());
    }

    Ok(index_status)
}

fn bundle_artifact_metadata(
    bundle_dir: &Path,
    artifacts: &[String],
) -> Vec<BundleArtifactMetadata> {
    artifacts
        .iter()
        .map(|artifact| {
            let is_dir = artifact.ends_with('/');
            let relative = artifact.trim_end_matches('/');
            let path = bundle_dir.join(relative);

            if is_dir {
                let (bytes, file_count, exists) = directory_stats(&path);
                return BundleArtifactMetadata {
                    path: artifact.clone(),
                    kind: "directory".to_string(),
                    bytes,
                    file_count,
                    exists,
                    sha256: directory_sha256_hex(&path),
                };
            }

            let (bytes, exists) = file_bytes(&path);
            BundleArtifactMetadata {
                path: artifact.clone(),
                kind: "file".to_string(),
                bytes,
                file_count: usize::from(exists),
                exists,
                sha256: file_sha256_hex(&path),
            }
        })
        .collect()
}

fn file_sha256_hex(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer).ok()?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Some(hex_encode_digest(hasher.finalize()))
}

fn directory_sha256_hex(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return None;
    }

    let mut entries = Vec::new();
    collect_directory_hash_entries(path, path, &mut entries).ok()?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = Sha256::new();
    for (relative, digest) in entries {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update(digest.as_bytes());
        hasher.update([b'\n']);
    }

    Some(hex_encode_digest(hasher.finalize()))
}

fn collect_directory_hash_entries(
    root: &Path,
    current: &Path,
    entries: &mut Vec<(String, String)>,
) -> Result<()> {
    for entry in std::fs::read_dir(current)
        .with_context(|| format!("failed reading {}", current.display()))?
    {
        let entry = entry.with_context(|| format!("failed reading {}", current.display()))?;
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed reading type for {}", entry_path.display()))?;

        if file_type.is_dir() {
            collect_directory_hash_entries(root, &entry_path, entries)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let digest = file_sha256_hex(&entry_path)
            .ok_or_else(|| anyhow::anyhow!("failed hashing {}", entry_path.display()))?;
        let relative = entry_path
            .strip_prefix(root)
            .map_or_else(|_| entry_path.clone(), PathBuf::from)
            .to_string_lossy()
            .replace('\\', "/");
        entries.push((relative, digest));
    }

    Ok(())
}

fn hex_encode_digest(digest: impl AsRef<[u8]>) -> String {
    let bytes = digest.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn file_bytes(path: &Path) -> (u64, bool) {
    path.metadata().map_or((0, false), |metadata| {
        if metadata.is_file() {
            (metadata.len(), true)
        } else {
            (0, false)
        }
    })
}

fn directory_stats(path: &Path) -> (u64, usize, bool) {
    if !path.is_dir() {
        return (0, 0, false);
    }

    let Ok(entries) = std::fs::read_dir(path) else {
        return (0, 0, false);
    };

    let mut bytes = 0u64;
    let mut file_count = 0usize;
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            let (dir_bytes, dir_files, dir_exists) = directory_stats(&entry_path);
            if dir_exists {
                bytes = bytes.saturating_add(dir_bytes);
                file_count += dir_files;
            }
            continue;
        }

        if let Ok(metadata) = entry.metadata()
            && metadata.is_file()
        {
            bytes = bytes.saturating_add(metadata.len());
            file_count += 1;
        }
    }

    (bytes, file_count, true)
}

fn write_bundle_index_state(
    manifest: &SandboxManifest,
    bundle_dir: &Path,
    artifacts: &mut Vec<String>,
) -> Result<String> {
    let index_path = ConfigPaths::default()
        .state_dir()
        .join("sandbox")
        .join("index.json");
    let index_bundle_path = bundle_dir.join("sandbox-index.json");
    let status = if index_path.exists() {
        std::fs::copy(&index_path, &index_bundle_path)
            .with_context(|| format!("failed copying {}", index_path.display()))?;
        artifacts.push("sandbox-index.json".to_string());
        "copied".to_string()
    } else {
        "missing".to_string()
    };

    let index_entry_path = bundle_dir.join("sandbox-index-entry.json");
    let index_entry_payload = match read_sandbox_index_entries() {
        Ok(entries) => {
            let entry = entries.iter().find(|entry| entry.id == manifest.id);
            serde_json::json!({ "entry": entry })
        }
        Err(error) => serde_json::json!({ "index_read_error": error.to_string() }),
    };
    std::fs::write(
        &index_entry_path,
        serde_json::to_vec_pretty(&index_entry_payload)?,
    )
    .with_context(|| format!("failed writing {}", index_entry_path.display()))?;
    artifacts.push("sandbox-index-entry.json".to_string());

    Ok(status)
}

fn bundle_env_payload(manifest: &SandboxManifest) -> serde_json::Value {
    let root = PathBuf::from(&manifest.paths.root);
    serde_json::json!({
        "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
        "env_mode": manifest.env_mode,
        "bmux_bin": manifest.bmux_bin,
        "command": manifest.command,
        "paths": {
            "root": manifest.paths.root,
            "logs": manifest.paths.logs,
            "runtime": manifest.paths.runtime,
            "state": manifest.paths.state,
            "config_home": root.join("config").to_string_lossy().to_string(),
            "data_home": root.join("data").to_string_lossy().to_string(),
            "home": root.join("home").to_string_lossy().to_string(),
            "tmp": root.join("tmp").to_string_lossy().to_string(),
        }
    })
}

fn bundle_doctor_payload(target: &str) -> serde_json::Value {
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

    append_target_doctor_checks(&mut checks, target);
    let ok = checks.iter().all(|check| check.ok);

    serde_json::json!({
        "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
        "ok": ok,
        "checks": checks,
    })
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
    source_filter: Option<&str>,
    json: bool,
) -> Result<u8> {
    let older_than = older_than_secs.map_or(DEFAULT_CLEANUP_MIN_AGE, Duration::from_secs);
    let collection = collect_sandbox_candidates();
    let scan = cleanup_orphaned_sandboxes(
        collection.candidates,
        dry_run,
        failed_only,
        older_than,
        source_filter,
    );
    let orphaned = scan
        .entries
        .iter()
        .filter(|entry| cleanup_reason_is_orphaned(&entry.reason))
        .count();

    if json {
        let report = serde_json::json!({
            "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
            "scanned": scan.scanned,
            "orphaned": orphaned,
            "dry_run": dry_run,
            "failed_only": failed_only,
            "older_than_secs": older_than.as_secs(),
            "source": source_filter,
            "reconcile": collection.reconcile,
            "skipped_source_mismatch": scan.skipped.source_mismatch,
            "skipped_running": scan.skipped.running,
            "skipped_recent": scan.skipped.recent,
            "skipped_not_failed": scan.skipped.not_failed,
            "skipped_missing_manifest": scan.skipped.missing_manifest,
            "delete_failed": scan.skipped.delete_failed,
            "entries": scan.entries,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if !scan.entries.is_empty() {
        for entry in &scan.entries {
            let status = if entry.removed { "removed" } else { "kept" };
            println!(
                "  {status}: {} (source: {}, status: {}, age: {}s, reason: {})",
                entry.path, entry.source, entry.status, entry.age_secs, entry.reason
            );
        }
        if dry_run {
            println!("{orphaned} orphaned sandbox(es) found (dry run, not removed)");
        } else {
            let removed = scan.entries.iter().filter(|entry| entry.removed).count();
            println!(
                "{removed} orphaned sandbox(es) removed ({} failed removals)",
                scan.skipped.delete_failed
            );
        }
        println!(
            "reasons: source_mismatch={}, running={}, recent={}, not_failed={}, missing_manifest={}, delete_failed={}",
            scan.skipped.source_mismatch,
            scan.skipped.running,
            scan.skipped.recent,
            scan.skipped.not_failed,
            scan.skipped.missing_manifest,
            scan.skipped.delete_failed
        );
        print_reconcile_report_text(&collection.reconcile);
    } else {
        println!(
            "no orphaned sandboxes found ({} scanned; reasons source_mismatch={}, running={}, recent={}, not_failed={}, missing_manifest={}, delete_failed={})",
            scan.scanned,
            scan.skipped.source_mismatch,
            scan.skipped.running,
            scan.skipped.recent,
            scan.skipped.not_failed,
            scan.skipped.missing_manifest,
            scan.skipped.delete_failed
        );
        print_reconcile_report_text(&collection.reconcile);
    }

    Ok(0)
}

pub(super) fn run_sandbox_rebuild_index(json: bool) -> Result<u8> {
    let roots = collect_sandbox_directories();
    let report = rebuild_sandbox_index_from_roots(&roots, true);

    if json {
        let payload = serde_json::json!({
            "schema_version": SANDBOX_JSON_SCHEMA_VERSION,
            "scanned": report.scanned,
            "rebuilt_count": report.rebuilt_count,
            "pruned_count": report.pruned_count,
            "missing_manifest": report.missing_manifest,
            "scan_fallback_used": report.scan_fallback_used,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "sandbox index rebuilt: scanned={}, rebuilt={}, pruned={}, missing_manifest={}, fallback={}",
            report.scanned,
            report.rebuilt_count,
            report.pruned_count,
            report.missing_manifest,
            report.scan_fallback_used
        );
    }

    Ok(0)
}

fn cleanup_orphaned_sandboxes(
    candidates: Vec<SandboxCandidate>,
    dry_run: bool,
    failed_only: bool,
    older_than: Duration,
    source_filter: Option<&str>,
) -> CleanupScan {
    let now = SystemTime::now();
    let mut scanned = 0;
    let mut entries = Vec::new();
    let mut skipped = CleanupSkipped {
        source_mismatch: 0,
        running: 0,
        recent: 0,
        not_failed: 0,
        missing_manifest: 0,
        delete_failed: 0,
    };

    for candidate in candidates {
        scanned += 1;
        entries.push(evaluate_cleanup_candidate(
            candidate,
            now,
            dry_run,
            failed_only,
            older_than,
            source_filter,
            &mut skipped,
        ));
    }

    CleanupScan {
        scanned,
        entries,
        skipped,
    }
}

fn evaluate_cleanup_candidate(
    candidate: SandboxCandidate,
    now: SystemTime,
    dry_run: bool,
    failed_only: bool,
    older_than: Duration,
    source_filter: Option<&str>,
    skipped: &mut CleanupSkipped,
) -> CleanupEntry {
    let age = candidate_age(&candidate, now);
    let root_path = candidate.root;
    let source = sandbox_source_for_dir(&root_path);
    let status = sandbox_status_for_dir(&root_path).to_string();

    if let Some(filter) = source_filter
        && source != filter
    {
        skipped.source_mismatch += 1;
        return cleanup_entry(&root_path, source, age, status, "source_mismatch", false);
    }

    if age < older_than {
        skipped.recent += 1;
        return cleanup_entry(&root_path, source, age, status, "recent", false);
    }

    let manifest = read_manifest(&root_path).ok();
    if status == "running" {
        skipped.running += 1;
        return cleanup_entry(&root_path, source, age, status, "running", false);
    }

    if failed_only && status != "failed" {
        if manifest.is_some() {
            skipped.not_failed += 1;
            return cleanup_entry(&root_path, source, age, status, "not_failed", false);
        }
        skipped.missing_manifest += 1;
        return cleanup_entry(&root_path, source, age, status, "missing_manifest", false);
    }

    if dry_run {
        return cleanup_entry(&root_path, source, age, status, "would_remove", false);
    }

    if sandbox_lock_is_fresh(&root_path)
        || sandbox_process_alive(&root_path)
        || sandbox_socket_alive(&root_path)
    {
        skipped.running += 1;
        return cleanup_entry(&root_path, source, age, status, "running", false);
    }

    let removed = std::fs::remove_dir_all(&root_path).is_ok();
    if removed || !root_path.exists() {
        let _ = remove_sandbox_index_entry(&root_path);
        return cleanup_entry(&root_path, source, age, status, "removed", true);
    }

    skipped.delete_failed += 1;
    cleanup_entry(&root_path, source, age, status, "delete_failed", false)
}

fn cleanup_entry(
    root_path: &Path,
    source: &str,
    age: Duration,
    status: String,
    reason: &str,
    removed: bool,
) -> CleanupEntry {
    CleanupEntry {
        path: root_path.to_string_lossy().to_string(),
        source: source.to_string(),
        age_secs: age.as_secs(),
        status,
        reason: reason.to_string(),
        removed,
    }
}

fn cleanup_reason_is_orphaned(reason: &str) -> bool {
    matches!(reason, "removed" | "would_remove" | "delete_failed")
}

fn collect_sandbox_directories() -> Vec<PathBuf> {
    let Ok(dir_entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return Vec::new();
    };

    dir_entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            if !path.is_dir() {
                return false;
            }

            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(SANDBOX_PREFIX)
                    || name.starts_with("bpb-")
                    || name.starts_with("brv-")
            })
        })
        .collect()
}

fn collect_sandbox_directories_index_first() -> Vec<PathBuf> {
    collect_sandbox_candidates()
        .candidates
        .into_iter()
        .map(|candidate| candidate.root)
        .collect()
}

fn collect_sandbox_candidates() -> SandboxCandidateCollection {
    let mut reconcile = ReconcileReport::default();
    let index_exists = sandbox_index_exists_meta();
    match read_sandbox_index_entries() {
        Ok(_) => {
            reconcile.pruned_entries = prune_missing_index_entries().unwrap_or(0);
            if let Ok(entries) = read_sandbox_index_entries() {
                let indexed = index_entries_to_candidates(entries);
                if !indexed.is_empty() {
                    let recovery = recover_candidate_lifecycle_state(&indexed);
                    reconcile.normalized_running = recovery.normalized_running;
                    reconcile.cleared_stale_locks = recovery.cleared_stale_locks;
                    reconcile.healed_entries = reconcile.pruned_entries
                        + reconcile.normalized_running
                        + reconcile.cleared_stale_locks;
                    return SandboxCandidateCollection {
                        candidates: indexed,
                        reconcile,
                    };
                }
            }
        }
        Err(error) => {
            reconcile.scan_fallback_used = true;
            reconcile.index_read_error = Some(error.to_string());
        }
    }

    let roots = collect_sandbox_directories();
    if !roots.is_empty() {
        let rebuild_report = rebuild_sandbox_index_from_roots(&roots, !index_exists);
        reconcile.rebuilt_entries = rebuild_report.rebuilt_count;
        reconcile.pruned_entries = reconcile.pruned_entries.max(rebuild_report.pruned_count);
        reconcile.scan_fallback_used =
            reconcile.scan_fallback_used || rebuild_report.scan_fallback_used;
        if let Ok(entries) = read_sandbox_index_entries() {
            let indexed = index_entries_to_candidates(entries);
            if !indexed.is_empty() {
                let recovery = recover_candidate_lifecycle_state(&indexed);
                reconcile.normalized_running = recovery.normalized_running;
                reconcile.cleared_stale_locks = recovery.cleared_stale_locks;
                reconcile.healed_entries = reconcile.rebuilt_entries
                    + reconcile.pruned_entries
                    + reconcile.normalized_running
                    + reconcile.cleared_stale_locks;
                return SandboxCandidateCollection {
                    candidates: indexed,
                    reconcile,
                };
            }
        }
    }

    let candidates = roots
        .into_iter()
        .map(|root| SandboxCandidate {
            root,
            updated_at_unix_ms: None,
        })
        .collect::<Vec<_>>();

    let recovery = recover_candidate_lifecycle_state(&candidates);
    reconcile.normalized_running = recovery.normalized_running;
    reconcile.cleared_stale_locks = recovery.cleared_stale_locks;
    reconcile.healed_entries = reconcile.rebuilt_entries
        + reconcile.pruned_entries
        + reconcile.normalized_running
        + reconcile.cleared_stale_locks;
    SandboxCandidateCollection {
        candidates,
        reconcile,
    }
}

fn recover_candidate_lifecycle_state(candidates: &[SandboxCandidate]) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    for candidate in candidates {
        let root = &candidate.root;
        if sandbox_lock_is_fresh(root) || sandbox_process_alive(root) || sandbox_socket_alive(root)
        {
            continue;
        }

        if let Ok(mut manifest) = read_manifest(root)
            && manifest.status == "running"
        {
            manifest.updated_at_unix_ms = unix_millis_now_meta();
            manifest.status = "aborted".to_string();
            if manifest.exit_code.is_none() {
                manifest.exit_code = Some(1);
            }
            manifest.kept = true;
            if write_manifest(root, &manifest).is_ok() {
                let _ = upsert_sandbox_index_entry(&manifest);
                report.normalized_running += 1;
            }
        }

        if sandbox_lock_is_stale(root) {
            clear_sandbox_lock(root);
            report.cleared_stale_locks += 1;
        }
    }
    report
}

fn index_entries_to_candidates(
    entries: Vec<crate::sandbox_meta::SandboxIndexEntry>,
) -> Vec<SandboxCandidate> {
    entries
        .into_iter()
        .filter_map(|entry| {
            let root = PathBuf::from(entry.root);
            if root.is_dir() {
                Some(SandboxCandidate {
                    root,
                    updated_at_unix_ms: Some(entry.updated_at_unix_ms),
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
}

fn rebuild_sandbox_index_from_roots(
    roots: &[PathBuf],
    scan_fallback_used: bool,
) -> IndexRebuildReport {
    let existing = read_sandbox_index_entries().unwrap_or_default();
    let previous_roots = existing
        .iter()
        .map(|entry| entry.root.clone())
        .collect::<std::collections::BTreeSet<_>>();

    let mut rebuilt_entries = Vec::new();
    let mut seen_roots = std::collections::BTreeSet::new();
    let mut missing_manifest = 0usize;

    for root in roots {
        seen_roots.insert(root.to_string_lossy().to_string());
        if let Ok(manifest) = read_manifest(root) {
            rebuilt_entries.push(crate::sandbox_meta::SandboxIndexEntry {
                id: manifest.id,
                root: manifest.paths.root,
                source: manifest.source,
                status: manifest.status,
                created_at_unix_ms: manifest.created_at_unix_ms,
                updated_at_unix_ms: manifest.updated_at_unix_ms,
                last_seen_unix_ms: unix_millis_now_meta(),
            });
        } else {
            missing_manifest += 1;
        }
    }

    let _ = replace_sandbox_index_entries(rebuilt_entries.clone());

    let pruned_count = previous_roots
        .iter()
        .filter(|root| !seen_roots.contains(*root))
        .count();

    IndexRebuildReport {
        scanned: roots.len(),
        rebuilt_count: rebuilt_entries.len(),
        pruned_count,
        missing_manifest,
        scan_fallback_used,
    }
}

fn preview_rebuild_sandbox_index_from_roots(roots: &[PathBuf]) -> IndexRebuildReport {
    let existing = read_sandbox_index_entries().unwrap_or_default();
    let previous_roots = existing
        .iter()
        .map(|entry| entry.root.clone())
        .collect::<std::collections::BTreeSet<_>>();

    let mut rebuilt_count = 0usize;
    let mut missing_manifest = 0usize;
    let mut seen_roots = std::collections::BTreeSet::new();
    for root in roots {
        seen_roots.insert(root.to_string_lossy().to_string());
        if read_manifest(root).is_ok() {
            rebuilt_count += 1;
        } else {
            missing_manifest += 1;
        }
    }

    let pruned_count = previous_roots
        .iter()
        .filter(|root| !seen_roots.contains(*root))
        .count();

    IndexRebuildReport {
        scanned: roots.len(),
        rebuilt_count,
        pruned_count,
        missing_manifest,
        scan_fallback_used: !sandbox_index_exists_meta(),
    }
}

fn candidate_sort_key(candidate: &SandboxCandidate) -> u128 {
    candidate.updated_at_unix_ms.unwrap_or_else(|| {
        candidate
            .root
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_millis())
    })
}

fn candidate_age(candidate: &SandboxCandidate, now: SystemTime) -> Duration {
    if let Some(updated_at) = candidate.updated_at_unix_ms {
        let now_unix_ms = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let elapsed_ms = now_unix_ms.saturating_sub(updated_at);
        let elapsed_ms_u64 = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
        return Duration::from_millis(elapsed_ms_u64);
    }

    candidate
        .root
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| now.duration_since(modified).ok())
        .unwrap_or_default()
}

fn sandbox_list_entry(candidate: &SandboxCandidate) -> SandboxListEntry {
    let root = &candidate.root;
    let age = candidate_age(candidate, SystemTime::now());

    let manifest = read_manifest(root).ok();
    SandboxListEntry {
        id: sandbox_id_from_root_meta(root),
        source: manifest.as_ref().map_or_else(
            || sandbox_source_for_dir(root).to_string(),
            |value| value.source.clone(),
        ),
        root: root.to_string_lossy().to_string(),
        status: sandbox_status_for_dir(root).to_string(),
        age_secs: age.as_secs(),
        exit_code: manifest.as_ref().and_then(|value| value.exit_code),
        kept: manifest.as_ref().map(|value| value.kept),
    }
}

fn sandbox_source_for_dir(root: &Path) -> &'static str {
    if let Ok(manifest) = read_manifest(root) {
        return match manifest.source.as_str() {
            "playbook" => "playbook",
            "recording-verify" => "recording-verify",
            _ => "sandbox-cli",
        };
    }

    let Some(name) = root.file_name() else {
        return "sandbox-cli";
    };
    let name = name.to_string_lossy();
    if name.starts_with("bpb-") {
        "playbook"
    } else if name.starts_with("brv-") {
        "recording-verify"
    } else {
        "sandbox-cli"
    }
}

fn sandbox_status_for_dir(root: &Path) -> &'static str {
    if sandbox_lock_is_fresh(root) || sandbox_process_alive(root) || sandbox_socket_alive(root) {
        return "running";
    }

    if let Ok(manifest) = read_manifest(root) {
        if matches!(
            manifest.status.as_str(),
            "failed" | "timed_out" | "killed" | "aborted" | "running"
        ) {
            "failed"
        } else {
            "stopped"
        }
    } else {
        "stopped"
    }
}

fn read_manifest(root: &Path) -> Result<SandboxManifest> {
    read_sandbox_manifest(root)
}

fn write_manifest(root: &Path, manifest: &SandboxManifest) -> Result<()> {
    write_sandbox_manifest(root, manifest)
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

    let directories = collect_sandbox_directories_index_first();

    if let Some(found) = directories
        .iter()
        .find(|candidate| sandbox_id_from_root_meta(candidate) == target)
    {
        return Ok(found.clone());
    }

    let prefix_matches = directories
        .iter()
        .filter_map(|candidate| {
            let id = sandbox_id_from_root_meta(candidate);
            id.starts_with(target).then(|| (candidate.clone(), id))
        })
        .collect::<Vec<_>>();

    if prefix_matches.len() == 1 {
        return Ok(prefix_matches[0].0.clone());
    }
    if prefix_matches.len() > 1 {
        let suggestions = prefix_matches
            .into_iter()
            .map(|(_, id)| id)
            .take(5)
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "sandbox target '{target}' is ambiguous; matches: {suggestions}; use a full id or absolute path"
        );
    }

    let suggestions = directories
        .iter()
        .map(|candidate| sandbox_id_from_root_meta(candidate))
        .filter(|id| id.contains(target))
        .take(5)
        .collect::<Vec<_>>();

    if suggestions.is_empty() {
        anyhow::bail!("sandbox target not found: {target}");
    }
    anyhow::bail!(
        "sandbox target not found: {target}; did you mean: {}",
        suggestions.join(", ")
    )
}

fn format_known_sources(candidates: &[SandboxCandidate]) -> String {
    let mut known = candidates
        .iter()
        .map(|candidate| sandbox_source_for_dir(&candidate.root))
        .collect::<Vec<_>>();
    known.sort_unstable();
    known.dedup();
    known.join(", ")
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
    write_sandbox_lock(root_dir, std::process::id())
}

fn clear_lock(root_dir: &Path) {
    clear_sandbox_lock(root_dir);
}

fn sandbox_lock_is_fresh(root_dir: &Path) -> bool {
    let Some(lock) = read_sandbox_lock(root_dir) else {
        return false;
    };
    let now = unix_millis_now_meta();
    if now.saturating_sub(lock.updated_at_unix_ms) > LOCK_FRESHNESS.as_millis() {
        return false;
    }
    is_pid_alive(lock.pid)
}

fn sandbox_lock_is_stale(root_dir: &Path) -> bool {
    let lock_path = root_dir.join(LOCK_FILE);
    lock_path.exists()
        && !sandbox_lock_is_fresh(root_dir)
        && !sandbox_process_alive(root_dir)
        && !sandbox_socket_alive(root_dir)
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

fn exit_code_to_u8(code: i32) -> u8 {
    if code < 0 {
        1
    } else if code > i32::from(u8::MAX) {
        u8::MAX
    } else {
        u8::try_from(code).unwrap_or(1)
    }
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
        SandboxEnvModeArg, SandboxPaths, apply_sandbox_env, directory_sha256_hex, exit_code_to_u8,
        file_sha256_hex, sanitize_component,
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

    #[test]
    fn file_sha256_hex_matches_known_digest() {
        let root = std::env::temp_dir().join(format!(
            "bmux-file-sha-test-{}-{}",
            std::process::id(),
            super::unix_millis_now_meta()
        ));
        std::fs::create_dir_all(&root).expect("create temp test root");
        let file = root.join("sample.txt");
        std::fs::write(&file, "abc").expect("write sample file");

        let digest = file_sha256_hex(&file).expect("file hash should exist");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn directory_sha256_hex_is_stable_and_detects_changes() {
        let base = std::env::temp_dir().join(format!(
            "bmux-dir-sha-test-{}-{}",
            std::process::id(),
            super::unix_millis_now_meta()
        ));
        let dir_one = base.join("one");
        let dir_two = base.join("two");
        std::fs::create_dir_all(dir_one.join("nested")).expect("create dir one");
        std::fs::create_dir_all(dir_two.join("nested")).expect("create dir two");

        std::fs::write(dir_one.join("b.txt"), "bbb").expect("write one b");
        std::fs::write(dir_one.join("nested").join("a.txt"), "aaa").expect("write one a");

        std::fs::write(dir_two.join("nested").join("a.txt"), "aaa").expect("write two a");
        std::fs::write(dir_two.join("b.txt"), "bbb").expect("write two b");

        let hash_one = directory_sha256_hex(&dir_one).expect("dir one hash should exist");
        let hash_two = directory_sha256_hex(&dir_two).expect("dir two hash should exist");
        assert_eq!(hash_one, hash_two, "directory hash should be order-stable");

        std::fs::write(dir_two.join("nested").join("a.txt"), "changed")
            .expect("mutate nested file");
        let hash_two_changed =
            directory_sha256_hex(&dir_two).expect("dir two changed hash should exist");
        assert_ne!(
            hash_one, hash_two_changed,
            "directory hash should change on content drift"
        );

        let _ = std::fs::remove_dir_all(base);
    }
}
