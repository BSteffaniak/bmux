use std::collections::BTreeMap;
use std::env;
use std::fs;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct BenchmarkManifest {
    pub(crate) benchmark: BenchmarkSpec,
    #[serde(default)]
    pub(crate) defaults: BenchmarkDefaults,
    #[serde(default)]
    pub(crate) profiles: BTreeMap<String, BenchmarkProfile>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BenchmarkSpec {
    pub(crate) name: String,
    pub(crate) kind: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct BenchmarkDefaults {
    pub(crate) iterations: Option<usize>,
    pub(crate) warmup: Option<usize>,
    pub(crate) scenario: Option<String>,
    pub(crate) windows: Option<usize>,
    pub(crate) switches: Option<usize>,
    pub(crate) max_p99_ms: Option<f64>,
    pub(crate) attach_command_limit_ms: Option<f64>,
    pub(crate) retarget_limit_ms: Option<f64>,
    pub(crate) core_service_limit_ms: Option<f64>,
    pub(crate) static_service_limit_ms: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct BenchmarkProfile {
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) service_timing: bool,
    #[serde(default)]
    pub(crate) ipc_timing: bool,
    #[serde(default)]
    pub(crate) storage_timing: bool,
    #[serde(default)]
    pub(crate) loosen_slo: bool,
}

#[derive(Debug, Default)]
pub(crate) struct BenchmarkRunOptions {
    pub(crate) manifest: Option<String>,
    pub(crate) profile: String,
    pub(crate) artifact_json: Option<String>,
    pub(crate) phase_report_dir: Option<String>,
    pub(crate) bmux_bin: Option<String>,
    pub(crate) iterations: Option<usize>,
    pub(crate) warmup: Option<usize>,
    pub(crate) scenario: Option<String>,
    pub(crate) windows: Option<usize>,
    pub(crate) switches: Option<usize>,
    pub(crate) max_p99_ms: Option<f64>,
    pub(crate) tags: Vec<String>,
    pub(crate) limits: BTreeMap<String, f64>,
    pub(crate) vars: BTreeMap<String, String>,
}

#[derive(Debug)]
pub(crate) struct BenchmarkResolvedOptions {
    pub(crate) manifest_path: String,
    pub(crate) profile: String,
    pub(crate) artifact_json: Option<String>,
    pub(crate) phase_report_dir: String,
    pub(crate) bmux_bin: Option<String>,
    pub(crate) iterations: usize,
    pub(crate) warmup: usize,
    pub(crate) scenario: String,
    pub(crate) windows: usize,
    pub(crate) switches: usize,
    pub(crate) max_p99_ms: Option<f64>,
    pub(crate) tags: Vec<String>,
    pub(crate) limits: BTreeMap<String, f64>,
    pub(crate) vars: BTreeMap<String, String>,
    pub(crate) service_timing: bool,
    pub(crate) ipc_timing: bool,
    pub(crate) storage_timing: bool,
    pub(crate) loosen_slo: bool,
}

pub(crate) fn read_manifest(path: &str) -> Result<BenchmarkManifest, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("failed reading benchmark manifest {path}: {error}"))?;
    toml::from_str(&text).map_err(|error| format!("failed parsing benchmark manifest: {error}"))
}

pub(crate) fn resolve_benchmark_options(
    cli: BenchmarkRunOptions,
    manifest: &BenchmarkManifest,
    manifest_path: String,
) -> Result<BenchmarkResolvedOptions, String> {
    let profile = manifest
        .profiles
        .get(&cli.profile)
        .ok_or_else(|| format!("profile '{}' not found in manifest", cli.profile))?;
    let phase_report_dir = cli.phase_report_dir.unwrap_or_else(|| {
        env::temp_dir()
            .join(format!("bmux-{}-phase-reports", manifest.benchmark.name))
            .display()
            .to_string()
    });
    let mut tags = profile.tags.clone();
    tags.extend(cli.tags);
    let mut limits = cli.limits;
    insert_default_limit(
        &mut limits,
        "attach_command",
        manifest.defaults.attach_command_limit_ms,
    );
    insert_default_limit(&mut limits, "retarget", manifest.defaults.retarget_limit_ms);
    insert_default_limit(
        &mut limits,
        "core_service",
        manifest.defaults.core_service_limit_ms,
    );
    insert_default_limit(
        &mut limits,
        "static_service",
        manifest.defaults.static_service_limit_ms,
    );
    if profile.loosen_slo {
        for value in limits.values_mut() {
            *value = 1_000_000.0;
        }
    }
    Ok(BenchmarkResolvedOptions {
        manifest_path,
        profile: cli.profile,
        artifact_json: cli.artifact_json,
        phase_report_dir,
        bmux_bin: cli.bmux_bin,
        iterations: cli
            .iterations
            .or(manifest.defaults.iterations)
            .unwrap_or(30),
        warmup: cli.warmup.or(manifest.defaults.warmup).unwrap_or(0),
        scenario: cli
            .scenario
            .or_else(|| manifest.defaults.scenario.clone())
            .unwrap_or_else(|| "default".to_string()),
        windows: cli.windows.or(manifest.defaults.windows).unwrap_or(4),
        switches: cli.switches.or(manifest.defaults.switches).unwrap_or(4),
        max_p99_ms: cli.max_p99_ms.or(manifest.defaults.max_p99_ms),
        tags,
        limits,
        vars: cli.vars,
        service_timing: profile.service_timing,
        ipc_timing: profile.ipc_timing,
        storage_timing: profile.storage_timing,
        loosen_slo: profile.loosen_slo,
    })
}

fn insert_default_limit(limits: &mut BTreeMap<String, f64>, name: &str, value: Option<f64>) {
    if let Some(value) = value {
        limits.entry(name.to_string()).or_insert(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_profile_enables_timing_and_relaxes_limits() {
        let manifest: BenchmarkManifest = toml::from_str(
            r#"
[benchmark]
name = "example"
kind = "core-services"

[defaults]
iterations = 10
core_service_limit_ms = 1

[profiles.diagnostic]
tags = ["service"]
service_timing = true
ipc_timing = true
storage_timing = true
loosen_slo = true
"#,
        )
        .unwrap();
        let options = resolve_benchmark_options(
            BenchmarkRunOptions {
                profile: "diagnostic".to_string(),
                ..BenchmarkRunOptions::default()
            },
            &manifest,
            "perf/example.toml".to_string(),
        )
        .unwrap();
        assert!(options.service_timing);
        assert!(options.ipc_timing);
        assert!(options.storage_timing);
        assert_eq!(options.tags, vec!["service"]);
        assert_eq!(options.limits.get("core_service"), Some(&1_000_000.0));
    }

    #[test]
    fn cli_limit_overrides_manifest_default() {
        let manifest: BenchmarkManifest = toml::from_str(
            r#"
[benchmark]
name = "example"
kind = "core-services"

[defaults]
core_service_limit_ms = 1

[profiles.normal]
"#,
        )
        .unwrap();
        let mut limits = BTreeMap::new();
        limits.insert("core_service".to_string(), 0.5);
        let options = resolve_benchmark_options(
            BenchmarkRunOptions {
                profile: "normal".to_string(),
                limits,
                ..BenchmarkRunOptions::default()
            },
            &manifest,
            "perf/example.toml".to_string(),
        )
        .unwrap();
        assert_eq!(options.limits.get("core_service"), Some(&0.5));
    }
}
