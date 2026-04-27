mod benchmark_manifest;
mod phase_schema;

use benchmark_manifest::{
    BenchmarkManifest, BenchmarkResolvedOptions, BenchmarkRunOptions, read_manifest,
    resolve_benchmark_options,
};
use phase_schema::validate_phase_schema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bmux_plugin::ServiceCaller;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        return Err(usage().to_string());
    }

    let subcommand = args.remove(0);
    match subcommand.as_str() {
        "sample" => run_sample(args),
        "report-latency" => run_report_latency(args),
        "report-faults" => run_report_faults(args),
        "report-json" => run_report_json(args),
        "report-phase-file" => run_report_phase_file(args),
        "validate-phase-config" => run_validate_phase_config(args),
        "validate-phase-schema" => run_validate_phase_schema(args),
        "list-benchmarks" => run_list_benchmarks(args),
        "run-benchmark" => run_benchmark(args),
        "sample-generic-ipc" => run_sample_generic_ipc(args),
        "sample-static-service" => run_sample_static_service(args),
        "sample-core-services" => run_sample_core_services(args),
        "prepare-scale-fixture" => run_prepare_scale_fixture(args),
        "compare-report" => run_compare_report(args),
        "discover-run-candidate" => run_discover_run_candidate(args),
        "-h" | "--help" => {
            println!("{}", usage());
            Ok(())
        }
        _ => Err(format!("unknown subcommand '{subcommand}'\n{}", usage())),
    }
}

fn usage() -> &'static str {
    "bmux-perf-tools commands:
  sample --iterations N --allow-nonzero 0|1 --out-json PATH -- <command...>
  report-latency --input PATH [--max-p95-ms N] [--max-p99-ms N] [--max-avg-ms N] [--max-steady-p95-ms N] [--max-steady-p99-ms N] [--max-steady-avg-ms N]
  report-faults --input PATH [--max-runtime-retries N] [--max-runtime-respawns N] [--max-runtime-timeouts N]
  report-json --input PATH --output PATH [threshold flags]
  report-phase-file --input PATH --output PATH --phase NAME --field FIELD [--filter-key KEY --filter-value VALUE] [--max-p99-ms N] [--max-p95-ms N]
  validate-phase-config --input PATH --config PATH --output-dir PATH [--tag TAG] [--limit NAME=MS] [--var NAME=VALUE]
  validate-phase-schema --input PATH
  list-benchmarks [--dir PATH]
  run-benchmark --manifest PATH [--profile NAME] [--artifact-json PATH] [--phase-report-dir PATH] [benchmark overrides]
  sample-generic-ipc --iterations N --warmup N --out-json PATH
  sample-static-service --iterations N --warmup N --out-json PATH [--max-p99-us N]
  sample-core-services --iterations N --warmup N --out-json PATH [--max-p99-us N]
  prepare-scale-fixture --config-dir PATH --plugin-root PATH --count N [--profile small|medium|large]
  compare-report --baseline PATH --candidate PATH [--candidate PATH ...] [--warn-regression-ms N] [--json-output PATH]
  discover-run-candidate --bmux-bin PATH"
}

const PHASE_MARKER: &str = "[bmux-plugin-phase-json]";
const ATTACH_PHASE_MARKER: &str = "[bmux-attach-phase-json]";
const SERVICE_PHASE_MARKER: &str = "[bmux-service-phase-json]";
const IPC_PHASE_MARKER: &str = "[bmux-ipc-phase-json]";
const STORAGE_PHASE_MARKER: &str = "[bmux-storage-phase-json]";

#[derive(Debug, Deserialize)]
struct PhaseReportConfig {
    #[serde(default)]
    limit: BTreeMap<String, f64>,
    #[serde(default)]
    reports: Vec<PhaseReportSpec>,
}

#[derive(Debug, Deserialize)]
struct PhaseReportSpec {
    phase: String,
    field: String,
    #[serde(default)]
    filter: Option<PhaseReportFilter>,
    #[serde(default)]
    max_p99_ms: Option<f64>,
    #[serde(default)]
    max_p95_ms: Option<f64>,
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PhaseReportFilter {
    key: String,
    value: String,
}

struct PhaseReportRequest<'a> {
    input_label: &'a str,
    events: &'a [Value],
    output: &'a str,
    phase: &'a str,
    field: &'a str,
    filter_key: Option<&'a str>,
    filter_value: Option<&'a str>,
    max_p99_ms: Option<f64>,
    max_p95_ms: Option<f64>,
}

struct SampleCommandRequest<'a> {
    iterations: usize,
    command: &'a [String],
    envs: &'a [(String, String)],
    out_json: &'a str,
    allow_nonzero: bool,
}

fn run_benchmark(args: Vec<String>) -> Result<(), String> {
    let cli = parse_benchmark_run_options(args)?;
    let manifest_path = cli
        .manifest
        .clone()
        .ok_or_else(|| "--manifest is required".to_string())?;
    let manifest = read_manifest(&manifest_path)?;
    let options = resolve_benchmark_options(cli, &manifest, manifest_path)?;
    match manifest.benchmark.kind.as_str() {
        "core-services" => run_core_services_benchmark(&manifest, &options),
        "generic-ipc" => run_generic_ipc_benchmark(&manifest, &options),
        "attach-tab-switch" => run_attach_tab_switch_benchmark(&manifest, &options),
        "plugin-hot-path" => run_plugin_hot_path_benchmark(&manifest, &options),
        other => Err(format!("unsupported benchmark kind '{other}'")),
    }
}

fn parse_benchmark_run_options(args: Vec<String>) -> Result<BenchmarkRunOptions, String> {
    let mut options = BenchmarkRunOptions {
        profile: "normal".to_string(),
        ..BenchmarkRunOptions::default()
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--manifest" => {
                options.manifest = args.get(index + 1).cloned();
                index += 2;
            }
            "--profile" => {
                options.profile = require_arg(&args, index, "--profile")?.to_string();
                index += 2;
            }
            "--artifact-json" => {
                options.artifact_json = args.get(index + 1).cloned();
                index += 2;
            }
            "--phase-report-dir" => {
                options.phase_report_dir = args.get(index + 1).cloned();
                index += 2;
            }
            "--bmux-bin" => {
                options.bmux_bin = args.get(index + 1).cloned();
                index += 2;
            }
            "--iterations" => {
                options.iterations = Some(parse_usize_arg(&args, index, "--iterations")?);
                index += 2;
            }
            "--warmup" => {
                options.warmup = Some(parse_usize_arg(&args, index, "--warmup")?);
                index += 2;
            }
            "--scenario" => {
                options.scenario = args.get(index + 1).cloned();
                index += 2;
            }
            "--windows" => {
                options.windows = Some(parse_usize_arg(&args, index, "--windows")?);
                index += 2;
            }
            "--switches" => {
                options.switches = Some(parse_usize_arg(&args, index, "--switches")?);
                index += 2;
            }
            "--max-p99-ms" => {
                options.max_p99_ms = Some(parse_f64(
                    require_arg(&args, index, "--max-p99-ms")?,
                    "--max-p99-ms",
                )?);
                index += 2;
            }
            "--tag" => {
                options
                    .tags
                    .push(require_arg(&args, index, "--tag")?.to_string());
                index += 2;
            }
            "--limit" => {
                let value = require_arg(&args, index, "--limit")?;
                let Some((name, limit)) = value.split_once('=') else {
                    return Err("--limit requires NAME=MS".to_string());
                };
                options
                    .limits
                    .insert(name.to_string(), parse_f64(limit, "--limit")?);
                index += 2;
            }
            "--var" => {
                let value = require_arg(&args, index, "--var")?;
                let Some((name, value)) = value.split_once('=') else {
                    return Err("--var requires NAME=VALUE".to_string());
                };
                options.vars.insert(name.to_string(), value.to_string());
                index += 2;
            }
            other => return Err(format!("unknown argument for run-benchmark: {other}")),
        }
    }
    Ok(options)
}

fn require_arg<'a>(args: &'a [String], index: usize, name: &str) -> Result<&'a str, String> {
    args.get(index + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("{name} requires a value"))
}

fn parse_usize_arg(args: &[String], index: usize, name: &str) -> Result<usize, String> {
    let value = parse_u64(require_arg(args, index, name)?, name)?;
    usize::try_from(value).map_err(|_| format!("{name} is too large"))
}

fn run_core_services_benchmark(
    manifest: &BenchmarkManifest,
    options: &BenchmarkResolvedOptions,
) -> Result<(), String> {
    let artifact_json = options.artifact_json.clone().unwrap_or_else(|| {
        env::temp_dir()
            .join("bmux-core-services.json")
            .display()
            .to_string()
    });
    run_sample_core_services(vec![
        "--iterations".to_string(),
        options.iterations.to_string(),
        "--warmup".to_string(),
        options.warmup.to_string(),
        "--out-json".to_string(),
        artifact_json.clone(),
    ])?;
    validate_benchmark_phases(&artifact_json, options)?;
    write_standard_benchmark_artifact(manifest, options, &artifact_json, &artifact_json)?;
    println!("artifact_json={artifact_json}");
    println!("phase_report_dir={}", options.phase_report_dir);
    Ok(())
}

fn run_generic_ipc_benchmark(
    manifest: &BenchmarkManifest,
    options: &BenchmarkResolvedOptions,
) -> Result<(), String> {
    let artifact_json = options.artifact_json.clone().unwrap_or_else(|| {
        env::temp_dir()
            .join("bmux-generic-ipc.json")
            .display()
            .to_string()
    });
    let sample_json = env::temp_dir()
        .join(format!("bmux-generic-ipc-sample-{}.json", process::id()))
        .display()
        .to_string();
    let exe =
        env::current_exe().map_err(|error| format!("failed resolving current exe: {error}"))?;
    let output = Command::new(exe)
        .args([
            "sample-generic-ipc",
            "--iterations",
            &options.iterations.to_string(),
            "--warmup",
            &options.warmup.to_string(),
            "--out-json",
            &sample_json,
        ])
        .env("BMUX_IPC_PHASE_TIMING", "1")
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("failed running generic IPC sample: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("generic IPC sample failed:\n{stderr}"));
    }
    merge_sample_events(&sample_json, parse_phase_events(&stderr))?;
    validate_benchmark_phases(&sample_json, options)?;
    write_standard_benchmark_artifact(manifest, options, &sample_json, &artifact_json)?;
    let _ = fs::remove_file(&sample_json);
    println!("artifact_json={artifact_json}");
    println!("phase_report_dir={}", options.phase_report_dir);
    Ok(())
}

fn run_plugin_hot_path_benchmark(
    manifest: &BenchmarkManifest,
    options: &BenchmarkResolvedOptions,
) -> Result<(), String> {
    let artifact_json = options.artifact_json.clone().unwrap_or_else(|| {
        env::temp_dir()
            .join("bmux-plugin-hot-path.json")
            .display()
            .to_string()
    });
    run_sample_static_service(vec![
        "--iterations".to_string(),
        options.iterations.to_string(),
        "--warmup".to_string(),
        options.warmup.to_string(),
        "--out-json".to_string(),
        artifact_json.clone(),
    ])?;
    validate_benchmark_phases(&artifact_json, options)?;
    write_standard_benchmark_artifact(manifest, options, &artifact_json, &artifact_json)?;
    println!("artifact_json={artifact_json}");
    println!("phase_report_dir={}", options.phase_report_dir);
    Ok(())
}

