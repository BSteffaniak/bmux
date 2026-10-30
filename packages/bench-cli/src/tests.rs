use super::{
    args::{Cli, Command, ReportFormat, RunArgs},
    human_summary, resolve_run, run_from, safe,
};
use bmux_bench::{
    artifact, config,
    model::{CaseOutcome, RunRecord, SCHEMA_VERSION, Sample, Status},
    workload,
};
use clap::Parser;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn run_args(options: &[&str]) -> RunArgs {
    let cli = Cli::try_parse_from(
        ["bmux-bench", "run"]
            .into_iter()
            .chain(options.iter().copied()),
    )
    .unwrap();
    let Command::Run(args) = cli.command else {
        panic!("expected run")
    };
    args
}

#[test]
fn default_profile_and_flags_preserve_profile_values() {
    let args = run_args(&[]);
    assert_eq!(args.profile, "quick");
    assert!(args.probe.is_none());
    assert!(args.synchronized_updates.is_none());
    let resolved = resolve_run(&args).unwrap();
    let default = config::resolve(None, "quick").unwrap();
    assert_eq!(resolved.samples, default.samples);
    assert_eq!(resolved.seed, default.seed);
    assert_eq!(resolved.probe, default.probe);
}

#[test]
fn explicit_cli_overrides_are_applied_and_revalidated() {
    let args = run_args(&[
        "--samples",
        "7",
        "--seed",
        "42",
        "--columns",
        "90",
        "--rows",
        "30",
        "--probe=false",
        "--synchronized-updates",
    ]);
    let resolved = resolve_run(&args).unwrap();
    assert_eq!(resolved.samples, 7);
    assert_eq!(resolved.seed, 42);
    assert_eq!(resolved.geometry.columns, 90);
    assert_eq!(resolved.geometry.rows, 30);
    assert!(!resolved.probe);
    assert!(resolved.synchronized_updates);
    for flags in [
        vec!["--samples", "0"],
        vec!["--samples", "101"],
        vec!["--columns", "0"],
        vec!["--rows", "0"],
        vec!["--case", "unknown"],
        vec!["--category", "unknown"],
    ] {
        assert!(resolve_run(&run_args(&flags)).is_err(), "{flags:?}");
    }
}

#[test]
fn exact_cases_replace_implicit_suites_and_repeat_in_catalog_order() {
    let catalog = workload::catalog();
    let ids: Vec<_> = catalog
        .iter()
        .filter(|case| !case.requires_sync)
        .take(2)
        .map(|case| case.id.as_str())
        .collect();
    let args = run_args(&["--case", ids[1], "--case", ids[0]]);
    let resolved = resolve_run(&args).unwrap();
    assert!(resolved.suites.is_empty());
    let selected = config::select(&resolved).unwrap();
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].id, ids[0]);
    assert_eq!(selected[1].id, ids[1]);
}

#[test]
fn category_maps_to_catalog_suites_and_combines_with_explicit_cases() {
    let args = run_args(&[
        "--category",
        "unicode",
        "--category",
        "throughput",
        "--case",
        "plain-logs",
    ]);
    let resolved = resolve_run(&args).unwrap();
    assert_eq!(resolved.suites, ["unicode", "throughput"]);
    let selected = config::select(&resolved).unwrap();
    assert!(selected.iter().any(|case| case.category == "unicode"));
    assert!(selected.iter().any(|case| case.id == "plain-logs"));
}

#[test]
fn argument_errors_and_formats_are_strict() {
    for argv in [
        vec!["bmux-bench", "run", "--samples", "-1"],
        vec!["bmux-bench", "run", "--seed", "nan"],
        vec!["bmux-bench", "report", "x", "--format", "csv"],
        vec!["bmux-bench", "compare", "one"],
        vec!["bmux-bench", "run", "--unknown"],
        vec!["bmux-bench", "run", "--probe=maybe"],
    ] {
        assert!(Cli::try_parse_from(argv).is_err());
    }
    let cli = Cli::try_parse_from(["bmux-bench", "report", "run", "--format", "json"]).unwrap();
    assert!(matches!(
        cli.command,
        Command::Report {
            format: ReportFormat::Json,
            ..
        }
    ));
    let args = run_args(&[
        "--plain", "--json", "--label", "baseline", "--output", "results",
    ]);
    assert!(args.plain && args.json);
    assert_eq!(args.label, "baseline");
    assert_eq!(args.output, Some(PathBuf::from("results")));
}

