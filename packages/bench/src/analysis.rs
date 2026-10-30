//! Deterministic, distribution-free estimates; never a claim of visible rendering.
use crate::model::{
    BenchResult, CaseOutcome, Change, Comparison, MetricSummary, RunRecord, RunSummary,
    SCHEMA_VERSION, Status,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

const RESAMPLES: usize = 2_000;
const MIN_COMPARISON_SAMPLES: usize = 5;
const MAX_SAMPLES: usize = 100_000;
const MAX_GROUP_SAMPLES: usize = 10_000;
const METHOD: &str = "independent percentile bootstrap of medians; 2000 resamples; central 95% interval; SplitMix64 rejection-sampled indexes; FNV-1a byte seed (offset xor run seed; case + NUL + metric); no observations discarded; relative difference = 100*(candidate/baseline-1)";

/// Shared validation for both in-memory analysis and untrusted artifact input.
pub(crate) fn validate(run: &RunRecord) -> BenchResult<()> {
    let config = &run.config;
    crate::config::validate(config)?;
    if run.schema_version != SCHEMA_VERSION {
        return Err(format!("unsupported run schema {}", run.schema_version));
    }
    if run.run_id.trim().is_empty() || run.harness_version.trim().is_empty() {
        return Err("run identity and harness version must be nonempty".into());
    }
    if config.geometry.columns == 0
        || config.geometry.rows == 0
        || config.samples == 0
        || config.samples > MAX_GROUP_SAMPLES
        || config.warmup > MAX_GROUP_SAMPLES
        || config.max_case_ms == 0
        || config.max_bytes_per_sample == 0
        || config.probe_timeout_ms == 0
        || !config.practical_change_percent.is_finite()
        || !(0.0..=100.0).contains(&config.practical_change_percent)
    {
        return Err("invalid benchmark counts, dimensions, budgets, or practical threshold".into());
    }
    if run.case_order.is_empty() || run.case_order.len() > 1_024 || run.outcomes.len() > 1_024 {
        return Err("run must name 1..=1024 cases".into());
    }
    let order: BTreeSet<_> = run.case_order.iter().collect();
    if order.len() != run.case_order.len() || order.iter().any(|id| id.trim().is_empty()) {
        return Err("empty or duplicate case order identity".into());
    }
    let mut seen = BTreeSet::new();
    let mut total = 0_usize;
    for outcome in &run.outcomes {
        let id = &outcome.spec.id;
        if !order.contains(id)
            || !seen.insert(id)
            || outcome.spec.revision == 0
            || outcome.spec.category.trim().is_empty()
            || outcome.spec.title.trim().is_empty()
        {
            return Err(format!("invalid or duplicate case identity: {id}"));
        }
        // Unsupported workloads may not have a generated payload at all.
        if !(outcome.payload_hash.len() == 64
            && outcome.payload_hash.bytes().all(|b| b.is_ascii_hexdigit()))
            && !(matches!(outcome.status, Status::Unsupported | Status::Failed)
                && outcome.payload_hash.is_empty()
                && outcome.samples.is_empty())
        {
            return Err(format!("invalid SHA-256 payload hash for {id}"));
        }
        if outcome.status == Status::Complete && outcome.reason.is_some() {
            return Err(format!("complete case {id} carries a failure/stop reason"));
        }
        if outcome.status != Status::Complete
            && outcome
                .reason
                .as_ref()
                .is_none_or(|reason| reason.trim().is_empty())
        {
            return Err(format!("non-complete case {id} needs an explicit reason"));
        }
        if outcome.status == Status::Unsupported && !outcome.samples.is_empty() {
            return Err(format!("unsupported case {id} contains measurements"));
        }
        total = total
            .checked_add(outcome.samples.len())
            .ok_or("sample count overflow")?;
        if total > MAX_SAMPLES {
            return Err("artifact exceeds 100000 raw samples".into());
        }
        let mut groups: BTreeMap<&str, (&str, BTreeSet<usize>)> = BTreeMap::new();
        for sample in &outcome.samples {
            let internal_metric = id.starts_with("internal-v1/")
                && matches!(
                    sample.metric.as_str(),
                    "parser_bytes_per_second"
                        | "grid_snapshot_ns"
                        | "grid_visible_text_ns"
                        | "tui_plain_text_layout_paint_ns"
                        | "tui_ansi_encode_ns"
                );
            let byte_limit = if internal_metric {
                64 * 1024 * 1024
            } else {
                config.max_bytes_per_sample as u64
            };
            if sample.case_id != *id
                || sample.metric.trim().is_empty()
                || sample.unit.trim().is_empty()
                || sample.index >= config.samples
                || !sample.value.is_finite()
                || sample.value < 0.0
                || (positive_unit(&sample.unit) && sample.value <= 0.0)
                || sample.elapsed_ns == 0
                || sample.bytes > byte_limit
                || (!internal_metric && (sample.bytes == 0 || sample.write_calls == 0))
                || (internal_metric && sample.write_calls != 0)
                || sample.operations == 0
                || sample.write_calls > sample.bytes
                || sample.missed_deadlines > sample.write_calls
                || sample.elapsed_ns > config.max_case_ms.saturating_mul(1_000_000)
            {
                return Err(format!("invalid measurement in {id}/{}", sample.metric));
            }
            if sample.unit == "count" && sample.value.fract() != 0.0 {
                return Err(format!("fractional count in {id}/{}", sample.metric));
            }
            if sample.metric == "emission_deadline_misses"
                && (sample.unit != "count" || sample.value != sample.missed_deadlines as f64)
            {
                return Err(format!(
                    "deadline count contradicts sample metadata in {id}"
                ));
            }
            let (unit, indexes) = groups
                .entry(&sample.metric)
                .or_insert((&sample.unit, BTreeSet::new()));
            if *unit != sample.unit || !indexes.insert(sample.index) {
                return Err(format!(
                    "mixed units or duplicate sample index in {id}/{}",
                    sample.metric
                ));
            }
        }
        if groups.len() > 32 {
            return Err(format!("too many metrics in {id}"));
        }
        if outcome.status == Status::Complete
            && (groups.is_empty()
                || groups
                    .values()
                    .any(|(_, indexes)| indexes.len() != config.samples))
        {
            return Err(format!(
                "complete case {id} does not contain all configured samples"
            ));
        }
    }
    let all_complete = run.outcomes.len() == order.len()
        && run
            .outcomes
            .iter()
            .all(|case| case.status == Status::Complete);
    let has_failed = run
        .outcomes
        .iter()
        .any(|case| case.status == Status::Failed);
    let has_cancelled = run
        .outcomes
        .iter()
        .any(|case| case.status == Status::Cancelled);
    if (run.status == Status::Partial && (has_failed || has_cancelled))
        || (run.status == Status::Failed && !has_failed && run.warnings.is_empty())
    {
        return Err("run status hides a failed/cancelled case or lacks failure evidence".into());
    }
    if (run.status == Status::Complete && !all_complete)
        || (run.status != Status::Complete && run.status != Status::Failed && all_complete)
        || (run.status == Status::Failed && all_complete && run.warnings.is_empty())
        || (run.status == Status::Unsupported
            && (run.outcomes.len() != order.len()
                || run
                    .outcomes
                    .iter()
                    .any(|case| case.status != Status::Unsupported)))
    {
        return Err("run status contradicts case outcomes".into());
    }
    Ok(())
}

fn positive_unit(unit: &str) -> bool {
    matches!(
        unit,
        "ns" | "us" | "µs" | "ms" | "s" | "bytes/s" | "ops/s" | "operations/s" | "MiB/s" | "MB/s"
    )
}

fn groups(case: &CaseOutcome) -> BTreeMap<(&str, &str), Vec<f64>> {
    let mut groups = BTreeMap::new();
    for sample in &case.samples {
        groups
            .entry((sample.metric.as_str(), sample.unit.as_str()))
            .or_insert_with(Vec::new)
            .push(sample.value);
    }
    for values in groups.values_mut() {
        values.sort_unstable_by(f64::total_cmp);
    }
    groups
}

// Stable byte-based derivation: independent of Rust's randomized hash implementation.
fn seed(run_seed: u64, case: &str, metric: &str) -> u64 {
    let mut hash = run_seed ^ 0xcbf2_9ce4_8422_2325;
    for byte in case.bytes().chain([0]).chain(metric.bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

struct Random(u64);
impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
    fn index(&mut self, len: usize) -> usize {
        let bound = len as u64;
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let value = self.next();
            if value >= threshold {
                return (value % bound) as usize;
            }
        }
    }
}

fn median(values: &mut [f64]) -> f64 {
    let len = values.len();
    let mid = len / 2;
    let (left, center, _) = values.select_nth_unstable_by(mid, f64::total_cmp);
    if len.is_multiple_of(2) {
        let lower = left
            .iter()
            .copied()
            .max_by(f64::total_cmp)
            .unwrap_or(*center);
        lower + (*center - lower) / 2.0
    } else {
        *center
    }
}

fn resampled_median(values: &[f64], random: &mut Random, scratch: &mut Vec<f64>) -> f64 {
    scratch.clear();
    scratch.extend((0..values.len()).map(|_| values[random.index(values.len())]));
    median(scratch)
}

fn interval(mut values: Vec<f64>) -> (f64, f64) {
    values.sort_unstable_by(f64::total_cmp);
    // Conservative observed order statistics; no interpolation beyond the data.
    (
        values[(values.len() - 1) * 25 / 1000],
        values[((values.len() - 1) * 975).div_ceil(1000)],
    )
}

fn bootstrap(values: &[f64], random_seed: u64) -> (f64, f64) {
    let mut random = Random(random_seed);
    let mut scratch = Vec::with_capacity(values.len());
    interval(
        (0..RESAMPLES)
            .map(|_| resampled_median(values, &mut random, &mut scratch))
            .collect(),
    )
}

/// Summarize all observations, including explicitly marked partial measurements.
pub fn summarize(run: &RunRecord) -> BenchResult<RunSummary> {
    validate(run)?;
    let mut metrics = Vec::new();
    let mut cases: Vec<_> = run.outcomes.iter().collect();
    cases.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
    for case in cases {
        for ((metric, unit), mut values) in groups(case) {
            let center = median(&mut values);
            let mut deviations: Vec<_> =
                values.iter().map(|value| (value - center).abs()).collect();
            let mad = median(&mut deviations);
            let outliers = deviations
                .iter()
                .filter(|deviation| **deviation > 3.0 * mad)
                .count();
            let (ci_low, ci_high) =
                bootstrap(&values, seed(run.config.seed, &case.spec.id, metric));
            let mut warnings = Vec::new();
            if values.len() < MIN_COMPARISON_SAMPLES {
                warnings.push(
                    "Too few samples for a comparison; bootstrap coverage is unreliable.".into(),
                );
            }
            if values.len() < 100 {
                warnings.push("No p99 estimate: fewer than 100 observations.".into());
            }
            if case.status != Status::Complete {
                warnings.push(format!(
                    "{:?} case: descriptive observations only, excluded from comparisons.",
                    case.status
                ));
            }
            if outliers > 0 {
                warnings.push(format!("{outliers} observations beyond 3 MAD; all retained (MAD=0 flags every nonmedian value)."));
            }
            metrics.push(MetricSummary {
                case_id: case.spec.id.clone(),
                metric: metric.into(),
                unit: unit.into(),
                count: values.len(),
                median: center,
                min: values.iter().copied().fold(f64::INFINITY, f64::min),
                max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                mad,
                ci_low,
                ci_high,
                outliers,
                warnings,
            });
        }
    }
    Ok(RunSummary {
        schema_version: SCHEMA_VERSION,
        method: format!("{METHOD}; run seed={}", run.config.seed),
        metrics,
    })
}

fn compatible(a: &RunRecord, b: &RunRecord) -> bool {
    let a = &a.config;
    let b = &b.config;
    a.geometry == b.geometry
        && a.seed == b.seed
        && a.samples == b.samples
        && a.warmup == b.warmup
        && a.max_case_ms == b.max_case_ms
        && a.max_bytes_per_sample == b.max_bytes_per_sample
        && a.probe_timeout_ms == b.probe_timeout_ms
        && a.probe == b.probe
        && a.synchronized_updates == b.synchronized_updates
        && a.practical_change_percent == b.practical_change_percent
}

fn direction(metric: &str, unit: &str) -> Option<bool> {
    if matches!(
        unit,
        "bytes/s" | "ops/s" | "operations/s" | "MiB/s" | "MB/s"
    ) {
        Some(true)
    } else if matches!(unit, "ns" | "us" | "µs" | "ms" | "s")
        || metric.contains("missed_deadline")
        || metric == "emission_deadline_misses"
        || metric.contains("write_calls")
    {
        Some(false)
    } else {
        None
    }
}

fn undecided(case_id: &str, metric: &str, change: Change, reason: &str) -> Comparison {
    Comparison {
        case_id: case_id.into(),
        metric: metric.into(),
        change,
        percent_change: None,
        ci_low: None,
        ci_high: None,
        reason: reason.into(),
    }
}

/// Compare the union of selected case identities; positive change means a larger raw value.
pub fn compare(baseline: &RunRecord, candidate: &RunRecord) -> BenchResult<Vec<Comparison>> {
    validate(baseline)?;
    validate(candidate)?;
    let before: BTreeMap<_, _> = baseline
        .outcomes
        .iter()
        .map(|case| (case.spec.id.as_str(), case))
        .collect();
    let after: BTreeMap<_, _> = candidate
        .outcomes
        .iter()
        .map(|case| (case.spec.id.as_str(), case))
        .collect();
    let ids: BTreeSet<_> = baseline
        .case_order
        .iter()
        .chain(&candidate.case_order)
        .map(String::as_str)
        .collect();
    let mut comparisons = Vec::new();
    for id in ids {
        let (Some(a), Some(b)) = (before.get(id), after.get(id)) else {
            comparisons.push(undecided(
                id,
                "*",
                Change::Inconclusive,
                "Case is missing from one run; no paired outcome.",
            ));
            continue;
        };
        if !compatible(baseline, candidate)
            || baseline.case_order != candidate.case_order
            || baseline.harness_version != candidate.harness_version
            || baseline.environment.get("backend") != candidate.environment.get("backend")
            || baseline.environment.get("harness_source_sha256")
                != candidate.environment.get("harness_source_sha256")
            || baseline.environment.get("arch") != candidate.environment.get("arch")
            || baseline.environment.get("os") != candidate.environment.get("os")
            || a.spec.revision != b.spec.revision
            || a.payload_hash != b.payload_hash
            || a.spec.requires_sync != b.spec.requires_sync
        {
            comparisons.push(undecided(id, "*", Change::Incompatible,
                "Harness, case ordering, case revision/hash, geometry, seed, sampling, pacing budgets, synchronization, probe mode, or practical threshold differs."));
            continue;
        }
        if a.status == Status::Unsupported || b.status == Status::Unsupported {
            comparisons.push(undecided(
                id,
                "*",
                Change::Unsupported,
                "At least one case is explicitly unsupported.",
            ));
            continue;
        }
        if matches!(baseline.status, Status::Failed | Status::Cancelled)
            || matches!(candidate.status, Status::Failed | Status::Cancelled)
            || a.status != Status::Complete
            || b.status != Status::Complete
        {
            comparisons.push(undecided(
                id,
                "*",
                Change::Inconclusive,
                "Failed, cancelled, or partial cases are not compared.",
            ));
            continue;
        }
        let ga = groups(a);
        let gb = groups(b);
        let metrics: BTreeSet<_> = ga
            .keys()
            .chain(gb.keys())
            .map(|(metric, _)| *metric)
            .collect();
        for metric in metrics {
            let left = ga.iter().find(|((name, _), _)| *name == metric);
            let right = gb.iter().find(|((name, _), _)| *name == metric);
            let (Some(((_, unit_a), va)), Some(((_, unit_b), vb))) = (left, right) else {
                comparisons.push(undecided(
                    id,
                    metric,
                    Change::Incompatible,
                    "Metric absent from one run.",
                ));
                continue;
            };
            if unit_a != unit_b {
                comparisons.push(undecided(
                    id,
                    metric,
                    Change::Incompatible,
                    "Metric units differ.",
                ));
                continue;
            }
            let Some(higher_is_better) = direction(metric, unit_a) else {
                comparisons.push(undecided(
                    id,
                    metric,
                    Change::Inconclusive,
                    "Unknown metric direction; no performance claim.",
                ));
                continue;
            };
            if va.len() < MIN_COMPARISON_SAMPLES || vb.len() < MIN_COMPARISON_SAMPLES {
                comparisons.push(undecided(
                    id,
                    metric,
                    Change::Inconclusive,
                    "At least five observations per run are required.",
                ));
                continue;
            }
            if va.contains(&0.0) {
                comparisons.push(undecided(
                    id,
                    metric,
                    Change::Inconclusive,
                    "Zero baseline observations make relative bootstrap ratios undefined.",
                ));
                continue;
            }
            let base = median(&mut va.clone());
            let next = median(&mut vb.clone());
            let percent = 100.0 * (next / base - 1.0);
            let mut random = Random(seed(baseline.config.seed, id, metric));
            let mut scratch = Vec::new();
            let mut estimates = Vec::with_capacity(RESAMPLES);
            for _ in 0..RESAMPLES {
                let denominator = resampled_median(va, &mut random, &mut scratch);
                let numerator = resampled_median(vb, &mut random, &mut scratch);
                estimates.push(100.0 * (numerator / denominator - 1.0));
            }
            if !percent.is_finite() || estimates.iter().any(|value| !value.is_finite()) {
                comparisons.push(undecided(
                    id,
                    metric,
                    Change::Inconclusive,
                    "Relative estimate exceeds finite numeric range.",
                ));
                continue;
            }
            let (low, high) = interval(estimates);
            let threshold = baseline.config.practical_change_percent;
            let larger = low > threshold;
            let smaller = high < -threshold;
            let change = if larger || smaller {
                if larger == higher_is_better {
                    Change::Faster
                } else {
                    Change::Slower
                }
            } else if low >= -threshold && high <= threshold {
                Change::NoMeaningfulChange
            } else {
                Change::Inconclusive
            };
            comparisons.push(Comparison { case_id: id.into(), metric: metric.into(), change,
                percent_change: Some(percent), ci_low: Some(low), ci_high: Some(high),
                reason: format!("Independent median bootstrap 95% CI; practical band ±{threshold}%. Positive means larger raw metric. Estimates are not causal diagnosis; no multiplicity correction."),
            });
        }
    }
    Ok(comparisons)
}

fn escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| match c {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect(),
            '>' => "&gt;".chars().collect(),
            '|' => "&#124;".chars().collect(),
            '\n' | '\r' => " ".chars().collect(),
            '`' => "&#96;".chars().collect(),
            '\\' => "&#92;".chars().collect(),
            '*' => "&#42;".chars().collect(),
            '_' => "&#95;".chars().collect(),
            '[' => "&#91;".chars().collect(),
            ']' => "&#93;".chars().collect(),
            '!' => "&#33;".chars().collect(),
            '#' => "&#35;".chars().collect(),
            c if c.is_control() => Vec::new(),
            c => vec![c],
        })
        .collect()
}