fn run_attach_tab_switch_benchmark(
    manifest: &BenchmarkManifest,
    options: &BenchmarkResolvedOptions,
) -> Result<(), String> {
    let bmux_bin = options
        .bmux_bin
        .clone()
        .ok_or_else(|| "attach-tab-switch requires --bmux-bin".to_string())?;
    validate_attach_options(options)?;
    let scenario = attach_scenario(&options.scenario)?;
    let sandbox = create_temp_dir("bmux-attach-benchmark")?;
    let playbook = sandbox.join("tab-switch.dsl");
    let sample_json = sandbox.join("sample.json");
    write_attach_playbook(&playbook, options, &scenario)?;
    let envs = attach_envs(options);
    for _ in 0..options.warmup {
        run_command_once(
            &[
                bmux_bin.clone(),
                "playbook".to_string(),
                "run".to_string(),
                playbook.display().to_string(),
                "--json".to_string(),
            ],
            &envs,
        )?;
    }
    let command = vec![
        bmux_bin,
        "playbook".to_string(),
        "run".to_string(),
        playbook.display().to_string(),
        "--json".to_string(),
    ];
    sample_command(SampleCommandRequest {
        iterations: options.iterations,
        command: &command,
        envs: &envs,
        out_json: &sample_json.display().to_string(),
        allow_nonzero: false,
    })?;
    report_latency_if_needed(&sample_json.display().to_string(), options.max_p99_ms)?;
    let mut vars = options.vars.clone();
    vars.entry("command_name".to_string())
        .or_insert(scenario.command_name.to_string());
    vars.entry("service_operation".to_string())
        .or_insert(scenario.service_operation.to_string());
    validate_benchmark_phases_with_vars(&sample_json.display().to_string(), options, vars)?;
    let artifact_json = options.artifact_json.clone().unwrap_or_else(|| {
        env::temp_dir()
            .join("bmux-attach-tab-switch.json")
            .display()
            .to_string()
    });
    write_standard_benchmark_artifact(
        manifest,
        options,
        &sample_json.display().to_string(),
        &artifact_json,
    )?;
    let _ = fs::remove_dir_all(&sandbox);
    println!("artifact_json={artifact_json}");
    println!("phase_report_dir={}", options.phase_report_dir);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct AttachScenario {
    command_name: &'static str,
    service_operation: &'static str,
    prime_key: &'static str,
}

fn attach_scenario(name: &str) -> Result<AttachScenario, String> {
    match name {
        "next-window" => Ok(AttachScenario {
            command_name: "next-window",
            service_operation: "switch-window",
            prime_key: "ctrl+h",
        }),
        "prev-window" => Ok(AttachScenario {
            command_name: "prev-window",
            service_operation: "switch-window",
            prime_key: "ctrl+s",
        }),
        "goto-window" => Ok(AttachScenario {
            command_name: "goto-window",
            service_operation: "switch-window",
            prime_key: "alt+1",
        }),
        "new-window" => Ok(AttachScenario {
            command_name: "new-window",
            service_operation: "new-window",
            prime_key: "",
        }),
        other => Err(format!("unknown attach scenario '{other}'")),
    }
}

fn validate_attach_options(options: &BenchmarkResolvedOptions) -> Result<(), String> {
    if options.windows < 2 {
        return Err("attach benchmark requires at least 2 windows".to_string());
    }
    if options.switches < 1 {
        return Err("attach benchmark requires at least 1 switch".to_string());
    }
    if options.scenario == "goto-window" && options.windows < 3 {
        return Err("goto-window scenario requires at least 3 windows".to_string());
    }
    Ok(())
}

fn write_attach_playbook(
    path: &Path,
    options: &BenchmarkResolvedOptions,
    scenario: &AttachScenario,
) -> Result<(), String> {
    let mut lines = vec![
        "@timeout 30000".to_string(),
        "@shell sh".to_string(),
        "@viewport cols=120 rows=40".to_string(),
        "new-session".to_string(),
    ];
    for _ in 1..=options.windows {
        lines.push("send-attach key=c".to_string());
    }
    if !scenario.prime_key.is_empty() {
        lines.push(format!("send-attach key={}", scenario.prime_key));
    }
    for index in 1..=options.switches {
        let key = match options.scenario.as_str() {
            "next-window" => "ctrl+s",
            "prev-window" => "ctrl+h",
            "goto-window" if index % 2 == 0 => "alt+2",
            "goto-window" => "alt+3",
            "new-window" => "c",
            _ => return Err(format!("unknown attach scenario '{}'", options.scenario)),
        };
        lines.push(format!("send-attach key={key}"));
    }
    lines.push("screen".to_string());
    fs::write(path, format!("{}\n", lines.join("\n")))
        .map_err(|error| format!("failed writing attach playbook {}: {error}", path.display()))
}

fn attach_envs(options: &BenchmarkResolvedOptions) -> Vec<(String, String)> {
    let mut envs = vec![("BMUX_ATTACH_PHASE_TIMING".to_string(), "1".to_string())];
    if options.service_timing {
        envs.push(("BMUX_SERVICE_PHASE_TIMING".to_string(), "1".to_string()));
        envs.push((
            "BMUX_PLAYBOOK_FORWARD_SANDBOX_PHASE_TIMING".to_string(),
            "1".to_string(),
        ));
    }
    if options.ipc_timing {
        envs.push(("BMUX_IPC_PHASE_TIMING".to_string(), "1".to_string()));
        envs.push((
            "BMUX_PLAYBOOK_FORWARD_SANDBOX_PHASE_TIMING".to_string(),
            "1".to_string(),
        ));
    }
    if options.storage_timing {
        envs.push((
            "BMUX_PLUGIN_STORAGE_PHASE_TIMING".to_string(),
            "1".to_string(),
        ));
        envs.push((
            "BMUX_PLAYBOOK_FORWARD_SANDBOX_PHASE_TIMING".to_string(),
            "1".to_string(),
        ));
    }
    envs
}

fn create_temp_dir(prefix: &str) -> Result<std::path::PathBuf, String> {
    let path = env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock is before unix epoch: {error}"))?
            .as_nanos()
    ));
    fs::create_dir_all(&path)
        .map_err(|error| format!("failed creating temp dir {}: {error}", path.display()))?;
    Ok(path)
}

fn run_command_once(command: &[String], envs: &[(String, String)]) -> Result<(), String> {
    let Some((command_name, command_args)) = command.split_first() else {
        return Err("command is empty".to_string());
    };
    let status = Command::new(command_name)
        .args(command_args)
        .envs(envs.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("failed executing command: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "command exited with non-zero status: {}",
            status.code().unwrap_or(1)
        ))
    }
}

fn sample_command(request: SampleCommandRequest<'_>) -> Result<(), String> {
    let Some((command_name, command_args)) = request.command.split_first() else {
        return Err("sample requires command args".to_string());
    };
    let mut samples_ms = Vec::with_capacity(request.iterations);
    let mut retries = 0_u64;
    let mut respawns = 0_u64;
    let mut timeouts = 0_u64;
    let mut phase_samples = Vec::with_capacity(request.iterations);
    for _ in 0..request.iterations {
        let started = Instant::now();
        let output = Command::new(command_name)
            .args(command_args)
            .envs(request.envs.iter().map(|(key, value)| (key, value)))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("failed executing sampled command: {error}"))?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        if !request.allow_nonzero && !output.status.success() {
            return Err(format!(
                "command exited with non-zero status: {}",
                output.status.code().unwrap_or(1)
            ));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let faults = count_runtime_faults(&stderr);
        retries += faults.retries;
        respawns += faults.respawns;
        timeouts += faults.timeouts;
        phase_samples.push(parse_phase_events(&stderr));
        samples_ms.push(elapsed_ms);
    }
    let payload = json!({
        "samples_ms": samples_ms,
        "runtime_faults": {
            "retries": retries,
            "respawns": respawns,
            "timeouts": timeouts,
        },
        "phase_samples": phase_samples,
    });
    let encoded = serde_json::to_vec_pretty(&payload)
        .map_err(|error| format!("failed encoding sample json: {error}"))?;
    fs::write(request.out_json, encoded)
        .map_err(|error| format!("failed writing sample json: {error}"))
}

fn report_latency_if_needed(input: &str, max_p99_ms: Option<f64>) -> Result<(), String> {
    let mut args = vec!["--input".to_string(), input.to_string()];
    if let Some(limit) = max_p99_ms {
        args.push("--max-p99-ms".to_string());
        args.push(format_f64_arg(limit));
    }
    run_report_latency(args)
}

fn validate_benchmark_phases(
    input: &str,
    options: &BenchmarkResolvedOptions,
) -> Result<(), String> {
    validate_benchmark_phases_with_vars(input, options, options.vars.clone())
}

fn validate_benchmark_phases_with_vars(
    input: &str,
    options: &BenchmarkResolvedOptions,
    vars: BTreeMap<String, String>,
) -> Result<(), String> {
    let mut args = vec![
        "--input".to_string(),
        input.to_string(),
        "--config".to_string(),
        options.manifest_path.clone(),
        "--output-dir".to_string(),
        options.phase_report_dir.clone(),
    ];
    for tag in &options.tags {
        args.push("--tag".to_string());
        args.push(tag.clone());
    }
    for (name, limit) in &options.limits {
        args.push("--limit".to_string());
        args.push(format!("{name}={}", format_f64_arg(*limit)));
    }
    for (name, value) in vars {
        args.push("--var".to_string());
        args.push(format!("{name}={value}"));
    }
    run_validate_phase_config(args)
}

fn write_standard_benchmark_artifact(
    manifest: &BenchmarkManifest,
    options: &BenchmarkResolvedOptions,
    input: &str,
    output: &str,
) -> Result<(), String> {
    let payload = read_json_file(input)?;
    let events = parse_phase_events_input(&serde_json::to_string(&payload).unwrap_or_default());
    let latency = parse_samples_ms(&payload)
        .ok()
        .map(|samples| stats_json(compute_latency_stats(&samples)));
    let artifact = json!({
        "schema_version": 1,
        "benchmark": manifest.benchmark.name,
        "kind": manifest.benchmark.kind,
        "started_at_unix_ms": unix_now_ms(),
        "git_rev": git_output(&["rev-parse", "HEAD"]),
        "git_dirty": git_dirty(),
        "manifest_path": options.manifest_path,
        "command": format!("run-benchmark --manifest {} --profile {}", options.manifest_path, options.profile),
        "profile": options.profile,
        "scenario": options.scenario,
        "iterations": options.iterations,
        "warmup": options.warmup,
        "phase_report_dir": options.phase_report_dir,
        "limits": options.limits,
        "tags": options.tags,
        "loosen_slo": options.loosen_slo,
        "latency_ms": latency,
        "events": events,
        "raw": payload,
    });
    let encoded = serde_json::to_vec_pretty(&artifact)
        .map_err(|error| format!("failed encoding benchmark artifact: {error}"))?;
    fs::write(output, encoded)
        .map_err(|error| format!("failed writing benchmark artifact {output}: {error}"))
}

fn unix_now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_dirty() -> Option<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(!output.stdout.is_empty())
}

fn format_f64_arg(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn run_validate_phase_config(args: Vec<String>) -> Result<(), String> {
    let mut input = None;
    let mut config = None;
    let mut output_dir = None;
    let mut tags = Vec::new();
    let mut limit_overrides = BTreeMap::new();
    let mut vars = BTreeMap::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--input" => {
                input = args.get(index + 1).cloned();
                index += 2;
            }
            "--config" => {
                config = args.get(index + 1).cloned();
                index += 2;
            }
            "--output-dir" => {
                output_dir = args.get(index + 1).cloned();
                index += 2;
            }
            "--tag" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--tag requires a value".to_string());
                };
                tags.push(value.clone());
                index += 2;
            }
            "--limit" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--limit requires NAME=MS".to_string());
                };
                let Some((name, limit)) = value.split_once('=') else {
                    return Err("--limit requires NAME=MS".to_string());
                };
                limit_overrides.insert(name.to_string(), parse_f64(limit, "--limit")?);
                index += 2;
            }
            "--var" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--var requires NAME=VALUE".to_string());
                };
                let Some((name, value)) = value.split_once('=') else {
                    return Err("--var requires NAME=VALUE".to_string());
                };
                vars.insert(name.to_string(), value.to_string());
                index += 2;
            }
            other => {
                return Err(format!(
                    "unknown argument for validate-phase-config: {other}"
                ));
            }
        }
    }

    let input = input.ok_or_else(|| "--input is required".to_string())?;
    let config = config.ok_or_else(|| "--config is required".to_string())?;
    let output_dir = output_dir.ok_or_else(|| "--output-dir is required".to_string())?;
    fs::create_dir_all(&output_dir)
        .map_err(|error| format!("failed creating phase output dir {output_dir}: {error}"))?;

    let input_text = fs::read_to_string(&input)
        .map_err(|error| format!("failed reading phase input {input}: {error}"))?;
    let events = parse_phase_events_input(&input_text);
    let config_text = fs::read_to_string(&config)
        .map_err(|error| format!("failed reading phase config {config}: {error}"))?;
    let config: PhaseReportConfig = toml::from_str(&config_text)
        .map_err(|error| format!("failed parsing phase config: {error}"))?;

    let mut limits = config.limit;
    limits.extend(limit_overrides);
    let mut failures = Vec::new();
    let enabled_tags = tags.iter().map(String::as_str).collect::<Vec<_>>();
    for (report_index, report) in config.reports.iter().enumerate() {
        if !report.tags.is_empty()
            && !report
                .tags
                .iter()
                .any(|tag| enabled_tags.contains(&tag.as_str()))
        {
            continue;
        }
        let max_p99_ms = if let Some(limit_name) = &report.limit {
            limits.get(limit_name).copied().or(report.max_p99_ms)
        } else {
            report.max_p99_ms
        };
        let output = format!(
            "{}/{}.{}.{}.json",
            output_dir,
            report_index,
            sanitize_report_component(&report.phase),
            sanitize_report_component(&report.field)
        );
        let result = write_phase_report(PhaseReportRequest {
            input_label: &input,
            events: &events,
            output: &output,
            phase: &report.phase,
            field: &report.field,
            filter_key: report.filter.as_ref().map(|filter| filter.key.as_str()),
            filter_value: report
                .filter
                .as_ref()
                .map(|filter| expand_report_vars(&filter.value, &vars))
                .as_deref(),
            max_p99_ms,
            max_p95_ms: report.max_p95_ms,
        });
        if let Err(error) = result {
            failures.push(format!(
                "phase={} field={} failed: {error}",
                report.phase, report.field
            ));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

fn run_validate_phase_schema(args: Vec<String>) -> Result<(), String> {
    let mut input = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--input" => {
                input = args.get(index + 1).cloned();
                index += 2;
            }
            other => {
                return Err(format!(
                    "unknown argument for validate-phase-schema: {other}"
                ));
            }
        }
    }
    let input = input.ok_or_else(|| "--input is required".to_string())?;
    let text = fs::read_to_string(&input)
        .map_err(|error| format!("failed reading phase input {input}: {error}"))?;
    let events = parse_phase_events_input(&text);
    validate_phase_schema(&events).map_err(|errors| errors.join("\n"))?;
    println!("phase schema validation passed: {} events", events.len());
    Ok(())
}