struct FixtureDir(PathBuf);
impl FixtureDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "bmux-bench-cli-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn toml_profile_then_cli_override_and_unknown_fields() {
    let dir = FixtureDir::new();
    let path = dir.0.join("profiles.toml");
    std::fs::write(
        &path,
        "[profiles.quick]\nsamples = 9\nseed = 25\nprobe = true\ncases = ['plain-logs']\n",
    )
    .unwrap();
    let mut args = run_args(&["--samples", "4", "--probe=false"]);
    args.config = Some(path.clone());
    let resolved = resolve_run(&args).unwrap();
    assert_eq!(resolved.samples, 4);
    assert_eq!(resolved.seed, 25);
    assert!(!resolved.probe);
    assert_eq!(resolved.cases, ["plain-logs"]);
    std::fs::write(&path, "[profiles.quick]\nunknown = true\n").unwrap();
    assert!(resolve_run(&args).is_err());
    std::fs::write(&path, " ".repeat(65_537)).unwrap();
    assert!(resolve_run(&args).unwrap_err().contains("exceeds"));
}

fn record() -> RunRecord {
    let mut config = config::resolve(None, "quick").unwrap();
    config.samples = 3;
    config.cases = vec!["plain-logs".into()];
    config.suites.clear();
    let spec = config::select(&config).unwrap().remove(0);
    let samples = (0..3)
        .map(|index| Sample {
            case_id: spec.id.clone(),
            index,
            metric: "write_acceptance_ns".into(),
            value: 100.0 + index as f64,
            unit: "ns".into(),
            elapsed_ns: 100,
            bytes: 10,
            operations: 1,
            write_calls: 1,
            missed_deadlines: 0,
        })
        .collect();
    RunRecord {
        schema_version: SCHEMA_VERSION,
        run_id: "cli-fixture".into(),
        harness_version: env!("CARGO_PKG_VERSION").into(),
        label: "fixture".into(),
        started_unix_ms: 1,
        status: Status::Complete,
        config,
        environment: BTreeMap::new(),
        capabilities: BTreeMap::new(),
        case_order: vec![spec.id.clone()],
        outcomes: vec![CaseOutcome {
            spec,
            payload_hash: "a".repeat(64),
            status: Status::Complete,
            reason: None,
            samples,
        }],
        warnings: Vec::new(),
    }
}

#[test]
fn human_summary_states_measurement_boundary_samples_and_artifact_path() {
    let record = record();
    let summary = human_summary(&record, Some(std::path::Path::new("results"))).unwrap();
    assert!(summary.contains("NOT completed visible rendering"));
    assert!(summary.contains("3 recorded samples"));
    assert!(summary.contains("throughput"));
    assert!(summary.contains("Artifacts: results"));
    assert!(summary.contains("MAD"));
    assert!(
        human_summary(&record, None)
            .unwrap()
            .contains("Artifact retention explicitly disabled")
    );
}

#[test]
fn report_compare_and_catalog_work_without_opening_terminal() {
    let dir = FixtureDir::new();
    let bundle = dir.0.join("fixture");
    artifact::write_bundle(&bundle, &record()).unwrap();
    let path = bundle.to_str().unwrap();
    assert_eq!(
        run_from(["bmux-bench", "report", path, "--format", "json"]),
        Ok(0)
    );
    assert_eq!(
        run_from(["bmux-bench", "report", path, "--format", "markdown"]),
        Ok(0)
    );
    assert_eq!(
        run_from(["bmux-bench", "compare", path, path, "--json"]),
        Ok(0)
    );
    assert_eq!(run_from(["bmux-bench", "compare", path, path]), Ok(0));
    assert_eq!(run_from(["bmux-bench", "list", "--json"]), Ok(0));
    assert_eq!(run_from(["bmux-bench", "explain", "plain-logs"]), Ok(0));
    assert!(run_from(["bmux-bench", "explain", "missing-case"]).is_err());
    let missing = dir.0.join("missing");
    assert!(run_from(["bmux-bench", "report", missing.to_str().unwrap()]).is_err());
    assert!(run_from(["bmux-bench", "compare", path, missing.to_str().unwrap()]).is_err());
}

#[test]
fn invalid_run_fails_before_any_terminal_acquisition() {
    assert!(
        run_from(["bmux-bench", "run", "--samples", "0", "--json"])
            .unwrap_err()
            .contains("samples")
    );
    assert!(
        run_from(["bmux-bench", "run", "--label", "bad\x1b[31m"])
            .unwrap_err()
            .contains("label")
    );
}

