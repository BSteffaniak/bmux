//! Strict, composable TOML profiles. Selection is a union of suites and explicit
//! cases, followed by exclusions, in stable catalog order. Synchronized-update
//! cases are never admitted by a suite unless the caller explicitly opts in.
use crate::model::{BenchResult, CaseSpec, Geometry, RunConfig};
use crate::workload::{MAX_BYTES, MIN_BYTES, SUITES, catalog, minimum_budget, validate_geometry};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const MAX_CONFIG_BYTES: usize = 65_536;
const MAX_PROFILES: usize = 64;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    suites: Option<Vec<String>>,
    sampling: Option<String>,
    cases: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    samples: Option<usize>,
    warmup: Option<usize>,
    max_case_ms: Option<u64>,
    max_bytes_per_sample: Option<usize>,
    seed: Option<u64>,
    geometry: Option<Geometry>,
    probe: Option<bool>,
    probe_timeout_ms: Option<u64>,
    synchronized_updates: Option<bool>,
    practical_change_percent: Option<f64>,
}

/// User profiles inherit their named built-in (or standard for a new name).
/// `sampling` replaces only sampling defaults; explicit fields override last.
/// Lists replace inherited lists, and may be combined explicitly in one profile.
/// Every declared profile is validated, including profiles not selected for use.
pub fn resolve(config_toml: Option<&str>, profile: &str) -> BenchResult<RunConfig> {
    validate_name(profile)?;
    let document = if let Some(text) = config_toml {
        if text.len() > MAX_CONFIG_BYTES {
            return Err("benchmark configuration exceeds 65536 bytes".into());
        }
        toml::from_str::<Document>(text)
            .map_err(|error| format!("invalid benchmark configuration: {error}"))?
    } else {
        Document::default()
    };
    if document.profiles.len() > MAX_PROFILES {
        return Err("at most 64 profiles are permitted".into());
    }
    for (name, settings) in &document.profiles {
        validate_name(name)?;
        let candidate = compose(name, Some(settings))?;
        validate(&candidate).map_err(|error| format!("profile {name}: {error}"))?;
    }
    if !is_builtin(profile) && !document.profiles.contains_key(profile) {
        return Err(format!("unknown benchmark profile: {profile}"));
    }
    let config = compose(profile, document.profiles.get(profile))?;
    validate(&config)?;
    Ok(config)
}

fn is_builtin(name: &str) -> bool {
    matches!(name, "quick" | "standard" | "rigorous") || SUITES.contains(&name)
}

fn validate_name(name: &str) -> BenchResult<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(
            "profile names must contain 1..64 ASCII letters, digits, underscores, or hyphens"
                .into(),
        );
    }
    Ok(())
}

fn sampling(config: &mut RunConfig, name: &str) -> BenchResult<()> {
    let (samples, warmup, max_case_ms, max_bytes) = match name {
        "quick" => (3, 1, 1000, 64 * 1024),
        "standard" => (9, 2, 3000, 256 * 1024),
        "rigorous" => (25, 5, 10_000, 1024 * 1024),
        _ => return Err(format!("unknown sampling setting: {name}")),
    };
    config.samples = samples;
    config.warmup = warmup;
    config.max_case_ms = max_case_ms;
    config.max_bytes_per_sample = max_bytes;
    Ok(())
}