fn run_list_benchmarks(args: Vec<String>) -> Result<(), String> {
    let mut dir = "perf".to_string();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dir" => {
                dir = require_arg(&args, index, "--dir")?.to_string();
                index += 2;
            }
            other => return Err(format!("unknown argument for list-benchmarks: {other}")),
        }
    }
    let mut entries = fs::read_dir(&dir)
        .map_err(|error| format!("failed reading benchmark dir {dir}: {error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
        .collect::<Vec<_>>();
    entries.sort();
    println!("benchmark\tkind\tprofiles\tmanifest");
    for path in entries {
        let path_text = path.display().to_string();
        if let Ok(manifest) = read_manifest(&path_text) {
            let profiles = manifest
                .profiles
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(",");
            println!(
                "{}\t{}\t{}\t{}",
                manifest.benchmark.name, manifest.benchmark.kind, profiles, path_text
            );
        }
    }
    Ok(())
}

fn expand_report_vars(value: &str, vars: &BTreeMap<String, String>) -> String {
    let mut expanded = value.to_string();
    for (key, replacement) in vars {
        expanded = expanded.replace(&format!("${{{key}}}"), replacement);
    }
    expanded
}

fn sanitize_report_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn run_report_phase_file(args: Vec<String>) -> Result<(), String> {
    let mut input = None;
    let mut output = None;
    let mut phase = None;
    let mut field = None;
    let mut filter_key = None;
    let mut filter_value = None;
    let mut max_p99_ms = None;
    let mut max_p95_ms = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--input" => {
                input = args.get(index + 1).cloned();
                index += 2;
            }
            "--output" => {
                output = args.get(index + 1).cloned();
                index += 2;
            }
            "--phase" => {
                phase = args.get(index + 1).cloned();
                index += 2;
            }
            "--field" => {
                field = args.get(index + 1).cloned();
                index += 2;
            }
            "--filter-key" => {
                filter_key = args.get(index + 1).cloned();
                index += 2;
            }
            "--filter-value" => {
                filter_value = args.get(index + 1).cloned();
                index += 2;
            }
            "--max-p99-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-p99-ms requires a value".to_string());
                };
                max_p99_ms = Some(parse_f64(value, "--max-p99-ms")?);
                index += 2;
            }
            "--max-p95-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-p95-ms requires a value".to_string());
                };
                max_p95_ms = Some(parse_f64(value, "--max-p95-ms")?);
                index += 2;
            }
            other => return Err(format!("unknown argument for report-phase-file: {other}")),
        }
    }

    let input = input.ok_or_else(|| "--input is required".to_string())?;
    let output = output.ok_or_else(|| "--output is required".to_string())?;
    let phase = phase.ok_or_else(|| "--phase is required".to_string())?;
    let field = field.ok_or_else(|| "--field is required".to_string())?;
    let text = fs::read_to_string(&input)
        .map_err(|error| format!("failed reading phase input {input}: {error}"))?;
    let events = parse_phase_events_input(&text);
    write_phase_report(PhaseReportRequest {
        input_label: &input,
        events: &events,
        output: &output,
        phase: &phase,
        field: &field,
        filter_key: filter_key.as_deref(),
        filter_value: filter_value.as_deref(),
        max_p99_ms,
        max_p95_ms,
    })
}

fn write_phase_report(request: PhaseReportRequest<'_>) -> Result<(), String> {
    let selected = request
        .events
        .iter()
        .filter(|event| event.get("phase").and_then(Value::as_str) == Some(request.phase))
        .filter(|event| match (request.filter_key, request.filter_value) {
            (Some(key), Some(value)) => event.get(key).and_then(Value::as_str) == Some(value),
            _ => true,
        })
        .cloned()
        .collect::<Vec<_>>();
    let samples_us = selected
        .iter()
        .filter_map(|event| event.get(request.field).and_then(Value::as_f64))
        .collect::<Vec<_>>();
    if samples_us.is_empty() {
        return Err(format!(
            "no numeric samples found for phase '{}' field '{}' in {}",
            request.phase, request.field, request.input_label
        ));
    }
    let samples_ms = samples_us
        .iter()
        .map(|sample| sample / 1000.0)
        .collect::<Vec<_>>();
    let stats_ms = compute_latency_stats(&samples_ms);
    let stats_us = compute_latency_stats(&samples_us);
    println!(
        "phase={} field={} samples={} p50={:.3}ms p95={:.3}ms p99={:.3}ms avg={:.3}ms max={:.3}ms",
        request.phase,
        request.field,
        samples_us.len(),
        stats_ms.p50,
        stats_ms.p95,
        stats_ms.p99,
        stats_ms.avg,
        stats_ms.max
    );

    let mut violations = Vec::new();
    if let Some(limit) = request.max_p99_ms
        && stats_ms.p99 > limit
    {
        violations.push(format!("p99 {:.3}ms > {:.3}ms", stats_ms.p99, limit));
    }
    if let Some(limit) = request.max_p95_ms
        && stats_ms.p95 > limit
    {
        violations.push(format!("p95 {:.3}ms > {:.3}ms", stats_ms.p95, limit));
    }
    let passed = violations.is_empty();
    let payload = json!({
        "phase": request.phase,
        "field": request.field,
        "filter": {
            "key": request.filter_key,
            "value": request.filter_value,
        },
        "sample_count": samples_us.len(),
        "samples_us": samples_us,
        "latency_us": stats_json(stats_us),
        "latency_ms": stats_json(stats_ms),
        "events": selected,
        "limits": {
            "max_p99_ms": request.max_p99_ms,
            "max_p95_ms": request.max_p95_ms,
        },
        "passed": passed,
        "violations": violations,
    });
    let encoded = serde_json::to_vec_pretty(&payload)
        .map_err(|error| format!("failed encoding phase report: {error}"))?;
    fs::write(request.output, encoded)
        .map_err(|error| format!("failed writing phase report {}: {error}", request.output))?;
    if passed {
        Ok(())
    } else {
        Err("phase SLO failed".to_string())
    }
}

fn run_sample_static_service(args: Vec<String>) -> Result<(), String> {
    let mut iterations = None;
    let mut warmup = 0_usize;
    let mut out_json = None;
    let mut max_p99_us = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--iterations" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--iterations requires a value".to_string());
                };
                iterations = Some(parse_u64(value, "--iterations")? as usize);
                index += 2;
            }
            "--warmup" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--warmup requires a value".to_string());
                };
                warmup = parse_u64(value, "--warmup")? as usize;
                index += 2;
            }
            "--out-json" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--out-json requires a value".to_string());
                };
                out_json = Some(value.clone());
                index += 2;
            }
            "--max-p99-us" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-p99-us requires a value".to_string());
                };
                max_p99_us = Some(parse_u64(value, "--max-p99-us")? as f64);
                index += 2;
            }
            other => {
                return Err(format!(
                    "unknown argument for sample-static-service: {other}"
                ));
            }
        }
    }

    let iterations = iterations.ok_or_else(|| "--iterations is required".to_string())?;
    let out_json = out_json.ok_or_else(|| "--out-json is required".to_string())?;
    let loaded = load_performance_plugin()?;

    for _ in 0..warmup {
        invoke_performance_get_settings(&loaded)?;
    }

    let mut samples_us = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        invoke_performance_get_settings(&loaded)?;
        samples_us.push(started.elapsed().as_secs_f64() * 1_000_000.0);
    }

    let stats = compute_latency_stats(&samples_us);
    println!(
        "static_service_us min={:.3} p50={:.3} p95={:.3} p99={:.3} avg={:.3} max={:.3}",
        stats.min, stats.p50, stats.p95, stats.p99, stats.avg, stats.max
    );
    if let Some(limit) = max_p99_us
        && stats.p99 > limit
    {
        return Err(format!(
            "static service SLO failed: p99 {:.3}us > {:.3}us",
            stats.p99, limit
        ));
    }

    let payload = json!({
        "scenario": "static-performance-get-settings",
        "samples_us": samples_us,
        "events": samples_us.iter().map(|total_us| {
            json!({
                "phase": "static_service.direct",
                "scenario": "static_service.direct",
                "total_us": total_us,
            })
        }).collect::<Vec<_>>(),
        "latency_us": stats_json(stats),
        "limits": { "max_p99_us": max_p99_us },
        "passed": true,
    });
    let encoded = serde_json::to_vec_pretty(&payload)
        .map_err(|error| format!("failed encoding static service sample json: {error}"))?;
    fs::write(out_json, encoded)
        .map_err(|error| format!("failed writing static service sample json: {error}"))?;
    Ok(())
}

fn run_sample_core_services(args: Vec<String>) -> Result<(), String> {
    let mut iterations = None;
    let mut warmup = 0_usize;
    let mut out_json = None;
    let mut max_p99_us = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--iterations" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--iterations requires a value".to_string());
                };
                iterations = Some(parse_u64(value, "--iterations")? as usize);
                index += 2;
            }
            "--warmup" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--warmup requires a value".to_string());
                };
                warmup = parse_u64(value, "--warmup")? as usize;
                index += 2;
            }
            "--out-json" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--out-json requires a value".to_string());
                };
                out_json = Some(value.clone());
                index += 2;
            }
            "--max-p99-us" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-p99-us requires a value".to_string());
                };
                max_p99_us = Some(parse_u64(value, "--max-p99-us")? as f64);
                index += 2;
            }
            other => {
                return Err(format!(
                    "unknown argument for sample-core-services: {other}"
                ));
            }
        }
    }

    let iterations = iterations.ok_or_else(|| "--iterations is required".to_string())?;
    let out_json = out_json.ok_or_else(|| "--out-json is required".to_string())?;

    let sandbox = env::temp_dir().join(format!(
        "bmux-core-service-perf-{}-{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock is before unix epoch: {error}"))?
            .as_nanos()
    ));
    let data_dir = sandbox.join("data");
    fs::create_dir_all(&data_dir)
        .map_err(|error| format!("failed creating benchmark data dir: {error}"))?;
    let connection = bmux_plugin_sdk::HostConnectionInfo {
        config_dir: sandbox.join("config").display().to_string(),
        config_dir_candidates: Vec::new(),
        runtime_dir: sandbox.join("runtime").display().to_string(),
        data_dir: data_dir.display().to_string(),
        state_dir: sandbox.join("state").display().to_string(),
    };
    let context = CoreServiceBenchContext::new(connection);

    let result = (|| {
        for index in 0..warmup {
            context.storage_set("warmup", b"warmup")?;
            let _: Option<Vec<u8>> = context.storage_get("warmup")?;
            context.volatile_set("warmup", b"warmup")?;
            let _: Option<Vec<u8>> = context.volatile_get("warmup")?;
            context.volatile_clear("warmup")?;
            context.prepare_storage_file(&format!("warmup-cold-{index}"), b"warmup")?;
        }

        context.storage_set("cached", b"cached-value")?;
        let _: Option<Vec<u8>> = context.storage_get("cached")?;
        context.volatile_set("volatile", b"volatile-value")?;

        let storage_cold_get = sample_core_service_operation(iterations, |index| {
            let key = format!("cold-get-{index}");
            context.prepare_storage_file(&key, b"cold-value")?;
            let _: Option<Vec<u8>> = context.storage_get(&key)?;
            Ok(())
        })?;
        let storage_cached_get = sample_core_service_operation(iterations, |_| {
            let _: Option<Vec<u8>> = context.storage_get("cached")?;
            Ok(())
        })?;
        let storage_set = sample_core_service_operation(iterations, |index| {
            context.storage_set(&format!("set-{index}"), b"set-value")
        })?;
        let volatile_get = sample_core_service_operation(iterations, |_| {
            let _: Option<Vec<u8>> = context.volatile_get("volatile")?;
            Ok(())
        })?;
        let volatile_set = sample_core_service_operation(iterations, |index| {
            context.volatile_set(&format!("volatile-set-{index}"), b"volatile-value")
        })?;
        let volatile_clear = sample_core_service_operation(iterations, |index| {
            let key = format!("volatile-clear-{index}");
            context.volatile_set(&key, b"volatile-value")?;
            context.volatile_clear(&key)
        })?;

        let static_service_path = sandbox.join("static-service.json");
        run_sample_static_service(vec![
            "--iterations".to_string(),
            iterations.to_string(),
            "--warmup".to_string(),
            warmup.to_string(),
            "--out-json".to_string(),
            static_service_path.display().to_string(),
        ])?;
        let static_service_payload = fs::read_to_string(&static_service_path)
            .map_err(|error| format!("failed reading static service sample json: {error}"))?;
        let static_service_json: Value = serde_json::from_str(&static_service_payload)
            .map_err(|error| format!("failed decoding static service sample json: {error}"))?;
        let static_service_samples = static_service_json
            .get("samples_us")
            .and_then(Value::as_array)
            .ok_or_else(|| "static service sample json missing samples_us".to_string())?
            .iter()
            .filter_map(Value::as_f64)
            .collect::<Vec<_>>();

        let scenarios = vec![
            core_service_scenario_json("storage.cold_get", storage_cold_get, max_p99_us),
            core_service_scenario_json("storage.cached_get", storage_cached_get, max_p99_us),
            core_service_scenario_json("storage.set", storage_set, max_p99_us),
            core_service_scenario_json("volatile.get", volatile_get, max_p99_us),
            core_service_scenario_json("volatile.set", volatile_set, max_p99_us),
            core_service_scenario_json("volatile.clear", volatile_clear, max_p99_us),
            core_service_scenario_json("static_service.direct", static_service_samples, max_p99_us),
        ];
        let events = scenarios
            .iter()
            .flat_map(core_service_phase_events)
            .collect::<Vec<_>>();
        let violations = scenarios
            .iter()
            .filter_map(|scenario| scenario.get("violation").and_then(Value::as_str))
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for scenario in &scenarios {
            let stats = scenario
                .get("latency_us")
                .ok_or_else(|| "scenario missing latency_us".to_string())?;
            println!(
                "{} p50={:.3}us p95={:.3}us p99={:.3}us avg={:.3}us max={:.3}us",
                scenario
                    .get("scenario")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                stats.get("p50").and_then(Value::as_f64).unwrap_or(0.0),
                stats.get("p95").and_then(Value::as_f64).unwrap_or(0.0),
                stats.get("p99").and_then(Value::as_f64).unwrap_or(0.0),
                stats.get("avg").and_then(Value::as_f64).unwrap_or(0.0),
                stats.get("max").and_then(Value::as_f64).unwrap_or(0.0),
            );
        }
        let passed = violations.is_empty();
        let payload = json!({
            "scenario": "core-services",
            "iterations": iterations,
            "warmup": warmup,
            "scenarios": scenarios,
            "events": events,
            "limits": { "max_p99_us": max_p99_us },
            "passed": passed,
            "violations": violations,
        });
        let encoded = serde_json::to_vec_pretty(&payload)
            .map_err(|error| format!("failed encoding core service sample json: {error}"))?;
        fs::write(&out_json, encoded)
            .map_err(|error| format!("failed writing core service sample json: {error}"))?;
        if passed {
            Ok(())
        } else {
            Err("core service SLO failed".to_string())
        }
    })();

    let cleanup_result = fs::remove_dir_all(&sandbox);
    if let Err(error) = cleanup_result
        && result.is_ok()
    {
        return Err(format!("failed cleaning benchmark sandbox: {error}"));
    }
    result
}

