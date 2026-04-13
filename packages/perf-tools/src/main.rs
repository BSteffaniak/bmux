use serde_json::{Value, json};
use std::env;
use std::fs;
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::time::Instant;

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
  prepare-scale-fixture --config-dir PATH --plugin-root PATH --count N [--profile small|medium|large]
  compare-report --baseline PATH --candidate PATH [--candidate PATH ...] [--warn-regression-ms N]
  discover-run-candidate --bmux-bin PATH"
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

    let mut samples_ms = Vec::with_capacity(iterations);
    let mut retries = 0_u64;
    let mut respawns = 0_u64;
    let mut timeouts = 0_u64;

    let command_name = &command[0];
    let command_args = &command[1..];

    for _ in 0..iterations {
        let started = Instant::now();
        let output = Command::new(command_name)
            .args(command_args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("failed executing sampled command: {error}"))?;
        let elapsed = started.elapsed();
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;

        if !allow_nonzero && !output.status.success() {
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
        samples_ms.push(elapsed_ms);
    }

    let payload = json!({
        "samples_ms": samples_ms,
        "runtime_faults": {
            "retries": retries,
            "respawns": respawns,
            "timeouts": timeouts,
        }
    });
    let encoded = serde_json::to_vec_pretty(&payload)
        .map_err(|error| format!("failed encoding sample json: {error}"))?;
    fs::write(out_json, encoded).map_err(|error| format!("failed writing sample json: {error}"))?;
    Ok(())
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

    print_compare_summary(
        "startup_ms",
        baseline_latency.startup_ms,
        candidate_summaries
            .iter()
            .map(|summary| summary.startup_ms)
            .collect(),
        warn_regression_ms,
    );
    print_compare_summary(
        "p95_ms",
        baseline_latency.p95_ms,
        candidate_summaries
            .iter()
            .map(|summary| summary.p95_ms)
            .collect(),
        warn_regression_ms,
    );
    print_compare_summary(
        "p99_ms",
        baseline_latency.p99_ms,
        candidate_summaries
            .iter()
            .map(|summary| summary.p99_ms)
            .collect(),
        warn_regression_ms,
    );
    print_compare_summary(
        "avg_ms",
        baseline_latency.avg_ms,
        candidate_summaries
            .iter()
            .map(|summary| summary.avg_ms)
            .collect(),
        warn_regression_ms,
    );

    if let Some(steady_p95) = baseline_latency.steady_p95_ms {
        if candidate_summaries
            .iter()
            .all(|summary| summary.steady_p95_ms.is_some())
        {
            let steady_candidates = candidate_summaries
                .iter()
                .filter_map(|summary| summary.steady_p95_ms)
                .collect::<Vec<_>>();
            print_compare_summary(
                "steady_p95_ms",
                steady_p95,
                steady_candidates,
                warn_regression_ms,
            );
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
            print_compare_summary(
                "steady_p99_ms",
                steady_p99,
                steady_candidates,
                warn_regression_ms,
            );
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
            print_compare_summary(
                "steady_avg_ms",
                steady_avg,
                steady_candidates,
                warn_regression_ms,
            );
        } else {
            println!(
                "compare steady_avg_ms skipped: one or more candidates missing steady_state_ms.avg"
            );
        }
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
) {
    let stats = compare_delta_stats(baseline, &candidates);
    let status = if stats.median > warn_regression_ms {
        "WARN"
    } else if stats.max > warn_regression_ms {
        "SPIKE"
    } else if stats.median < 0.0 {
        "IMPROVED"
    } else {
        "OK"
    };
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

fn parse_u64(value: &str, flag: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{flag} must be a non-negative integer"))
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