fn compose(name: &str, settings: Option<&Profile>) -> BenchResult<RunConfig> {
    let mut config = RunConfig {
        profile: name.to_owned(),
        suites: SUITES.iter().map(|suite| (*suite).to_owned()).collect(),
        cases: Vec::new(),
        exclude: Vec::new(),
        geometry: Geometry::default(),
        seed: 0x424d_5558,
        samples: 9,
        warmup: 2,
        max_case_ms: 3000,
        max_bytes_per_sample: 256 * 1024,
        probe: false,
        probe_timeout_ms: 250,
        synchronized_updates: false,
        practical_change_percent: 5.0,
    };
    if SUITES.contains(&name) {
        config.suites = vec![name.to_owned()];
    }
    if matches!(name, "quick" | "standard" | "rigorous") {
        sampling(&mut config, name)?;
    }
    if let Some(settings) = settings {
        if let Some(name) = &settings.sampling {
            sampling(&mut config, name)?;
        }
        if let Some(value) = &settings.suites {
            config.suites.clone_from(value);
        }
        // An explicit case list without suites is an exact selection, not an
        // accidental union with all default suites.
        if let Some(value) = &settings.cases {
            config.cases.clone_from(value);
            if settings.suites.is_none() {
                config.suites.clear();
            }
        }
        if let Some(value) = &settings.exclude {
            config.exclude.clone_from(value);
        }
        if let Some(value) = settings.samples {
            config.samples = value;
        }
        if let Some(value) = settings.warmup {
            config.warmup = value;
        }
        if let Some(value) = settings.max_case_ms {
            config.max_case_ms = value;
        }
        if let Some(value) = settings.max_bytes_per_sample {
            config.max_bytes_per_sample = value;
        }
        if let Some(value) = settings.seed {
            config.seed = value;
        }
        if let Some(value) = settings.geometry {
            config.geometry = value;
        }
        if let Some(value) = settings.probe {
            config.probe = value;
        }
        if let Some(value) = settings.probe_timeout_ms {
            config.probe_timeout_ms = value;
        }
        if let Some(value) = settings.synchronized_updates {
            config.synchronized_updates = value;
        }
        if let Some(value) = settings.practical_change_percent {
            config.practical_change_percent = value;
        }
    }
    Ok(config)
}

/// Validate externally constructed configs too; deserialization alone is not an
/// admission check. Caps bound per-case execution and retained workload storage.
pub fn validate(config: &RunConfig) -> BenchResult<()> {
    validate_name(&config.profile)?;
    validate_geometry(config.geometry)?;
    if !(1..=100).contains(&config.samples) {
        return Err("samples must be 1..100".into());
    }
    if config.warmup > 20 {
        return Err("warmup must be 0..20".into());
    }
    if !(1..=60_000).contains(&config.max_case_ms) {
        return Err("max_case_ms must be 1..60000".into());
    }
    if !(MIN_BYTES..=MAX_BYTES).contains(&config.max_bytes_per_sample) {
        return Err("max_bytes_per_sample must be 4096..16777216".into());
    }
    if !(1..=5000).contains(&config.probe_timeout_ms) {
        return Err("probe_timeout_ms must be 1..5000".into());
    }
    if !config.practical_change_percent.is_finite()
        || !(0.0..=100.0).contains(&config.practical_change_percent)
    {
        return Err("practical_change_percent must be finite and 0..100".into());
    }
    validate_list("suites", &config.suites, SUITES.len())?;
    let cases = catalog();
    validate_list("cases", &config.cases, cases.len())?;
    validate_list("exclude", &config.exclude, cases.len())?;
    for suite in &config.suites {
        if !SUITES.contains(&suite.as_str()) {
            return Err(format!("unknown suite: {suite}"));
        }
    }
    for id in config.cases.iter().chain(&config.exclude) {
        if !cases.iter().any(|case| &case.id == id) {
            return Err(format!("unknown case: {id}"));
        }
    }
    for id in &config.cases {
        if !config.synchronized_updates
            && cases
                .iter()
                .any(|case| &case.id == id && case.requires_sync)
        {
            return Err(format!("case {id} requires synchronized_updates = true"));
        }
        if config.exclude.contains(id) {
            return Err(format!(
                "case {id} is both explicitly selected and excluded"
            ));
        }
    }
    let selected = selected(config, cases);
    if selected.is_empty() {
        return Err("profile selects no eligible cases".into());
    }
    for case in selected {
        let required = minimum_budget(&case.id, config.geometry);
        if config.max_bytes_per_sample < required {
            return Err(format!(
                "case {} requires max_bytes_per_sample >= {required} for this geometry",
                case.id
            ));
        }
    }
    Ok(())
}