fn core_service_phase_events(scenario: &Value) -> Vec<Value> {
    let scenario_name = scenario
        .get("scenario")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    scenario
        .get("samples_us")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_f64)
        .map(|total_us| {
            json!({
                "phase": "core_service",
                "scenario": scenario_name,
                "total_us": total_us,
            })
        })
        .collect()
}

fn run_sample_generic_ipc(args: Vec<String>) -> Result<(), String> {
    let mut iterations = None;
    let mut warmup = 0_usize;
    let mut out_json = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--iterations" => {
                iterations = Some(parse_usize_arg(&args, index, "--iterations")?);
                index += 2;
            }
            "--warmup" => {
                warmup = parse_usize_arg(&args, index, "--warmup")?;
                index += 2;
            }
            "--out-json" => {
                out_json = Some(require_arg(&args, index, "--out-json")?.to_string());
                index += 2;
            }
            other => return Err(format!("unknown argument for sample-generic-ipc: {other}")),
        }
    }

    let iterations = iterations.ok_or_else(|| "--iterations is required".to_string())?;
    let out_json = out_json.ok_or_else(|| "--out-json is required".to_string())?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("failed creating tokio runtime: {error}"))?;
    runtime.block_on(sample_generic_ipc_async(iterations, warmup, &out_json))
}

async fn sample_generic_ipc_async(
    iterations: usize,
    warmup: usize,
    out_json: &str,
) -> Result<(), String> {
    let endpoint = generic_ipc_endpoint();
    let server = std::sync::Arc::new(bmux_server::BmuxServer::new(endpoint.clone()));
    server
        .register_service_handler(
            "bmux.perf.ipc",
            bmux_ipc::InvokeServiceKind::Query,
            "generic-ipc/v1",
            "echo",
            |_route, _context, payload| async move { Ok(payload) },
        )
        .map_err(|error| format!("failed registering generic IPC handler: {error}"))?;
    let server_task = {
        let server = std::sync::Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    let mut client = connect_generic_ipc_client(&endpoint).await?;

    for _ in 0..warmup {
        generic_ipc_ping(&mut client).await?;
        generic_ipc_echo(&mut client, &[]).await?;
        generic_ipc_echo(&mut client, &[7_u8; 64]).await?;
        generic_ipc_echo(&mut client, &[9_u8; 4096]).await?;
    }

    let ping = sample_generic_ipc_ping_operation(iterations, &mut client).await?;
    let invoke_empty = sample_generic_ipc_echo_operation(iterations, &mut client, &[]).await?;
    let small_payload = vec![7_u8; 64];
    let invoke_small =
        sample_generic_ipc_echo_operation(iterations, &mut client, &small_payload).await?;
    let medium_payload = vec![9_u8; 4096];
    let invoke_medium =
        sample_generic_ipc_echo_operation(iterations, &mut client, &medium_payload).await?;

    server.request_shutdown();
    let _ = client.stop_server().await;
    match tokio::time::timeout(std::time::Duration::from_secs(5), server_task).await {
        Ok(Ok(Ok(()))) | Ok(Err(_)) => {}
        Ok(Ok(Err(error))) => return Err(format!("generic IPC server failed: {error:#}")),
        Err(_) => return Err("generic IPC server did not shut down".to_string()),
    }
    cleanup_generic_ipc_endpoint(&endpoint);

    let scenarios = vec![
        generic_ipc_scenario_json("ping", ping),
        generic_ipc_scenario_json("invoke.empty", invoke_empty),
        generic_ipc_scenario_json("invoke.small", invoke_small),
        generic_ipc_scenario_json("invoke.medium", invoke_medium),
    ];
    let events = scenarios
        .iter()
        .flat_map(generic_ipc_phase_events)
        .collect::<Vec<_>>();
    for scenario in &scenarios {
        let stats = scenario
            .get("latency_us")
            .ok_or_else(|| "generic IPC scenario missing latency_us".to_string())?;
        println!(
            "{} p50={:.3}us p95={:.3}us p99={:.3}us avg={:.3}us max={:.3}us",
            scenario
                .get("scenario")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            stats.get("p50").and_then(Value::as_f64).unwrap_or(0.0),
            stats.get("p95").and_then(Value::as_f64).unwrap_or(0.0),
            stats.get("p99").and_then(Value::as_f64).unwrap_or(0.0),
            stats.get("avg").and_then(Value::as_f64).unwrap_or(0.0),
            stats.get("max").and_then(Value::as_f64).unwrap_or(0.0),
        );
    }
    let all_samples_ms = scenarios
        .iter()
        .flat_map(|scenario| {
            scenario
                .get("samples_us")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_f64)
                .map(|value| value / 1000.0)
        })
        .collect::<Vec<_>>();
    let payload = json!({
        "scenario": "generic-ipc",
        "iterations": iterations,
        "warmup": warmup,
        "samples_ms": all_samples_ms,
        "scenarios": scenarios,
        "events": events,
        "passed": true,
    });
    let encoded = serde_json::to_vec_pretty(&payload)
        .map_err(|error| format!("failed encoding generic IPC sample json: {error}"))?;
    fs::write(out_json, encoded)
        .map_err(|error| format!("failed writing generic IPC sample json: {error}"))
}

async fn connect_generic_ipc_client(
    endpoint: &bmux_ipc::IpcEndpoint,
) -> Result<bmux_client::BmuxClient, String> {
    let mut last_error = None;
    for _ in 0..100 {
        match bmux_client::BmuxClient::connect(
            endpoint,
            std::time::Duration::from_secs(5),
            "bmux-perf-generic-ipc",
        )
        .await
        {
            Ok(client) => return Ok(client),
            Err(error) => {
                last_error = Some(error.to_string());
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }
    Err(format!(
        "failed connecting generic IPC client: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    ))
}

async fn generic_ipc_ping(client: &mut bmux_client::BmuxClient) -> Result<(), String> {
    client
        .ping()
        .await
        .map_err(|error| format!("generic IPC ping failed: {error}"))
}

async fn generic_ipc_echo(
    client: &mut bmux_client::BmuxClient,
    payload: &[u8],
) -> Result<(), String> {
    let response = client
        .invoke_service_raw(
            "bmux.perf.ipc",
            bmux_ipc::InvokeServiceKind::Query,
            "generic-ipc/v1",
            "echo",
            payload.to_vec(),
        )
        .await
        .map_err(|error| format!("generic IPC echo failed: {error}"))?;
    if response == payload {
        Ok(())
    } else {
        Err("generic IPC echo returned unexpected payload".to_string())
    }
}

async fn sample_generic_ipc_ping_operation(
    iterations: usize,
    client: &mut bmux_client::BmuxClient,
) -> Result<Vec<f64>, String> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        generic_ipc_ping(client).await?;
        samples.push(started.elapsed().as_secs_f64() * 1_000_000.0);
    }
    Ok(samples)
}

async fn sample_generic_ipc_echo_operation(
    iterations: usize,
    client: &mut bmux_client::BmuxClient,
    payload: &[u8],
) -> Result<Vec<f64>, String> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        generic_ipc_echo(client, payload).await?;
        samples.push(started.elapsed().as_secs_f64() * 1_000_000.0);
    }
    Ok(samples)
}

fn generic_ipc_scenario_json(name: &str, samples_us: Vec<f64>) -> Value {
    let stats = compute_latency_stats(&samples_us);
    json!({
        "scenario": name,
        "samples_us": samples_us,
        "latency_us": stats_json(stats),
    })
}

fn generic_ipc_phase_events(scenario: &Value) -> Vec<Value> {
    let Some(name) = scenario.get("scenario").and_then(Value::as_str) else {
        return Vec::new();
    };
    scenario
        .get("samples_us")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_f64)
        .map(|total_us| {
            json!({
                "phase": "generic_ipc",
                "scenario": name,
                "total_us": total_us,
            })
        })
        .collect()
}

fn generic_ipc_endpoint() -> bmux_ipc::IpcEndpoint {
    #[cfg(unix)]
    {
        let path = env::temp_dir().join(format!(
            "bmux-generic-ipc-{}-{}.sock",
            process::id(),
            unix_now_ms()
        ));
        bmux_ipc::IpcEndpoint::unix_socket(path)
    }
    #[cfg(windows)]
    {
        bmux_ipc::IpcEndpoint::windows_named_pipe(format!(
            r"\\.\pipe\bmux-generic-ipc-{}-{}",
            process::id(),
            unix_now_ms()
        ))
    }
}

fn cleanup_generic_ipc_endpoint(endpoint: &bmux_ipc::IpcEndpoint) {
    #[cfg(unix)]
    if let Some(path) = endpoint.as_unix_socket() {
        let _ = fs::remove_file(path);
    }
    #[cfg(windows)]
    let _ = endpoint;
}

struct CoreServiceBenchContext {
    connection: bmux_plugin_sdk::HostConnectionInfo,
    caller: bmux_plugin::TypedServiceCaller,
}