/// Human-readable category report. Deliberately has no aggregate terminal score.
pub fn report(run: &RunRecord, summary: &RunSummary) -> String {
    let mut out = format!(
        "# Portable terminal benchmark: {}\n\nRun `{}` — **{:?}**; {}×{} cells; seed {}.\n\n## Measurement boundaries\n\nWrite acceptance measures producer/transport backpressure, **not visible rendering**. A protocol response measures a round trip to its protocol owner, **not frame presentation or input-to-photon latency**. Neither establishes GPU completion, compositor scheduling, or pixels on screen. No aggregate terminal score is calculated.\n\n## Statistical method\n\n{}\n\nIntervals are estimates under independent, representative sampling, not guarantees or causal diagnoses. Serial correlation, thermal state, scheduling, font, display and terminal configuration can confound results. No outliers are discarded. Small-sample tail percentiles are not reported; no multiple-comparison correction is applied.\n\nRaw observations: [samples.jsonl](samples.jsonl); full metadata and outcomes: [run.json](run.json); machine-readable estimates: [summary.json](summary.json).\n",
        escape(&run.label),
        escape(&run.run_id),
        run.status,
        run.config.geometry.columns,
        run.config.geometry.rows,
        run.config.seed,
        escape(&summary.method)
    );
    let mut categories: BTreeMap<&str, Vec<&CaseOutcome>> = BTreeMap::new();
    for case in &run.outcomes {
        categories
            .entry(&case.spec.category)
            .or_default()
            .push(case);
    }
    for (category, mut cases) in categories {
        let _ = writeln!(out, "\n## {}\n", escape(category));
        cases.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
        for case in cases {
            let _ = writeln!(
                out,
                "### {} — {}\n\nStatus: **{:?}**. {}\n\n{}\n\nDiagnostic hypothesis (not a causal finding): {}\n",
                escape(&case.spec.id),
                escape(&case.spec.title),
                case.status,
                escape(
                    case.reason
                        .as_deref()
                        .unwrap_or("All configured measurements collected.")
                ),
                escape(&case.spec.description),
                escape(&case.spec.diagnostic)
            );
            let _ = writeln!(
                out,
                "| Metric | Unit | n | Median | 95% median CI | Min–max | MAD | Flagged, retained |\n|---|---|---:|---:|---|---|---:|---:|"
            );
            for metric in summary
                .metrics
                .iter()
                .filter(|metric| metric.case_id == case.spec.id)
            {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {:.6} | [{:.6}, {:.6}] | {:.6}–{:.6} | {:.6} | {} |",
                    escape(&metric.metric),
                    escape(&metric.unit),
                    metric.count,
                    metric.median,
                    metric.ci_low,
                    metric.ci_high,
                    metric.min,
                    metric.max,
                    metric.mad,
                    metric.outliers
                );
                for warning in &metric.warnings {
                    let _ = writeln!(out, "\n- {}", escape(warning));
                }
            }
        }
    }
    for id in &run.case_order {
        if !run.outcomes.iter().any(|case| &case.spec.id == id) {
            let _ = writeln!(
                out,
                "\n- **Unmeasured case:** {} (run {:?}).",
                escape(id),
                run.status
            );
        }
    }
    if !run.warnings.is_empty() {
        out.push_str("\n## Run warnings\n");
        for warning in &run.warnings {
            let _ = writeln!(out, "\n- {}", escape(warning));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::Sample;

    pub(crate) fn fixture(values: &[f64]) -> RunRecord {
        let mut config = crate::config::resolve(None, "standard").unwrap();
        config.samples = values.len();
        let spec = crate::workload::catalog().remove(0);
        let id = spec.id.clone();
        let samples = values
            .iter()
            .enumerate()
            .map(|(index, value)| Sample {
                case_id: id.clone(),
                index,
                metric: "write_acceptance_bytes_per_second".into(),
                value: *value,
                unit: "bytes/s".into(),
                elapsed_ns: 100,
                bytes: 100,
                operations: 1,
                write_calls: 1,
                missed_deadlines: 0,
            })
            .collect();
        RunRecord {
            schema_version: SCHEMA_VERSION,
            run_id: "test-run".into(),
            harness_version: "1".into(),
            label: "test terminal".into(),
            started_unix_ms: 1,
            status: Status::Complete,
            config,
            environment: BTreeMap::new(),
            capabilities: BTreeMap::new(),
            case_order: vec![id],
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
    fn median_handles_odd_even_and_extremes() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&mut [f64::MAX, f64::MAX]), f64::MAX);
        assert_eq!(
            median(&mut [f64::MIN_POSITIVE, f64::MIN_POSITIVE]),
            f64::MIN_POSITIVE
        );
    }

    #[test]
    fn deterministic_bootstrap_and_constant_fixture() {
        assert_eq!(bootstrap(&[7.0; 9], 123), (7.0, 7.0));
        assert_eq!(bootstrap(&[1.0, 9.0], 123), (1.0, 9.0));
        assert_eq!(Random(0).next(), 0xe220_a839_7b1d_cdaf);
        let values = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];
        assert_eq!(bootstrap(&values, 9), bootstrap(&values, 9));
        let (low, high) = bootstrap(&values, 9);
        assert!(low <= 5.0 && high >= 5.0 && low >= 1.0 && high <= 9.0);
        let summary = summarize(&fixture(&[10.0; 9])).unwrap();
        assert_eq!(summary.metrics[0].median, 10.0);
        assert_eq!(summary.metrics[0].mad, 0.0);
        assert!(summary.method.contains("seed="));
    }

    #[test]
    fn no_outlier_is_discarded() {
        let summary = summarize(&fixture(&[1.0, 1.0, 1.0, 1.0, 1000.0])).unwrap();
        let metric = &summary.metrics[0];
        assert_eq!(metric.count, 5);
        assert_eq!(metric.max, 1000.0);
        assert_eq!(metric.outliers, 1);
    }

    #[test]
    fn constant_relative_change_and_practical_band() {
        let a = fixture(&[100.0; 9]);
        let faster = compare(&a, &fixture(&[120.0; 9])).unwrap();
        assert_eq!(faster[0].change, Change::Faster);
        assert!((faster[0].ci_low.unwrap() - 20.0).abs() < 1e-10);
        assert_eq!(faster[0].ci_low, faster[0].ci_high);
        assert_eq!(
            compare(&a, &fixture(&[80.0; 9])).unwrap()[0].change,
            Change::Slower
        );
        assert_eq!(
            compare(&a, &fixture(&[102.0; 9])).unwrap()[0].change,
            Change::NoMeaningfulChange
        );
    }

    #[test]
    fn latency_direction_and_uncertainty() {
        let mut a = fixture(&[100.0; 9]);
        let mut b = fixture(&[80.0; 9]);
        for run in [&mut a, &mut b] {
            for sample in &mut run.outcomes[0].samples {
                sample.metric = "protocol_round_trip_ns".into();
                sample.unit = "ns".into();
            }
        }
        assert_eq!(compare(&a, &b).unwrap()[0].change, Change::Faster);
        let wide = fixture(&[1.0, 2.0, 3.0, 4.0, 100.0, 200.0, 300.0, 400.0, 500.0]);
        assert_eq!(
            compare(&fixture(&[100.0; 9]), &wide).unwrap()[0].change,
            Change::Inconclusive
        );
        assert_eq!(
            compare(&fixture(&[1.0; 3]), &fixture(&[2.0; 3])).unwrap()[0].change,
            Change::Inconclusive
        );
    }

    #[test]
    fn labels_and_terminal_identity_do_not_block_comparison() {
        let a = fixture(&[100.0; 9]);
        let mut b = a.clone();
        b.label = "different".into();
        b.environment.insert("TERM".into(), "different".into());
        b.capabilities
            .insert("identity".into(), "other terminal".into());
        b.config.profile = "quick".into();
        assert_eq!(
            compare(&a, &b).unwrap()[0].change,
            Change::NoMeaningfulChange
        );
        b.config.seed += 1;
        assert_eq!(compare(&a, &b).unwrap()[0].change, Change::Incompatible);
    }

    #[test]
    fn compatibility_checks_case_hash_revision_geometry_and_units() {
        let a = fixture(&[100.0; 9]);
        for field in 0..5 {
            let mut b = a.clone();
            match field {
                0 => b.outcomes[0].payload_hash = "b".repeat(64),
                1 => b.outcomes[0].spec.revision += 1,
                2 => b.config.geometry.columns += 1,
                3 => b.outcomes[0]
                    .samples
                    .iter_mut()
                    .for_each(|sample| sample.unit = "ns".into()),
                _ => b.config.synchronized_updates = true,
            }
            assert_eq!(compare(&a, &b).unwrap()[0].change, Change::Incompatible);
        }
    }

    #[test]
    fn missing_failed_unsupported_and_partial_are_explicit() {
        let a = fixture(&[100.0; 9]);
        let mut b = a.clone();
        b.status = Status::Cancelled;
        b.outcomes.clear();
        assert_eq!(compare(&a, &b).unwrap()[0].change, Change::Inconclusive);
        for status in [Status::Failed, Status::Partial, Status::Unsupported] {
            let mut b = a.clone();
            b.status = status;
            b.outcomes[0].status = status;
            b.outcomes[0].reason = Some("test stop".into());
            if status == Status::Unsupported {
                b.outcomes[0].samples.clear();
            }
            let expected = if status == Status::Unsupported {
                Change::Unsupported
            } else {
                Change::Inconclusive
            };
            assert_eq!(compare(&a, &b).unwrap()[0].change, expected);
        }
    }

    #[test]
    fn malformed_measurements_and_schema_are_rejected() {
        let a = fixture(&[100.0; 9]);
        for field in 0..9 {
            let mut b = a.clone();
            match field {
                0 => b.schema_version += 1,
                1 => b.config.geometry.columns = 0,
                2 => b.outcomes[0].samples[0].value = f64::NAN,
                3 => b.outcomes[0].samples[0].value = -1.0,
                4 => b.outcomes[0].samples[0].value = 0.0,
                5 => b.outcomes[0].samples[0].index = 1,
                6 => b.outcomes[0].samples[0].case_id = "wrong".into(),
                7 => b.outcomes[0].samples[0].bytes = u64::MAX,
                _ => {
                    b.outcomes[0].samples.pop();
                }
            }
            assert!(summarize(&b).is_err(), "field {field}");
        }
    }

    #[test]
    fn report_has_boundaries_status_categories_and_no_tail_claim() {
        let mut run = fixture(&[1.0; 3]);
        run.label = "<script>|unsafe".into();
        let text = report(&run, &summarize(&run).unwrap());
        assert!(text.contains("not visible rendering"));
        assert!(text.contains("not a causal finding"));
        assert!(text.contains("samples.jsonl"));
        assert!(text.contains("&lt;script&gt;&#124;unsafe"));
        assert!(text.contains("No p99 estimate"));
        assert!(!text.contains("<script>"));
    }
}