fn validate_list(field: &str, values: &[String], limit: usize) -> BenchResult<()> {
    if values.len() > limit {
        return Err(format!("too many {field} entries (maximum {limit})"));
    }
    let unique: BTreeSet<_> = values.iter().collect();
    if unique.len() != values.len() {
        return Err(format!("duplicate {field} entries"));
    }
    Ok(())
}

fn selected(config: &RunConfig, cases: Vec<CaseSpec>) -> Vec<CaseSpec> {
    cases
        .into_iter()
        .filter(|case| {
            (!case.requires_sync || config.synchronized_updates)
                && !config.exclude.contains(&case.id)
                && (config.cases.contains(&case.id)
                    || case
                        .suites
                        .iter()
                        .any(|suite| config.suites.contains(suite)))
        })
        .collect()
}

pub fn select(config: &RunConfig) -> BenchResult<Vec<CaseSpec>> {
    validate(config)?;
    Ok(selected(config, catalog()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtin_profiles_are_valid_and_sync_is_opt_in() {
        for name in [
            "quick",
            "standard",
            "rigorous",
            "realistic",
            "throughput",
            "unicode",
            "tui",
            "edge",
        ] {
            let config = resolve(None, name).unwrap();
            let cases = select(&config).unwrap();
            assert!(!cases.is_empty());
            assert!(cases.iter().all(|case| !case.requires_sync));
        }
        assert!(resolve(None, "missing").is_err());
        assert!(
            resolve(None, "quick").unwrap().samples < resolve(None, "rigorous").unwrap().samples
        );
    }

    #[test]
    fn sampling_and_explicit_overrides_compose() {
        let config = resolve(
            Some(
                r#"
[profiles.local]
sampling = 'quick'
suites = ['unicode', 'tui']
exclude = ['tui-dense']
samples = 7
warmup = 0
seed = 9
geometry = { columns = 120, rows = 40 }
probe = true
probe_timeout_ms = 100
max_case_ms = 100
max_bytes_per_sample = 32768
synchronized_updates = true
practical_change_percent = 2.5
"#,
            ),
            "local",
        )
        .unwrap();
        assert_eq!(config.samples, 7);
        assert_eq!(config.warmup, 0);
        assert_eq!(config.seed, 9);
        assert_eq!(config.geometry.columns, 120);
        assert_eq!(config.max_bytes_per_sample, 32768);
        assert!(config.probe);
        assert_eq!(config.practical_change_percent, 2.5);
        let cases = select(&config).unwrap();
        assert!(cases.iter().any(|case| case.id == "frames-synchronized"));
        assert!(cases.iter().all(|case| case.id != "tui-dense"));
    }

    #[test]
    fn explicit_cases_replace_default_suites_and_keep_catalog_order() {
        let config = resolve(
            Some("[profiles.local]\ncases = ['progress', 'plain-logs']"),
            "local",
        )
        .unwrap();
        assert!(config.suites.is_empty());
        assert_eq!(
            select(&config)
                .unwrap()
                .iter()
                .map(|case| case.id.as_str())
                .collect::<Vec<_>>(),
            ["plain-logs", "progress"]
        );
    }

    #[test]
    fn rejects_unknown_schema_even_in_unused_profiles() {
        for text in [
            "unexpected = true",
            "[profiles.local]\nunknown = 1",
            "[profiles.local]\nsampling = 'fast'",
            "[profiles.local]\nsuites = ['unknown']",
            "[profiles.local]\ncases = ['unknown']",
            "[profiles.local]\nexclude = ['unknown']",
            "[profiles.local.geometry]\ncolumns = 80\nrows = 24\nunknown = 1",
            "[profiles.local]\nsamples = '3'",
            "[profiles.local]\nsamples = -1",
            "[profiles.local]\nprobe = 'yes'",
            "[profiles.local]\nsuites = ['tui', 'tui']",
        ] {
            assert!(resolve(Some(text), "quick").is_err(), "accepted {text}");
        }
    }

    #[test]
    fn rejects_empty_contradictory_or_unopted_selection() {
        for fields in [
            "suites = []",
            "cases = []",
            "cases = ['frames-synchronized']",
            "cases = ['plain-logs']\nexclude = ['plain-logs']",
            "cases = ['plain-logs', 'plain-logs']",
            "suites = ['tui']\nexclude = ['tui-dense', 'tui-sparse', 'scroll-region', 'frames-unsynchronized', 'frames-synchronized']",
        ] {
            assert!(
                resolve(Some(&format!("[profiles.local]\n{fields}")), "local").is_err(),
                "accepted {fields}"
            );
        }
    }

    #[test]
    fn rejects_all_numeric_boundaries_and_nonfinite_values() {
        for field in [
            "samples = 0",
            "samples = 101",
            "warmup = 21",
            "max_case_ms = 0",
            "max_case_ms = 60001",
            "max_bytes_per_sample = 1023",
            "max_bytes_per_sample = 16777217",
            "probe_timeout_ms = 0",
            "probe_timeout_ms = 5001",
            "practical_change_percent = nan",
            "practical_change_percent = inf",
            "practical_change_percent = -0.1",
            "practical_change_percent = 100.1",
            "geometry = { columns = 0, rows = 24 }",
            "geometry = { columns = 513, rows = 24 }",
            "geometry = { columns = 512, rows = 256 }",
        ] {
            assert!(
                resolve(Some(&format!("[profiles.local]\n{field}")), "local").is_err(),
                "accepted {field}"
            );
        }
    }

    #[test]
    fn builtin_sampling_profiles_leave_generous_pacing_headroom() {
        for (profile, expected_ms) in [("quick", 120), ("standard", 330), ("rigorous", 900)] {
            let config = resolve(None, profile).unwrap();
            assert!(
                select(&config)
                    .unwrap()
                    .iter()
                    .any(|case| case.id == "paced-updates")
            );
            let workload = crate::workload::generate(
                "paced-updates",
                config.geometry,
                config.seed,
                config.max_bytes_per_sample,
            )
            .unwrap();
            assert_eq!(workload.chunks.len(), 16);
            let gaps = u64::try_from(workload.chunks.len() - 1).unwrap();
            let repetitions = u64::try_from(config.warmup + config.samples).unwrap();
            let pacing_ns = gaps * workload.interval_ns * repetitions;
            assert_eq!(pacing_ns, expected_ms * 1_000_000);
            // At least 75% of the whole-case deadline remains for actual I/O,
            // setup/teardown, scheduling, and sample bookkeeping.
            assert!(pacing_ns * 4 <= config.max_case_ms * 1_000_000, "{profile}");
        }
    }

    #[test]
    fn minimum_budget_matches_generator() {
        let mut config =
            resolve(Some("[profiles.local]\ncases = ['plain-logs']"), "local").unwrap();
        config.max_bytes_per_sample = MIN_BYTES - 1;
        assert!(validate(&config).is_err());
        config.max_bytes_per_sample = MIN_BYTES;
        assert!(validate(&config).is_ok());
        assert!(
            crate::workload::generate(
                "plain-logs",
                config.geometry,
                config.seed,
                config.max_bytes_per_sample
            )
            .is_ok()
        );
    }

    #[test]
    fn externally_constructed_configs_are_revalidated() {
        let mut config = resolve(None, "quick").unwrap();
        config.samples = 0;
        assert!(select(&config).is_err());
        config.samples = 3;
        config.practical_change_percent = f64::NAN;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn config_size_and_names_are_bounded() {
        assert!(resolve(Some(&" ".repeat(MAX_CONFIG_BYTES + 1)), "quick").is_err());
        assert!(resolve(Some("[profiles.'bad name']\nsamples = 3"), "quick").is_err());
        assert!(resolve(None, "").is_err());
    }
}