impl CoreServiceBenchContext {
    fn new(connection: bmux_plugin_sdk::HostConnectionInfo) -> Self {
        let storage = bmux_plugin_sdk::HostScope::new("bmux.storage")
            .expect("storage capability should parse");
        let services = vec![
            bmux_plugin_sdk::RegisteredService {
                capability: storage.clone(),
                kind: bmux_plugin_sdk::ServiceKind::Query,
                interface_id: "storage-query/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            },
            bmux_plugin_sdk::RegisteredService {
                capability: storage.clone(),
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "storage-command/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            },
            bmux_plugin_sdk::RegisteredService {
                capability: storage.clone(),
                kind: bmux_plugin_sdk::ServiceKind::Query,
                interface_id: "volatile-state-query/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            },
            bmux_plugin_sdk::RegisteredService {
                capability: storage,
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "volatile-state-command/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            },
        ];
        let host = bmux_plugin_sdk::HostMetadata {
            product_name: "bmux".to_string(),
            product_version: env!("CARGO_PKG_VERSION").to_string(),
            plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
            plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
        };
        let required_capabilities = vec!["bmux.storage".to_string()];
        let available_capabilities = vec!["bmux.storage".to_string()];
        let enabled_plugins = vec!["bmux.perf-tools".to_string()];
        let plugin_search_roots = Vec::new();
        let plugin_settings_map = BTreeMap::new();
        let registration_context = bmux_plugin_sdk::TypedServiceRegistrationContext {
            plugin_id: "bmux.perf-tools",
            host_kernel_bridge: None,
            required_capabilities: &required_capabilities,
            provided_capabilities: &[],
            services: &services,
            available_capabilities: &available_capabilities,
            enabled_plugins: &enabled_plugins,
            plugin_search_roots: &plugin_search_roots,
            host: &host,
            connection: &connection,
            plugin_settings_map: &plugin_settings_map,
        };
        let caller =
            bmux_plugin::TypedServiceCaller::from_registration_context(&registration_context);
        Self { connection, caller }
    }

    fn prepare_storage_file(&self, key: &str, value: &[u8]) -> Result<(), String> {
        let path = Path::new(&self.connection.data_dir)
            .join("plugin-storage")
            .join("bmux.perf-tools")
            .join(format!("{key}.bin"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed creating storage fixture dir: {error}"))?;
        }
        fs::write(path, value).map_err(|error| format!("failed writing storage fixture: {error}"))
    }

    fn storage_get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let request = bmux_plugin_sdk::StorageGetRequest {
            key: key.to_string(),
        };
        self.call(
            bmux_plugin_sdk::ServiceKind::Query,
            "storage-query/v1",
            "get",
            &request,
        )
        .map(|response: bmux_plugin_sdk::StorageGetResponse| response.value)
    }

    fn storage_set(&self, key: &str, value: &[u8]) -> Result<(), String> {
        let request = bmux_plugin_sdk::StorageSetRequest {
            key: key.to_string(),
            value: value.to_vec(),
        };
        self.call(
            bmux_plugin_sdk::ServiceKind::Command,
            "storage-command/v1",
            "set",
            &request,
        )
    }

    fn volatile_get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let request = bmux_plugin_sdk::VolatileStateGetRequest {
            key: key.to_string(),
        };
        self.call(
            bmux_plugin_sdk::ServiceKind::Query,
            "volatile-state-query/v1",
            "get",
            &request,
        )
        .map(|response: bmux_plugin_sdk::VolatileStateGetResponse| response.value)
    }

    fn volatile_set(&self, key: &str, value: &[u8]) -> Result<(), String> {
        let request = bmux_plugin_sdk::VolatileStateSetRequest {
            key: key.to_string(),
            value: value.to_vec(),
        };
        self.call(
            bmux_plugin_sdk::ServiceKind::Command,
            "volatile-state-command/v1",
            "set",
            &request,
        )
    }

    fn volatile_clear(&self, key: &str) -> Result<(), String> {
        let request = bmux_plugin_sdk::VolatileStateClearRequest {
            key: key.to_string(),
        };
        self.call(
            bmux_plugin_sdk::ServiceKind::Command,
            "volatile-state-command/v1",
            "clear",
            &request,
        )
    }

    fn call<Request, Response>(
        &self,
        kind: bmux_plugin_sdk::ServiceKind,
        interface_id: &str,
        operation: &str,
        request: &Request,
    ) -> Result<Response, String>
    where
        Request: serde::Serialize,
        Response: serde::de::DeserializeOwned,
    {
        let payload = bmux_plugin_sdk::encode_service_message(request)
            .map_err(|error| format!("failed encoding request: {error}"))?;
        let response = self
            .caller
            .call_service_raw("bmux.storage", kind, interface_id, operation, payload)
            .map_err(|error| format!("core service call failed: {error}"))?;
        bmux_plugin_sdk::decode_service_message(&response)
            .map_err(|error| format!("failed decoding response: {error}"))
    }
}

fn sample_core_service_operation(
    iterations: usize,
    mut operation: impl FnMut(usize) -> Result<(), String>,
) -> Result<Vec<f64>, String> {
    let mut samples = Vec::with_capacity(iterations);
    for index in 0..iterations {
        let started = Instant::now();
        operation(index)?;
        samples.push(started.elapsed().as_secs_f64() * 1_000_000.0);
    }
    Ok(samples)
}

fn core_service_scenario_json(name: &str, samples_us: Vec<f64>, max_p99_us: Option<f64>) -> Value {
    let stats = compute_latency_stats(&samples_us);
    let violation = max_p99_us.and_then(|limit| {
        (stats.p99 > limit).then(|| format!("{name} p99 {:.3}us > {:.3}us", stats.p99, limit))
    });
    json!({
        "scenario": name,
        "samples_us": samples_us,
        "latency_us": stats_json(stats),
        "violation": violation,
    })
}

fn load_performance_plugin() -> Result<bmux_plugin::LoadedPlugin, String> {
    const NOOP_MANIFEST: &str = r#"
id = "bmux.perf-tools.noop"
name = "bmux Perf Tools Noop"
version = "0.0.1-alpha.0"
execution_class = "native_fast"
provided_capabilities = ["bmux.perf_tools.noop"]
required_capabilities = []

[[services]]
capability = "bmux.perf_tools.noop"
interface_id = "perf-noop/v1"
kind = "query"
"#;

    let mut registry = bmux_plugin::PluginRegistry::new();
    registry
        .register_bundled_manifest(NOOP_MANIFEST)
        .map_err(|error| format!("failed registering noop manifest: {error}"))?;
    let registered = registry
        .get("bmux.perf-tools.noop")
        .ok_or_else(|| "noop plugin was not registered".to_string())?;
    let host = bmux_plugin_sdk::HostMetadata {
        product_name: "bmux".to_string(),
        product_version: env!("CARGO_PKG_VERSION").to_string(),
        plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
        plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
    };
    let vtable = bmux_plugin_sdk::bundled_plugin_vtable!(PerfNoopPlugin, NOOP_MANIFEST);
    bmux_plugin::load_static_plugin(registered, vtable, &host, &BTreeMap::new())
        .map_err(|error| format!("failed loading noop plugin: {error}"))
}

fn invoke_performance_get_settings(loaded: &bmux_plugin::LoadedPlugin) -> Result<(), String> {
    let payload = bmux_plugin_sdk::encode_service_message(&())
        .map_err(|error| format!("failed encoding noop request: {error}"))?;
    let capability = bmux_plugin_sdk::HostScope::new("bmux.perf_tools.noop")
        .map_err(|error| format!("failed parsing noop capability: {error}"))?;
    let response = loaded
        .invoke_service(&bmux_plugin_sdk::NativeServiceContext {
            plugin_id: "bmux.perf-tools.noop".to_string(),
            request: bmux_plugin_sdk::ServiceRequest {
                caller_plugin_id: "bmux.perf-tools".to_string(),
                service: bmux_plugin_sdk::RegisteredService {
                    capability,
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface_id: "perf-noop/v1".to_string(),
                    provider: bmux_plugin_sdk::ProviderId::Plugin(
                        "bmux.perf-tools.noop".to_string(),
                    ),
                },
                operation: "ping".to_string(),
                payload,
            },
            required_capabilities: Vec::new(),
            provided_capabilities: vec!["bmux.perf_tools.noop".to_string()],
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: vec!["bmux.perf-tools.noop".to_string()],
            plugin_search_roots: Vec::new(),
            host: bmux_plugin_sdk::HostMetadata {
                product_name: "bmux".to_string(),
                product_version: env!("CARGO_PKG_VERSION").to_string(),
                plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: Vec::new(),
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            host_kernel_bridge: None,
        })
        .map_err(|error| format!("noop service invocation failed: {error}"))?;
    if let Some(error) = response.error {
        return Err(format!("noop service returned error: {}", error.message));
    }
    bmux_plugin_sdk::decode_service_message::<()>(&response.payload)
        .map_err(|error| format!("failed decoding noop response: {error}"))?;
    Ok(())
}

#[derive(Default)]
struct PerfNoopPlugin;

impl bmux_plugin_sdk::RustPlugin for PerfNoopPlugin {
    fn invoke_service(
        &mut self,
        context: bmux_plugin_sdk::NativeServiceContext,
    ) -> bmux_plugin_sdk::ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "perf-noop/v1", "ping" => |_req: (), _ctx| {
                Ok::<(), bmux_plugin_sdk::ServiceResponse>(())
            },
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct LatencyThresholds {
    max_p95_ms: Option<f64>,
    max_p99_ms: Option<f64>,
    max_avg_ms: Option<f64>,
    max_steady_p95_ms: Option<f64>,
    max_steady_p99_ms: Option<f64>,
    max_steady_avg_ms: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct RuntimeFaultThresholds {
    max_runtime_retries: Option<u64>,
    max_runtime_respawns: Option<u64>,
    max_runtime_timeouts: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LatencyStats {
    min: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    avg: f64,
    max: f64,
}

#[derive(Debug, Clone, Copy)]
struct LatencyBreakdown {
    startup_ms: f64,
    overall: LatencyStats,
    steady_state: Option<LatencyStats>,
}

fn run_sample(args: Vec<String>) -> Result<(), String> {
    let mut iterations = None;
    let mut allow_nonzero = false;
    let mut out_json = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--iterations" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--iterations requires a value".to_string());
                };
                iterations = Some(parse_u64(value, "--iterations")? as usize);
                index += 2;
            }
            "--allow-nonzero" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--allow-nonzero requires a value".to_string());
                };
                allow_nonzero = match value.as_str() {
                    "0" => false,
                    "1" => true,
                    _ => {
                        return Err("--allow-nonzero expects 0 or 1".to_string());
                    }
                };
                index += 2;
            }
            "--out-json" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--out-json requires a value".to_string());
                };
                out_json = Some(value.clone());
                index += 2;
            }
            "--" => {
                index += 1;
                break;
            }
            other => {
                return Err(format!("unknown argument for sample: {other}"));
            }
        }
    }

    let command = args[index..].to_vec();
    if command.is_empty() {
        return Err("sample requires command args after --".to_string());
    }

    let iterations = iterations.ok_or_else(|| "--iterations is required".to_string())?;
    let out_json = out_json.ok_or_else(|| "--out-json is required".to_string())?;

    sample_command(SampleCommandRequest {
        iterations,
        command: &command,
        envs: &[],
        out_json: &out_json,
        allow_nonzero,
    })
}

fn parse_phase_events(stderr: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|line| {
            line.split_once(PHASE_MARKER)
                .or_else(|| line.split_once(ATTACH_PHASE_MARKER))
                .or_else(|| line.split_once(SERVICE_PHASE_MARKER))
                .or_else(|| line.split_once(IPC_PHASE_MARKER))
                .or_else(|| line.split_once(STORAGE_PHASE_MARKER))
                .map(|(_, payload)| payload.trim())
        })
        .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .collect()
}

fn parse_phase_events_input(input: &str) -> Vec<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(input) {
        if let Some(events) = value.as_array() {
            return events.clone();
        }
        if let Some(samples) = value.get("phase_samples").and_then(Value::as_array) {
            return samples
                .iter()
                .flat_map(|sample| sample.as_array().into_iter().flatten().cloned())
                .collect();
        }
        if let Some(events) = value.get("events").and_then(Value::as_array) {
            return events.clone();
        }
    }
    parse_phase_events(input)
}

fn run_report_latency(args: Vec<String>) -> Result<(), String> {
    let mut input = None;
    let mut max_p95_ms = None;
    let mut max_p99_ms = None;
    let mut max_avg_ms = None;
    let mut max_steady_p95_ms = None;
    let mut max_steady_p99_ms = None;
    let mut max_steady_avg_ms = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--input" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--input requires a value".to_string());
                };
                input = Some(value.clone());
                index += 2;
            }
            "--max-p95-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-p95-ms requires a value".to_string());
                };
                max_p95_ms = Some(parse_u64(value, "--max-p95-ms")? as f64);
                index += 2;
            }
            "--max-p99-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-p99-ms requires a value".to_string());
                };
                max_p99_ms = Some(parse_u64(value, "--max-p99-ms")? as f64);
                index += 2;
            }
            "--max-avg-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-avg-ms requires a value".to_string());
                };
                max_avg_ms = Some(parse_u64(value, "--max-avg-ms")? as f64);
                index += 2;
            }
            "--max-steady-p95-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-steady-p95-ms requires a value".to_string());
                };
                max_steady_p95_ms = Some(parse_u64(value, "--max-steady-p95-ms")? as f64);
                index += 2;
            }
            "--max-steady-p99-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-steady-p99-ms requires a value".to_string());
                };
                max_steady_p99_ms = Some(parse_u64(value, "--max-steady-p99-ms")? as f64);
                index += 2;
            }
            "--max-steady-avg-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-steady-avg-ms requires a value".to_string());
                };
                max_steady_avg_ms = Some(parse_u64(value, "--max-steady-avg-ms")? as f64);
                index += 2;
            }
            other => {
                return Err(format!("unknown argument for report-latency: {other}"));
            }
        }
    }

    let input = input.ok_or_else(|| "--input is required".to_string())?;
    let payload = read_json_file(&input)?;
    let samples = parse_samples_ms(&payload)?;
    let breakdown = compute_latency_breakdown(&samples);
    let stats = breakdown.overall;

    println!(
        "latency_ms min={:.3} p50={:.3} p95={:.3} p99={:.3} avg={:.3} max={:.3}",
        stats.min, stats.p50, stats.p95, stats.p99, stats.avg, stats.max
    );
    println!("latency_startup_ms value={:.3}", breakdown.startup_ms);
    if let Some(steady) = breakdown.steady_state {
        println!(
            "latency_steady_ms min={:.3} p50={:.3} p95={:.3} p99={:.3} avg={:.3} max={:.3}",
            steady.min, steady.p50, steady.p95, steady.p99, steady.avg, steady.max
        );
    }

    let violations = evaluate_latency_thresholds(
        breakdown,
        LatencyThresholds {
            max_p95_ms,
            max_p99_ms,
            max_avg_ms,
            max_steady_p95_ms,
            max_steady_p99_ms,
            max_steady_avg_ms,
        },
    );

    if !violations.is_empty() {
        return Err(format!("SLO check failed: {}", violations.join("; ")));
    }
    Ok(())
}