#[test]
fn imported_text_cannot_replay_control_sequences() {
    assert_eq!(safe("\x1b[31m\n\r\t"), "\\u{1b}[31m\\n\\r\\t");
    assert_eq!(safe("Unicode 界"), "Unicode 界");
}

#[test]
fn synchronized_cases_require_opt_in_and_unknown_profile_is_an_error() {
    let id = workload::catalog()
        .into_iter()
        .find(|case| case.requires_sync)
        .unwrap()
        .id;
    assert!(resolve_run(&run_args(&["--case", &id])).is_err());
    let resolved = resolve_run(&run_args(&["--case", &id, "--synchronized-updates"])).unwrap();
    assert_eq!(config::select(&resolved).unwrap().len(), 1);
    assert!(resolve_run(&run_args(&["--profile", "missing-profile"])).is_err());
}

#[test]
fn corrupted_bundle_is_not_reported_or_compared_as_a_healthy_run() {
    let dir = FixtureDir::new();
    let bundle = dir.0.join("corrupt");
    artifact::write_bundle(&bundle, &record()).unwrap();
    std::fs::write(bundle.join("run.json"), "{}").unwrap();
    let path = bundle.to_str().unwrap();
    assert!(run_from(["bmux-bench", "report", path, "--format", "json"]).is_err());
    assert!(run_from(["bmux-bench", "compare", path, path, "--json"]).is_err());
}

#[test]
fn artifacts_are_default_unique_contained_and_explicitly_disableable() {
    let args = run_args(&[]);
    let first = super::artifact_path(&args, "../../unsafe/id").unwrap();
    let second = super::artifact_path(&args, "../../unsafe/id").unwrap();
    assert!(first.starts_with("bmux-bench-results"));
    assert_eq!(first.components().count(), 2);
    assert_ne!(first, second);
    assert!(!first.file_name().unwrap().to_str().unwrap().contains('/'));
    assert!(super::artifact_path(&run_args(&["--no-artifacts"]), "run").is_none());
    assert_eq!(
        super::artifact_path(&run_args(&["--output", "custom"]), "run"),
        Some(PathBuf::from("custom"))
    );
    assert!(
        Cli::try_parse_from(["bmux-bench", "run", "--no-artifacts", "--output", "custom"]).is_err()
    );
}

#[test]
fn shared_finalizer_retains_explicit_bundle_and_refuses_overwrite() {
    let dir = FixtureDir::new();
    let output = dir.0.join("finalized");
    let args = run_args(&["--output", output.to_str().unwrap(), "--plain", "--json"]);
    assert_eq!(super::finalize_run(&args, &record(), false), Ok(0));
    assert_eq!(
        artifact::read_bundle(&output).unwrap().run_id,
        "cli-fixture"
    );
    assert!(super::finalize_run(&args, &record(), false).is_err());
    assert!(
        super::retain_artifacts(&run_args(&["--no-artifacts"]), &record())
            .unwrap()
            .is_none()
    );
}

#[cfg(feature = "internal")]
#[test]
fn internal_flag_routes_to_tty_free_runner_and_shared_artifacts() {
    let dir = FixtureDir::new();
    let output = dir.0.join("internal");
    let args = run_args(&[
        "--internal",
        "--plain",
        "--samples",
        "1",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(args.internal);
    let code = super::execute_run_with_cancel(
        &args,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        false,
    )
    .unwrap();
    assert!(code == 0 || code == 1);
    let record = artifact::read_bundle(&output).unwrap();
    assert!(!record.outcomes.is_empty());
    assert_eq!(record.config.samples, 1);
}

#[cfg(feature = "internal")]
#[test]
fn caller_owned_cancellation_is_preserved_and_returns_cancelled_record() {
    let dir = FixtureDir::new();
    let output = dir.0.join("cancelled");
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let code = super::run_from_with_cancel(
        [
            "bmux-bench",
            "run",
            "--internal",
            "--plain",
            "--json",
            "--samples",
            "1",
            "--output",
            output.to_str().unwrap(),
        ],
        std::sync::Arc::clone(&cancelled),
    )
    .unwrap();
    assert_eq!(code, 130);
    assert!(cancelled.load(Ordering::Relaxed));
    assert_eq!(
        artifact::read_bundle(&output).unwrap().status,
        Status::Cancelled
    );
}

#[cfg(not(feature = "internal"))]
#[test]
fn internal_flag_is_unavailable_without_feature() {
    assert!(Cli::try_parse_from(["bmux-bench", "run", "--internal"]).is_err());
}
