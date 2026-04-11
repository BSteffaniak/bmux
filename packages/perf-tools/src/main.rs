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
  report-latency --input PATH [--max-p95-ms N] [--max-p99-ms N] [--max-avg-ms N]
  report-faults --input PATH [--max-runtime-retries N] [--max-runtime-respawns N] [--max-runtime-timeouts N]
  discover-run-candidate --bmux-bin PATH"
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
        retries += count_occurrences(
            &stderr,
            "persistent process worker write failed; recycling worker",
        );
        retries += count_occurrences(
            &stderr,
            "persistent process worker read failed; recycling worker",
        );
        respawns += count_occurrences(&stderr, "persistent process worker exited; respawning");
        timeouts += count_occurrences(
            &stderr,
            "persistent process worker read timed out; recycling worker",
        );
        timeouts += count_occurrences(&stderr, "process runtime one-shot invocation timed out");
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
            other => {
                return Err(format!("unknown argument for report-latency: {other}"));
            }
        }
    }

    let input = input.ok_or_else(|| "--input is required".to_string())?;
    let payload = read_json_file(&input)?;
    let samples = payload
        .get("samples_ms")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing samples_ms in input".to_string())?;
    if samples.is_empty() {
        return Err("no samples collected".to_string());
    }

    let mut values = samples
        .iter()
        .map(|value| {
            value
                .as_f64()
                .ok_or_else(|| "samples_ms contains non-number".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    values.sort_by(f64::total_cmp);

    let min_v = values[0];
    let max_v = values[values.len() - 1];
    let p50 = percentile_nearest_rank(&values, 50.0);
    let p95 = percentile_nearest_rank(&values, 95.0);
    let p99 = percentile_nearest_rank(&values, 99.0);
    let avg = values.iter().sum::<f64>() / values.len() as f64;

    println!(
        "latency_ms min={min_v:.3} p50={p50:.3} p95={p95:.3} p99={p99:.3} avg={avg:.3} max={max_v:.3}"
    );

    let mut violations = Vec::new();
    if let Some(limit) = max_p99_ms
        && p99 > limit
    {
        violations.push(format!("p99 {p99:.3} > {limit:.0}"));
    }
    if let Some(limit) = max_p95_ms
        && p95 > limit
    {
        violations.push(format!("p95 {p95:.3} > {limit:.0}"));
    }
    if let Some(limit) = max_avg_ms
        && avg > limit
    {
        violations.push(format!("avg {avg:.3} > {limit:.0}"));
    }

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

    println!("runtime_faults retries={retries} respawns={respawns} timeouts={timeouts}");

    let mut violations = Vec::new();
    if let Some(limit) = max_runtime_retries
        && retries > limit
    {
        violations.push(format!("retries {retries} > {limit}"));
    }
    if let Some(limit) = max_runtime_respawns
        && respawns > limit
    {
        violations.push(format!("respawns {respawns} > {limit}"));
    }
    if let Some(limit) = max_runtime_timeouts
        && timeouts > limit
    {
        violations.push(format!("timeouts {timeouts} > {limit}"));
    }

    if !violations.is_empty() {
        return Err(format!(
            "runtime fault check failed: {}",
            violations.join("; ")
        ));
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

fn parse_u64(value: &str, flag: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{flag} must be a non-negative integer"))
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