fn run_report_faults(args: Vec<String>) -> Result<(), String> {
    let mut input = None;
    let mut max_runtime_retries = None;
    let mut max_runtime_respawns = None;
    let mut max_runtime_timeouts = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--input" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--input requires a value".to_string());
                };
                input = Some(value.clone());
                index += 2;
            }
            "--max-runtime-retries" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-runtime-retries requires a value".to_string());
                };
                max_runtime_retries = Some(parse_u64(value, "--max-runtime-retries")?);
                index += 2;
            }
            "--max-runtime-respawns" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-runtime-respawns requires a value".to_string());
                };
                max_runtime_respawns = Some(parse_u64(value, "--max-runtime-respawns")?);
                index += 2;
            }
            "--max-runtime-timeouts" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-runtime-timeouts requires a value".to_string());
                };
                max_runtime_timeouts = Some(parse_u64(value, "--max-runtime-timeouts")?);
                index += 2;
            }
            other => {
                return Err(format!("unknown argument for report-faults: {other}"));
            }
        }
    }

    let input = input.ok_or_else(|| "--input is required".to_string())?;
    let payload = read_json_file(&input)?;
    let faults = parse_runtime_fault_counts(&payload)?;

    println!(
        "runtime_faults retries={} respawns={} timeouts={}",
        faults.retries, faults.respawns, faults.timeouts
    );

    let violations = evaluate_fault_thresholds(
        faults,
        RuntimeFaultThresholds {
            max_runtime_retries,
            max_runtime_respawns,
            max_runtime_timeouts,
        },
    );

    if !violations.is_empty() {
        return Err(format!(
            "runtime fault check failed: {}",
            violations.join("; ")
        ));
    }

    Ok(())
}

fn run_report_json(args: Vec<String>) -> Result<(), String> {
    let mut input = None;
    let mut output = None;
    let mut latency_thresholds = LatencyThresholds::default();
    let mut fault_thresholds = RuntimeFaultThresholds::default();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--input" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--input requires a value".to_string());
                };
                input = Some(value.clone());
                index += 2;
            }
            "--output" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--output requires a value".to_string());
                };
                output = Some(value.clone());
                index += 2;
            }
            "--max-p95-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-p95-ms requires a value".to_string());
                };
                latency_thresholds.max_p95_ms = Some(parse_u64(value, "--max-p95-ms")? as f64);
                index += 2;
            }
            "--max-p99-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-p99-ms requires a value".to_string());
                };
                latency_thresholds.max_p99_ms = Some(parse_u64(value, "--max-p99-ms")? as f64);
                index += 2;
            }
            "--max-avg-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-avg-ms requires a value".to_string());
                };
                latency_thresholds.max_avg_ms = Some(parse_u64(value, "--max-avg-ms")? as f64);
                index += 2;
            }
            "--max-steady-p95-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-steady-p95-ms requires a value".to_string());
                };
                latency_thresholds.max_steady_p95_ms =
                    Some(parse_u64(value, "--max-steady-p95-ms")? as f64);
                index += 2;
            }
            "--max-steady-p99-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-steady-p99-ms requires a value".to_string());
                };
                latency_thresholds.max_steady_p99_ms =
                    Some(parse_u64(value, "--max-steady-p99-ms")? as f64);
                index += 2;
            }
            "--max-steady-avg-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-steady-avg-ms requires a value".to_string());
                };
                latency_thresholds.max_steady_avg_ms =
                    Some(parse_u64(value, "--max-steady-avg-ms")? as f64);
                index += 2;
            }
            "--max-runtime-retries" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-runtime-retries requires a value".to_string());
                };
                fault_thresholds.max_runtime_retries =
                    Some(parse_u64(value, "--max-runtime-retries")?);
                index += 2;
            }
            "--max-runtime-respawns" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-runtime-respawns requires a value".to_string());
                };
                fault_thresholds.max_runtime_respawns =
                    Some(parse_u64(value, "--max-runtime-respawns")?);
                index += 2;
            }
            "--max-runtime-timeouts" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--max-runtime-timeouts requires a value".to_string());
                };
                fault_thresholds.max_runtime_timeouts =
                    Some(parse_u64(value, "--max-runtime-timeouts")?);
                index += 2;
            }
            other => {
                return Err(format!("unknown argument for report-json: {other}"));
            }
        }
    }

    let input = input.ok_or_else(|| "--input is required".to_string())?;
    let output = output.ok_or_else(|| "--output is required".to_string())?;
    let payload = read_json_file(&input)?;
    let samples = parse_samples_ms(&payload)?;
    let faults = parse_runtime_fault_counts(&payload)?;
    let phase_samples = payload
        .get("phase_samples")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let breakdown = compute_latency_breakdown(&samples);
    let stats = breakdown.overall;

    let mut violations = Vec::new();
    violations.extend(evaluate_latency_thresholds(breakdown, latency_thresholds));
    violations.extend(evaluate_fault_thresholds(faults, fault_thresholds));

    let report = json!({
        "sample_count": samples.len(),
        "startup_ms": breakdown.startup_ms,
        "latency_ms": {
            "min": stats.min,
            "p50": stats.p50,
            "p95": stats.p95,
            "p99": stats.p99,
            "avg": stats.avg,
            "max": stats.max,
        },
        "steady_state_ms": breakdown.steady_state.map(|steady| {
            json!({
                "min": steady.min,
                "p50": steady.p50,
                "p95": steady.p95,
                "p99": steady.p99,
                "avg": steady.avg,
                "max": steady.max,
            })
        }),
        "runtime_faults": {
            "retries": faults.retries,
            "respawns": faults.respawns,
            "timeouts": faults.timeouts,
        },
        "phase_samples": phase_samples,
        "limits": {
            "max_p95_ms": latency_thresholds.max_p95_ms,
            "max_p99_ms": latency_thresholds.max_p99_ms,
            "max_avg_ms": latency_thresholds.max_avg_ms,
            "max_steady_p95_ms": latency_thresholds.max_steady_p95_ms,
            "max_steady_p99_ms": latency_thresholds.max_steady_p99_ms,
            "max_steady_avg_ms": latency_thresholds.max_steady_avg_ms,
            "max_runtime_retries": fault_thresholds.max_runtime_retries,
            "max_runtime_respawns": fault_thresholds.max_runtime_respawns,
            "max_runtime_timeouts": fault_thresholds.max_runtime_timeouts,
        },
        "violations": violations,
        "passed": violations.is_empty(),
    });
    let encoded = serde_json::to_vec_pretty(&report)
        .map_err(|error| format!("failed encoding report json: {error}"))?;
    fs::write(output, encoded).map_err(|error| format!("failed writing report json: {error}"))?;
    Ok(())
}

fn run_prepare_scale_fixture(args: Vec<String>) -> Result<(), String> {
    let mut config_dir = None;
    let mut plugin_root = None;
    let mut count = None;
    let mut profile = "medium".to_string();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config-dir" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--config-dir requires a value".to_string());
                };
                config_dir = Some(value.clone());
                index += 2;
            }
            "--plugin-root" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--plugin-root requires a value".to_string());
                };
                plugin_root = Some(value.clone());
                index += 2;
            }
            "--count" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--count requires a value".to_string());
                };
                count = Some(parse_u64(value, "--count")? as usize);
                index += 2;
            }
            "--profile" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--profile requires a value".to_string());
                };
                if !matches!(value.as_str(), "small" | "medium" | "large") {
                    return Err("--profile must be one of: small, medium, large".to_string());
                }
                profile = value.clone();
                index += 2;
            }
            other => {
                return Err(format!(
                    "unknown argument for prepare-scale-fixture: {other}"
                ));
            }
        }
    }

    let config_dir = config_dir.ok_or_else(|| "--config-dir is required".to_string())?;
    let plugin_root = plugin_root.ok_or_else(|| "--plugin-root is required".to_string())?;
    let count = count.ok_or_else(|| "--count is required".to_string())?;

    fs::create_dir_all(&config_dir)
        .map_err(|error| format!("failed creating config dir {config_dir}: {error}"))?;
    fs::create_dir_all(&plugin_root)
        .map_err(|error| format!("failed creating plugin root {plugin_root}: {error}"))?;

    for entry in fs::read_dir(&plugin_root)
        .map_err(|error| format!("failed reading plugin root {plugin_root}: {error}"))?
    {
        let entry = entry.map_err(|error| format!("failed reading plugin root entry: {error}"))?;
        if entry
            .file_type()
            .map_err(|error| format!("failed checking plugin root entry type: {error}"))?
            .is_dir()
        {
            fs::remove_dir_all(entry.path())
                .map_err(|error| format!("failed clearing fixture dir: {error}"))?;
        }
    }

    for idx in 0..count {
        let dir_name = format!("perf-plugin-{idx:04}");
        let plugin_dir = Path::new(&plugin_root).join(&dir_name);
        fs::create_dir_all(&plugin_dir)
            .map_err(|error| format!("failed creating fixture plugin dir: {error}"))?;
        let plugin_toml = plugin_dir.join("plugin.toml");
        let plugin_id = format!("bench.synthetic.{idx:04}");
        let content = synthetic_plugin_manifest(&profile, &plugin_id, idx);
        fs::write(&plugin_toml, content).map_err(|error| {
            format!(
                "failed writing fixture manifest {}: {error}",
                plugin_toml.display()
            )
        })?;
    }

    let config_file = Path::new(&config_dir).join("bmux.toml");
    let escaped_root = plugin_root.replace('\\', "\\\\");
    let config =
        format!("[plugins]\nsearch_paths = [\"{escaped_root}\"]\nenabled = []\ndisabled = []\n");
    fs::write(&config_file, config)
        .map_err(|error| format!("failed writing config {}: {error}", config_file.display()))?;

    println!(
        "prepared synthetic plugin fixture: profile={} count={} plugin_root={} config={}",
        profile,
        count,
        plugin_root,
        config_file.display()
    );

    Ok(())
}

fn synthetic_plugin_manifest(profile: &str, plugin_id: &str, idx: usize) -> String {
    let mut out = String::new();
    out.push_str("execution_class = \"native_fast\"\n");
    out.push_str(&format!("id = \"{plugin_id}\"\n"));
    out.push_str(&format!("name = \"Synthetic Perf Plugin {idx:04}\"\n"));
    out.push_str("version = \"0.0.0\"\n");

    match profile {
        "small" => {
            if idx.is_multiple_of(2) {
                out.push_str("provided_capabilities = [\"bench.synthetic.read\"]\n");
            }
        }
        "medium" => {
            out.push_str("required_capabilities = [\"bmux.commands\"]\n");
            out.push_str("provided_capabilities = [\"bench.synthetic.read\"]\n");
            out.push_str("\n[[commands]]\n");
            out.push_str("name = \"bench-status\"\n");
            out.push_str("path = [\"bench\", \"status\"]\n");
            out.push_str("summary = \"Synthetic bench status command\"\n");
            out.push_str("expose_in_cli = false\n");
        }
        "large" => {
            out.push_str("required_capabilities = [\"bmux.commands\", \"bmux.contexts.read\"]\n");
            out.push_str(
                "provided_capabilities = [\"bench.synthetic.read\", \"bench.synthetic.write\"]\n",
            );
            out.push_str("owns_namespaces = [\"bench\", \"synthetic\"]\n");
            out.push_str("\n[[commands]]\n");
            out.push_str("name = \"bench-status\"\n");
            out.push_str("path = [\"bench\", \"status\"]\n");
            out.push_str("summary = \"Synthetic bench status command\"\n");
            out.push_str("expose_in_cli = false\n");
            out.push_str("\n[[commands]]\n");
            out.push_str("name = \"bench-run\"\n");
            out.push_str("path = [\"bench\", \"run\"]\n");
            out.push_str("summary = \"Synthetic bench run command\"\n");
            out.push_str("expose_in_cli = false\n");
        }
        _ => {}
    }

    out
}

fn run_compare_report(args: Vec<String>) -> Result<(), String> {
    let mut baseline = None;
    let mut candidates = Vec::new();
    let mut warn_regression_ms = 10.0_f64;
    let mut json_output = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--baseline" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--baseline requires a value".to_string());
                };
                baseline = Some(value.clone());
                index += 2;
            }
            "--candidate" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--candidate requires a value".to_string());
                };
                candidates.push(value.clone());
                index += 2;
            }
            "--warn-regression-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--warn-regression-ms requires a value".to_string());
                };
                warn_regression_ms = parse_u64(value, "--warn-regression-ms")? as f64;
                index += 2;
            }
            "--json-output" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--json-output requires a value".to_string());
                };
                json_output = Some(value.clone());
                index += 2;
            }
            other => return Err(format!("unknown argument for compare-report: {other}")),
        }
    }

    let baseline = baseline.ok_or_else(|| "--baseline is required".to_string())?;
    if candidates.is_empty() {
        return Err("at least one --candidate is required".to_string());
    }

    let baseline_json = read_json_file(&baseline)?;
    let baseline_latency = parse_report_latency_summary(&baseline_json)?;

    let mut candidate_summaries = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        let candidate_json = read_json_file(candidate)?;
        candidate_summaries.push(parse_report_latency_summary(&candidate_json)?);
    }

    let mut metric_summaries = vec![
        print_compare_summary(
            "startup_ms",
            baseline_latency.startup_ms,
            candidate_summaries
                .iter()
                .map(|summary| summary.startup_ms)
                .collect(),
            warn_regression_ms,
        ),
        print_compare_summary(
            "p95_ms",
            baseline_latency.p95_ms,
            candidate_summaries
                .iter()
                .map(|summary| summary.p95_ms)
                .collect(),
            warn_regression_ms,
        ),
        print_compare_summary(
            "p99_ms",
            baseline_latency.p99_ms,
            candidate_summaries
                .iter()
                .map(|summary| summary.p99_ms)
                .collect(),
            warn_regression_ms,
        ),
        print_compare_summary(
            "avg_ms",
            baseline_latency.avg_ms,
            candidate_summaries
                .iter()
                .map(|summary| summary.avg_ms)
                .collect(),
            warn_regression_ms,
        ),
    ];

    if let Some(steady_p95) = baseline_latency.steady_p95_ms {
        if candidate_summaries
            .iter()
            .all(|summary| summary.steady_p95_ms.is_some())
        {
            let steady_candidates = candidate_summaries
                .iter()
                .filter_map(|summary| summary.steady_p95_ms)
                .collect::<Vec<_>>();
            metric_summaries.push(print_compare_summary(
                "steady_p95_ms",
                steady_p95,
                steady_candidates,
                warn_regression_ms,
            ));
        } else {
            println!(
                "compare steady_p95_ms skipped: one or more candidates missing steady_state_ms.p95"
            );
        }
    }

    if let Some(steady_p99) = baseline_latency.steady_p99_ms {
        if candidate_summaries
            .iter()
            .all(|summary| summary.steady_p99_ms.is_some())
        {
            let steady_candidates = candidate_summaries
                .iter()
                .filter_map(|summary| summary.steady_p99_ms)
                .collect::<Vec<_>>();
            metric_summaries.push(print_compare_summary(
                "steady_p99_ms",
                steady_p99,
                steady_candidates,
                warn_regression_ms,
            ));
        } else {
            println!(
                "compare steady_p99_ms skipped: one or more candidates missing steady_state_ms.p99"
            );
        }
    }

    if let Some(steady_avg) = baseline_latency.steady_avg_ms {
        if candidate_summaries
            .iter()
            .all(|summary| summary.steady_avg_ms.is_some())
        {
            let steady_candidates = candidate_summaries
                .iter()
                .filter_map(|summary| summary.steady_avg_ms)
                .collect::<Vec<_>>();
            metric_summaries.push(print_compare_summary(
                "steady_avg_ms",
                steady_avg,
                steady_candidates,
                warn_regression_ms,
            ));
        } else {
            println!(
                "compare steady_avg_ms skipped: one or more candidates missing steady_state_ms.avg"
            );
        }
    }

    if let Some(output_path) = json_output {
        let report = json!({
            "baseline": baseline,
            "candidate_count": candidates.len(),
            "warn_regression_ms": warn_regression_ms,
            "metrics": metric_summaries.iter().map(CompareMetricSummary::to_json).collect::<Vec<_>>(),
        });
        if let Some(parent) = Path::new(&output_path).parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed creating compare report parent {}: {error}",
                    parent.display()
                )
            })?;
        }
        let encoded = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("failed encoding compare report json: {error}"))?;
        fs::write(&output_path, encoded).map_err(|error| {
            format!("failed writing compare report json {output_path}: {error}")
        })?;
    }

    Ok(())
}

fn run_discover_run_candidate(args: Vec<String>) -> Result<(), String> {
    let mut bmux_bin = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--bmux-bin" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--bmux-bin requires a value".to_string());
                };
                bmux_bin = Some(value.clone());
                index += 2;
            }
            other => {
                return Err(format!(
                    "unknown argument for discover-run-candidate: {other}"
                ));
            }
        }
    }

    let bmux_bin = bmux_bin.ok_or_else(|| "--bmux-bin is required".to_string())?;
    if !Path::new(&bmux_bin).exists() {
        return Err(format!("bmux binary does not exist: {bmux_bin}"));
    }

    let list_output = Command::new(&bmux_bin)
        .args(["plugin", "list", "--json"])
        .output()
        .map_err(|error| format!("failed running plugin list json: {error}"))?;
    if !list_output.status.success() {
        return Err("bmux plugin list --json failed".to_string());
    }

    let payload: Value = serde_json::from_slice(&list_output.stdout)
        .map_err(|error| format!("failed parsing plugin list json: {error}"))?;
    let plugins = payload
        .as_array()
        .ok_or_else(|| "plugin list json root must be an array".to_string())?;

    for plugin in plugins {
        let Some(plugin_id) = plugin.get("id").and_then(Value::as_str) else {
            continue;
        };
        if plugin_id == "bmux.plugin_cli" {
            continue;
        }
        let Some(commands) = plugin.get("commands").and_then(Value::as_array) else {
            continue;
        };
        for command in commands {
            let Some(command_name) = command.as_str() else {
                continue;
            };
            let status = Command::new(&bmux_bin)
                .args(["plugin", "run", plugin_id, command_name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|error| {
                    format!(
                        "failed probing plugin run candidate {plugin_id}:{command_name}: {error}"
                    )
                })?;
            if status.success() {
                println!("{plugin_id}");
                println!("{command_name}");
                return Ok(());
            }
        }
    }

    Err("no successful plugin run candidate discovered".to_string())
}

fn count_occurrences(haystack: &str, needle: &str) -> u64 {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count() as u64
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RuntimeFaultCounts {
    retries: u64,
    respawns: u64,
    timeouts: u64,
}

fn count_runtime_faults(stderr: &str) -> RuntimeFaultCounts {
    let json_counts = count_runtime_faults_from_json_markers(stderr);
    if json_counts.retries + json_counts.respawns + json_counts.timeouts > 0 {
        return json_counts;
    }

    let token_retries = count_occurrences(stderr, "[bmux-runtime-fault:persistent-retry]");
    let token_respawns = count_occurrences(stderr, "[bmux-runtime-fault:persistent-respawn]");
    let token_timeouts = count_occurrences(stderr, "[bmux-runtime-fault:persistent-timeout]")
        + count_occurrences(stderr, "[bmux-runtime-fault:one-shot-timeout]");

    if token_retries + token_respawns + token_timeouts > 0 {
        return RuntimeFaultCounts {
            retries: token_retries,
            respawns: token_respawns,
            timeouts: token_timeouts,
        };
    }

    RuntimeFaultCounts {
        retries: count_occurrences(
            stderr,
            "persistent process worker write failed; recycling worker",
        ) + count_occurrences(
            stderr,
            "persistent process worker read failed; recycling worker",
        ),
        respawns: count_occurrences(stderr, "persistent process worker exited; respawning"),
        timeouts: count_occurrences(
            stderr,
            "persistent process worker read timed out; recycling worker",
        ) + count_occurrences(stderr, "process runtime one-shot invocation timed out"),
    }
}

fn count_runtime_faults_from_json_markers(stderr: &str) -> RuntimeFaultCounts {
    let mut counts = RuntimeFaultCounts::default();
    for line in stderr.lines() {
        if let Some(payload) = extract_json_marker_payload(line) {
            let Ok(value) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            let Some(kind) = value.get("kind").and_then(Value::as_str) else {
                continue;
            };
            match kind {
                "persistent-retry" => counts.retries += 1,
                "persistent-respawn" => counts.respawns += 1,
                "persistent-timeout" | "one-shot-timeout" => counts.timeouts += 1,
                _ => {}
            }
        }
    }
    counts
}

fn extract_json_marker_payload(line: &str) -> Option<&str> {
    const PREFIX: &str = "[bmux-runtime-fault-json]";
    let marker_index = line.find(PREFIX)?;
    let after_prefix = &line[marker_index + PREFIX.len()..];
    if !after_prefix.starts_with('{') {
        return None;
    }

    let mut depth = 0_u32;
    for (idx, ch) in after_prefix.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return Some(&after_prefix[..=idx]);
                }
            }
            _ => {}
        }
    }

    None
}

#[derive(Debug, Clone, Copy)]
struct ReportLatencySummary {
    startup_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    avg_ms: f64,
    steady_p95_ms: Option<f64>,
    steady_p99_ms: Option<f64>,
    steady_avg_ms: Option<f64>,
}

fn parse_report_latency_summary(payload: &Value) -> Result<ReportLatencySummary, String> {
    let startup_ms = payload
        .get("startup_ms")
        .and_then(Value::as_f64)
        .ok_or_else(|| "missing report startup_ms".to_string())?;
    let latency = payload
        .get("latency_ms")
        .and_then(Value::as_object)
        .ok_or_else(|| "missing report latency_ms".to_string())?;
    let p95_ms = latency
        .get("p95")
        .and_then(Value::as_f64)
        .ok_or_else(|| "missing report latency_ms.p95".to_string())?;
    let p99_ms = latency
        .get("p99")
        .and_then(Value::as_f64)
        .ok_or_else(|| "missing report latency_ms.p99".to_string())?;
    let avg_ms = latency
        .get("avg")
        .and_then(Value::as_f64)
        .ok_or_else(|| "missing report latency_ms.avg".to_string())?;

    let steady = payload.get("steady_state_ms").and_then(Value::as_object);
    let steady_p95_ms = steady
        .and_then(|value| value.get("p95"))
        .and_then(Value::as_f64);
    let steady_p99_ms = steady
        .and_then(|value| value.get("p99"))
        .and_then(Value::as_f64);
    let steady_avg_ms = steady
        .and_then(|value| value.get("avg"))
        .and_then(Value::as_f64);

    Ok(ReportLatencySummary {
        startup_ms,
        p95_ms,
        p99_ms,
        avg_ms,
        steady_p95_ms,
        steady_p99_ms,
        steady_avg_ms,
    })
}

fn print_compare_summary(
    label: &str,
    baseline: f64,
    candidates: Vec<f64>,
    warn_regression_ms: f64,
) -> CompareMetricSummary {
    let stats = compare_delta_stats(baseline, &candidates);
    let status = compare_status(stats, warn_regression_ms);
    let variance = classify_variance(stats, warn_regression_ms);
    println!(
        "compare {label} baseline={baseline:.3} runs={} delta_median={:+.3} delta_mean={:+.3} delta_min={:+.3} delta_max={:+.3} delta_stddev={:.3} status={status} variance={variance}",
        candidates.len(),
        stats.median,
        stats.mean,
        stats.min,
        stats.max,
        stats.stddev,
    );

    CompareMetricSummary {
        label: label.to_string(),
        baseline,
        runs: candidates.len(),
        delta_median: stats.median,
        delta_mean: stats.mean,
        delta_min: stats.min,
        delta_max: stats.max,
        delta_stddev: stats.stddev,
        status,
        variance: variance.to_string(),
    }
}

#[derive(Debug, Clone)]
struct CompareMetricSummary {
    label: String,
    baseline: f64,
    runs: usize,
    delta_median: f64,
    delta_mean: f64,
    delta_min: f64,
    delta_max: f64,
    delta_stddev: f64,
    status: &'static str,
    variance: String,
}

impl CompareMetricSummary {
    fn to_json(&self) -> Value {
        json!({
            "label": self.label,
            "baseline": self.baseline,
            "runs": self.runs,
            "delta_median": self.delta_median,
            "delta_mean": self.delta_mean,
            "delta_min": self.delta_min,
            "delta_max": self.delta_max,
            "delta_stddev": self.delta_stddev,
            "status": self.status,
            "variance": self.variance,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct DeltaStats {
    min: f64,
    max: f64,
    mean: f64,
    median: f64,
    stddev: f64,
}

fn compare_delta_stats(baseline: f64, candidates: &[f64]) -> DeltaStats {
    let mut deltas = candidates
        .iter()
        .map(|candidate| candidate - baseline)
        .collect::<Vec<_>>();
    deltas.sort_by(f64::total_cmp);

    let min = *deltas.first().expect("at least one candidate required");
    let max = *deltas.last().expect("at least one candidate required");
    let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
    let median = if deltas.len().is_multiple_of(2) {
        let upper = deltas.len() / 2;
        (deltas[upper - 1] + deltas[upper]) / 2.0
    } else {
        deltas[deltas.len() / 2]
    };
    let variance = deltas
        .iter()
        .map(|delta| {
            let distance = delta - mean;
            distance * distance
        })
        .sum::<f64>()
        / deltas.len() as f64;
    let stddev = variance.sqrt();

    DeltaStats {
        min,
        max,
        mean,
        median,
        stddev,
    }
}

fn classify_variance(stats: DeltaStats, warn_regression_ms: f64) -> &'static str {
    if stats.median > warn_regression_ms {
        if stats.stddev > warn_regression_ms / 2.0 {
            "likely_regression_with_variance"
        } else {
            "likely_regression"
        }
    } else if stats.max > warn_regression_ms {
        "likely_noise"
    } else {
        "stable"
    }
}

fn compare_status(stats: DeltaStats, warn_regression_ms: f64) -> &'static str {
    if stats.median > warn_regression_ms {
        "WARN"
    } else if stats.max > warn_regression_ms {
        "SPIKE"
    } else if stats.median < 0.0 {
        "IMPROVED"
    } else {
        "OK"
    }
}

fn parse_u64(value: &str, flag: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{flag} must be a non-negative integer"))
}

fn parse_f64(value: &str, flag: &str) -> Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| format!("{flag} must be a non-negative number"))?;
    if parsed.is_sign_negative() || !parsed.is_finite() {
        return Err(format!("{flag} must be a non-negative finite number"));
    }
    Ok(parsed)
}

fn parse_samples_ms(payload: &Value) -> Result<Vec<f64>, String> {
    let samples = payload
        .get("samples_ms")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing samples_ms in input".to_string())?;
    if samples.is_empty() {
        return Err("no samples collected".to_string());
    }

    samples
        .iter()
        .map(|value| {
            value
                .as_f64()
                .ok_or_else(|| "samples_ms contains non-number".to_string())
        })
        .collect::<Result<Vec<_>, _>>()
}

fn compute_latency_stats(values: &[f64]) -> LatencyStats {
    let mut sorted_values = values.to_vec();
    sorted_values.sort_by(f64::total_cmp);

    let min = sorted_values[0];
    let max = sorted_values[sorted_values.len() - 1];
    let p50 = percentile_nearest_rank(&sorted_values, 50.0);
    let p95 = percentile_nearest_rank(&sorted_values, 95.0);
    let p99 = percentile_nearest_rank(&sorted_values, 99.0);
    let avg = sorted_values.iter().sum::<f64>() / sorted_values.len() as f64;
    LatencyStats {
        min,
        p50,
        p95,
        p99,
        avg,
        max,
    }
}

fn compute_latency_breakdown(samples: &[f64]) -> LatencyBreakdown {
    let startup_ms = samples[0];
    let overall = compute_latency_stats(samples);
    let steady_state = if samples.len() > 1 {
        Some(compute_latency_stats(&samples[1..]))
    } else {
        None
    };
    LatencyBreakdown {
        startup_ms,
        overall,
        steady_state,
    }
}

fn stats_json(stats: LatencyStats) -> Value {
    json!({
        "min": stats.min,
        "p50": stats.p50,
        "p95": stats.p95,
        "p99": stats.p99,
        "avg": stats.avg,
        "max": stats.max,
    })
}

fn parse_runtime_fault_counts(payload: &Value) -> Result<RuntimeFaultCounts, String> {
    let runtime_faults = payload
        .get("runtime_faults")
        .and_then(Value::as_object)
        .ok_or_else(|| "missing runtime_faults in input".to_string())?;

    let retries = runtime_faults
        .get("retries")
        .and_then(Value::as_u64)
        .ok_or_else(|| "runtime_faults.retries missing or invalid".to_string())?;
    let respawns = runtime_faults
        .get("respawns")
        .and_then(Value::as_u64)
        .ok_or_else(|| "runtime_faults.respawns missing or invalid".to_string())?;
    let timeouts = runtime_faults
        .get("timeouts")
        .and_then(Value::as_u64)
        .ok_or_else(|| "runtime_faults.timeouts missing or invalid".to_string())?;
    Ok(RuntimeFaultCounts {
        retries,
        respawns,
        timeouts,
    })
}

fn evaluate_latency_thresholds(
    breakdown: LatencyBreakdown,
    limits: LatencyThresholds,
) -> Vec<String> {
    let stats = breakdown.overall;
    let mut violations = Vec::new();
    if let Some(limit) = limits.max_p99_ms
        && stats.p99 > limit
    {
        violations.push(format!("p99 {:.3} > {:.0}", stats.p99, limit));
    }
    if let Some(limit) = limits.max_p95_ms
        && stats.p95 > limit
    {
        violations.push(format!("p95 {:.3} > {:.0}", stats.p95, limit));
    }
    if let Some(limit) = limits.max_avg_ms
        && stats.avg > limit
    {
        violations.push(format!("avg {:.3} > {:.0}", stats.avg, limit));
    }
    if let Some(steady) = breakdown.steady_state {
        if let Some(limit) = limits.max_steady_p99_ms
            && steady.p99 > limit
        {
            violations.push(format!("steady_p99 {:.3} > {:.0}", steady.p99, limit));
        }
        if let Some(limit) = limits.max_steady_p95_ms
            && steady.p95 > limit
        {
            violations.push(format!("steady_p95 {:.3} > {:.0}", steady.p95, limit));
        }
        if let Some(limit) = limits.max_steady_avg_ms
            && steady.avg > limit
        {
            violations.push(format!("steady_avg {:.3} > {:.0}", steady.avg, limit));
        }
    }
    violations
}

fn evaluate_fault_thresholds(
    faults: RuntimeFaultCounts,
    limits: RuntimeFaultThresholds,
) -> Vec<String> {
    let mut violations = Vec::new();
    if let Some(limit) = limits.max_runtime_retries
        && faults.retries > limit
    {
        violations.push(format!("retries {} > {}", faults.retries, limit));
    }
    if let Some(limit) = limits.max_runtime_respawns
        && faults.respawns > limit
    {
        violations.push(format!("respawns {} > {}", faults.respawns, limit));
    }
    if let Some(limit) = limits.max_runtime_timeouts
        && faults.timeouts > limit
    {
        violations.push(format!("timeouts {} > {}", faults.timeouts, limit));
    }
    violations
}

fn read_json_file(path: &str) -> Result<Value, String> {
    let bytes = fs::read(path).map_err(|error| format!("failed reading {path}: {error}"))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("failed parsing json from {path}: {error}"))
}

fn merge_sample_events(path: &str, mut additional_events: Vec<Value>) -> Result<(), String> {
    let mut payload = read_json_file(path)?;
    let events = payload
        .get_mut("events")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("sample json {path} missing events array"))?;
    events.append(&mut additional_events);
    let encoded = serde_json::to_vec_pretty(&payload)
        .map_err(|error| format!("failed encoding merged sample json: {error}"))?;
    fs::write(path, encoded).map_err(|error| format!("failed writing merged sample json: {error}"))
}

fn percentile_nearest_rank(values: &[f64], percentile: f64) -> f64 {
    let rank = ((percentile / 100.0) * values.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(values.len() - 1);
    values[index]
}

#[cfg(test)]
mod tests {
    use super::{
        LatencyBreakdown, LatencyStats, LatencyThresholds, RuntimeFaultCounts,
        RuntimeFaultThresholds, classify_variance, compare_delta_stats, count_occurrences,
        count_runtime_faults, evaluate_fault_thresholds, evaluate_latency_thresholds,
        parse_report_latency_summary, parse_u64, percentile_nearest_rank,
        synthetic_plugin_manifest,
    };
    use serde_json::json;

    #[test]
    fn count_occurrences_counts_multiple_matches() {
        let haystack = "retry retry respawn retry";
        assert_eq!(count_occurrences(haystack, "retry"), 3);
        assert_eq!(count_occurrences(haystack, "respawn"), 1);
    }

    #[test]
    fn count_occurrences_empty_needle_is_zero() {
        assert_eq!(count_occurrences("abc", ""), 0);
    }

    #[test]
    fn percentile_nearest_rank_matches_expected_points() {
        let values = [10.0, 20.0, 30.0, 40.0, 50.0];
        assert_eq!(percentile_nearest_rank(&values, 50.0), 30.0);
        assert_eq!(percentile_nearest_rank(&values, 95.0), 50.0);
        assert_eq!(percentile_nearest_rank(&values, 99.0), 50.0);
    }

    #[test]
    fn parse_u64_rejects_non_numeric_values() {
        let error = parse_u64("abc", "--iterations").expect_err("parse should fail");
        assert!(error.contains("--iterations must be a non-negative integer"));
    }

    #[test]
    fn count_runtime_faults_prefers_structured_tokens() {
        let stderr = "[bmux-runtime-fault:persistent-retry]\n[bmux-runtime-fault:persistent-respawn]\n[bmux-runtime-fault:one-shot-timeout]\n";
        assert_eq!(
            count_runtime_faults(stderr),
            RuntimeFaultCounts {
                retries: 1,
                respawns: 1,
                timeouts: 1
            }
        );
    }

    #[test]
    fn count_runtime_faults_prefers_json_markers() {
        let stderr = "INFO [bmux-runtime-fault-json]{\"kind\":\"persistent-retry\"} details\nWARN [bmux-runtime-fault-json]{\"kind\":\"persistent-timeout\"} details\n";
        assert_eq!(
            count_runtime_faults(stderr),
            RuntimeFaultCounts {
                retries: 1,
                respawns: 0,
                timeouts: 1
            }
        );
    }

    #[test]
    fn count_runtime_faults_supports_legacy_messages() {
        let stderr = "persistent process worker write failed; recycling worker\nprocess runtime one-shot invocation timed out\n";
        assert_eq!(
            count_runtime_faults(stderr),
            RuntimeFaultCounts {
                retries: 1,
                respawns: 0,
                timeouts: 1
            }
        );
    }

    #[test]
    fn evaluate_threshold_helpers_emit_expected_violations() {
        let latency = evaluate_latency_thresholds(
            LatencyBreakdown {
                startup_ms: 10.0,
                overall: LatencyStats {
                    min: 1.0,
                    p50: 2.0,
                    p95: 10.0,
                    p99: 20.0,
                    avg: 5.0,
                    max: 20.0,
                },
                steady_state: Some(LatencyStats {
                    min: 1.0,
                    p50: 2.0,
                    p95: 9.0,
                    p99: 11.0,
                    avg: 4.5,
                    max: 11.0,
                }),
            },
            LatencyThresholds {
                max_p95_ms: Some(9.0),
                max_p99_ms: Some(19.0),
                max_avg_ms: Some(4.0),
                max_steady_p95_ms: Some(8.0),
                max_steady_p99_ms: Some(10.0),
                max_steady_avg_ms: Some(4.0),
            },
        );
        assert_eq!(latency.len(), 6);

        let faults = evaluate_fault_thresholds(
            RuntimeFaultCounts {
                retries: 2,
                respawns: 1,
                timeouts: 3,
            },
            RuntimeFaultThresholds {
                max_runtime_retries: Some(1),
                max_runtime_respawns: Some(0),
                max_runtime_timeouts: Some(2),
            },
        );
        assert_eq!(faults.len(), 3);
    }

    #[test]
    fn parse_report_latency_summary_reads_required_fields() {
        let payload = json!({
            "startup_ms": 111.0,
            "latency_ms": {
                "p95": 20.0,
                "p99": 30.0,
                "avg": 15.0,
            }
        });
        let summary = parse_report_latency_summary(&payload).expect("summary should parse");
        assert_eq!(summary.startup_ms, 111.0);
        assert_eq!(summary.p95_ms, 20.0);
        assert_eq!(summary.p99_ms, 30.0);
        assert_eq!(summary.avg_ms, 15.0);
        assert_eq!(summary.steady_p95_ms, None);
        assert_eq!(summary.steady_p99_ms, None);
        assert_eq!(summary.steady_avg_ms, None);
    }

    #[test]
    fn compare_delta_stats_computes_median_and_stddev() {
        let stats = compare_delta_stats(100.0, &[110.0, 95.0, 105.0]);
        assert_eq!(stats.min, -5.0);
        assert_eq!(stats.max, 10.0);
        assert_eq!(stats.median, 5.0);
        assert!(stats.stddev > 0.0);
    }

    #[test]
    fn classify_variance_distinguishes_noise_and_regression() {
        let stable = classify_variance(
            super::DeltaStats {
                min: -1.0,
                max: 2.0,
                mean: 0.5,
                median: 0.5,
                stddev: 0.8,
            },
            5.0,
        );
        assert_eq!(stable, "stable");

        let noise = classify_variance(
            super::DeltaStats {
                min: -1.0,
                max: 8.0,
                mean: 2.0,
                median: 2.0,
                stddev: 3.0,
            },
            5.0,
        );
        assert_eq!(noise, "likely_noise");

        let regression = classify_variance(
            super::DeltaStats {
                min: 6.0,
                max: 8.0,
                mean: 7.0,
                median: 7.0,
                stddev: 0.5,
            },
            5.0,
        );
        assert_eq!(regression, "likely_regression");
    }

    #[test]
    fn synthetic_plugin_manifest_profile_shapes_are_distinct() {
        let small = synthetic_plugin_manifest("small", "bench.synthetic.0001", 1);
        let medium = synthetic_plugin_manifest("medium", "bench.synthetic.0001", 1);
        let large = synthetic_plugin_manifest("large", "bench.synthetic.0001", 1);

        assert!(small.contains("execution_class = \"native_fast\""));
        assert!(!small.contains("[[commands]]"));
        assert!(medium.contains("[[commands]]"));
        assert!(large.contains("owns_namespaces"));
        assert!(large.matches("[[commands]]").count() >= 2);
    }
}
