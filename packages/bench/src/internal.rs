//! In-process diagnostics, deliberately incompatible with portable sink measurements.
//! No terminal, daemon, runtime attachment, or presentation fence is involved.
#![cfg(feature = "internal")]

use crate::model::{
    BenchResult, CaseOutcome, CaseSpec, Geometry, RunConfig, RunRecord, SCHEMA_VERSION, Sample,
    Status, Workload,
};
use bmux_terminal_grid::{GridLimits, TerminalGridStream, visible_text_lines};
use bmux_tui::ansi::write_ansi_frame;
use bmux_tui::buffer::Buffer;
use bmux_tui::geometry::Rect;
use bmux_tui::layout::{Constraint, Direction, split};
use bmux_tui::text::Line;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ADAPTER_REVISION: u32 = 1;
const PROCESS_QUANTUM: usize = 4096;
const SNAPSHOT_BUDGET: usize = 32 * 1024 * 1024;
const MAX_CELLS: usize = 32_768;
static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct Stop {
    status: Status,
    reason: String,
}
fn check(cancelled: &AtomicBool, deadline: Instant) -> Result<(), Stop> {
    if cancelled.load(Ordering::Relaxed) {
        Err(Stop {
            status: Status::Cancelled,
            reason: "internal diagnostics cancelled".into(),
        })
    } else if Instant::now() >= deadline {
        Err(Stop {
            status: Status::Partial,
            reason: "internal case deadline exceeded; interrupted sample omitted".into(),
        })
    } else {
        Ok(())
    }
}
fn failure(reason: impl Into<String>) -> Stop {
    Stop {
        status: Status::Failed,
        reason: reason.into(),
    }
}
fn ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn internal_spec(mut spec: CaseSpec) -> CaseSpec {
    spec.id = format!("internal-v{ADAPTER_REVISION}/{}", spec.id);
    spec.title = format!("Internal: {}", spec.title);
    spec.description = format!(
        "In-process parser/grid and plain-text TUI projection diagnostics. {}",
        spec.description
    );
    spec.diagnostic = "CPU diagnostics only; no terminal transport, compositor, GPU, or visible presentation timing.".into();
    spec
}
fn identity(workload: &Workload) -> String {
    let mut hash = Sha256::new();
    hash.update(b"bmux-internal-adapter-v1\0default-grid-limits\0plain-text-tui\0quantum-4096\0");
    hash.update(workload.payload_hash.as_bytes());
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn unsupported(workload: &Workload, geometry: Geometry) -> Option<String> {
    if workload.spec.requires_sync {
        Some("synchronized presentation capability cannot be measured by an in-process grid".into())
    } else if workload.interval_ns != 0 {
        Some("paced producer scheduling requires the portable runner; CPU replay would erase pacing semantics".into())
    } else if usize::from(geometry.columns) * usize::from(geometry.rows) > MAX_CELLS {
        Some(format!(
            "internal projection supports at most {MAX_CELLS} visible cells"
        ))
    } else {
        None
    }
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}
impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(bytes.len()).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("encoded byte count overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Stages {
    parser: Duration,
    snapshot: Duration,
    text_projection: Duration,
    paint: Duration,
    encode: Duration,
    payload_bytes: u64,
    process_calls: u64,
    encoded_bytes: u64,
}

fn verify_plain_logs(id: &str, lines: &[String], geometry: Geometry) -> Result<(), Stop> {
    // Small viewports can wrap away any fixed marker, so do not guess their contents.
    if id == "plain-logs"
        && geometry.columns >= 64
        && geometry.rows >= 2
        && !lines.iter().any(|line| line.contains("INFO bmux job="))
    {
        return Err(failure(
            "plain-logs semantic check failed: expected visible authored log marker",
        ));
    }
    Ok(())
}

fn stages(
    workload: &Workload,
    geometry: Geometry,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<Stages, Stop> {
    check(cancelled, deadline)?;
    // Construction/allocation and workload setup are deliberately outside parser timing.
    let mut stream =
        TerminalGridStream::new(geometry.columns, geometry.rows, GridLimits::default())
            .map_err(|e| failure(format!("create grid: {e}")))?;
    stream.process(&workload.setup);
    let result = (|| {
        check(cancelled, deadline)?;
        let started = Instant::now();
        let mut payload_bytes = 0_u64;
        let mut process_calls = 0_u64;
        for chunk in &workload.chunks {
            // Bound non-preemptible work while retaining original boundaries and parser continuity.
            for quantum in chunk.chunks(PROCESS_QUANTUM) {
                check(cancelled, deadline)?;
                stream.process(black_box(quantum));
                payload_bytes +=
                    u64::try_from(quantum.len()).map_err(|e| failure(e.to_string()))?;
                process_calls += 1;
            }
        }
        black_box(stream.grid());
        let parser = started.elapsed();
        check(cancelled, deadline)?;
        let started = Instant::now();
        let snapshot = stream
            .try_snapshot(0, usize::from(geometry.rows), SNAPSHOT_BUDGET)
            .ok_or_else(|| failure("visible snapshot exceeded 32 MiB admission budget"))?;
        black_box(&snapshot);
        let snapshot_time = started.elapsed();
        check(cancelled, deadline)?;
        let started = Instant::now();
        let lines = visible_text_lines(stream.grid(), 0, usize::from(geometry.rows));
        black_box(&lines);
        let text_projection = started.elapsed();
        check(cancelled, deadline)?;
        verify_plain_logs(&workload.spec.id, &lines, geometry)?;
        // Conversion/allocation of source Lines and destination buffer are not paint work.
        let source: Vec<Line> = lines.into_iter().map(Line::raw).collect();
        let area = Rect::new(0, 0, geometry.columns, geometry.rows);
        let mut buffer = Buffer::empty(area);
        check(cancelled, deadline)?;
        let started = Instant::now();
        let constraints = vec![Constraint::Length(1); source.len()];
        let rows = split(area, Direction::Vertical, &constraints);
        for (row, line) in rows.into_iter().zip(&source) {
            buffer.write_line(row, line);
        }
        black_box(buffer.cells());
        let paint = started.elapsed();
        check(cancelled, deadline)?;
        let mut writer = CountingWriter::default();
        let started = Instant::now();
        write_ansi_frame(&mut writer, &buffer, None)
            .map_err(|e| failure(format!("encode frame: {e}")))?;
        black_box(writer.bytes);
        let encode = started.elapsed();
        check(cancelled, deadline)?;
        Ok(Stages {
            parser,
            snapshot: snapshot_time,
            text_projection,
            paint,
            encode,
            payload_bytes,
            process_calls,
            encoded_bytes: writer.bytes,
        })
    })();
    // Reset is outside every measured phase, even after cancellation or semantic failure.
    stream.process(&workload.teardown);
    black_box(stream.grid());
    result
}

fn samples(workload: &Workload, case_id: &str, index: usize, stages: &Stages) -> Vec<Sample> {
    [
        (
            "parser_bytes_per_second",
            "bytes/s",
            stages.payload_bytes as f64 / stages.parser.as_secs_f64().max(1e-9),
            stages.parser,
            stages.payload_bytes,
            workload.logical_operations,
        ),
        (
            "grid_snapshot_ns",
            "ns",
            ns(stages.snapshot) as f64,
            stages.snapshot,
            0,
            1,
        ),
        (
            "grid_visible_text_ns",
            "ns",
            ns(stages.text_projection) as f64,
            stages.text_projection,
            0,
            1,
        ),
        (
            "tui_plain_text_layout_paint_ns",
            "ns",
            ns(stages.paint) as f64,
            stages.paint,
            0,
            1,
        ),
        (
            "tui_ansi_encode_ns",
            "ns",
            ns(stages.encode) as f64,
            stages.encode,
            stages.encoded_bytes,
            1,
        ),
    ]
    .into_iter()
    .map(|(metric, unit, value, elapsed, bytes, operations)| Sample {
        case_id: case_id.into(),
        index,
        metric: metric.into(),
        value,
        unit: unit.into(),
        elapsed_ns: ns(elapsed),
        bytes,
        operations,
        // This is not a write-acceptance measurement; no terminal writes occur.
        write_calls: 0,
        missed_deadlines: 0,
    })
    .collect()
}

fn measure(workload: &Workload, config: &RunConfig, cancelled: &AtomicBool) -> CaseOutcome {
    measure_with(workload, config, cancelled, stages)
}

fn measure_with(
    workload: &Workload,
    config: &RunConfig,
    cancelled: &AtomicBool,
    mut execute: impl FnMut(&Workload, Geometry, &AtomicBool, Instant) -> Result<Stages, Stop>,
) -> CaseOutcome {
    let mut outcome = CaseOutcome {
        spec: internal_spec(workload.spec.clone()),
        payload_hash: identity(workload),
        status: Status::Complete,
        reason: None,
        samples: Vec::new(),
    };
    if cancelled.load(Ordering::Relaxed) {
        outcome.status = Status::Cancelled;
        outcome.reason = Some("not started: cancellation requested".into());
        return outcome;
    }
    if let Some(reason) = unsupported(workload, config.geometry) {
        outcome.status = Status::Unsupported;
        outcome.reason = Some(reason);
        return outcome;
    }
    let Some(deadline) = Instant::now().checked_add(Duration::from_millis(config.max_case_ms))
    else {
        outcome.status = Status::Failed;
        outcome.reason = Some("case deadline overflow".into());
        return outcome;
    };
    for iteration in 0..config.warmup + config.samples {
        match execute(workload, config.geometry, cancelled, deadline) {
            Ok(stage) => {
                // No per-chunk records; exactly five scalar metrics per completed sample.
                black_box(stage.process_calls);
                if iteration >= config.warmup {
                    outcome.samples.extend(samples(
                        workload,
                        &outcome.spec.id,
                        iteration - config.warmup,
                        &stage,
                    ));
                }
            }
            Err(stop) => {
                outcome.status = stop.status;
                outcome.reason = Some(stop.reason);
                break;
            }
        }
    }
    outcome
}

fn aggregate(outcomes: &[CaseOutcome]) -> Status {
    if outcomes.iter().any(|o| o.status == Status::Failed) {
        Status::Failed
    } else if outcomes.iter().any(|o| o.status == Status::Cancelled) {
        Status::Cancelled
    } else if outcomes.iter().any(|o| o.status == Status::Partial) {
        Status::Partial
    } else if outcomes.iter().all(|o| o.status == Status::Unsupported) {
        Status::Unsupported
    } else if outcomes.iter().any(|o| o.status == Status::Unsupported) {
        Status::Partial
    } else {
        Status::Complete
    }
}

/// Measure isolated CPU stages with no daemon, terminal descriptor, or external effects.
/// Stage calls are not preemptible; deadlines are checked around bounded work units.
pub fn run(config: RunConfig, label: String, cancelled: &AtomicBool) -> BenchResult<RunRecord> {
    crate::config::validate(&config)?;
    if label.len() > 256 || label.chars().any(char::is_control) {
        return Err("label must be at most 256 bytes without control characters".into());
    }
    let selected = crate::config::select(&config)?;
    let started_unix_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
    )
    .map_err(|e| e.to_string())?;
    let mut outcomes = Vec::with_capacity(selected.len());
    for spec in selected {
        match crate::workload::generate(
            &spec.id,
            config.geometry,
            config.seed,
            config.max_bytes_per_sample,
        ) {
            Ok(workload) => outcomes.push(measure(&workload, &config, cancelled)),
            Err(reason) => outcomes.push(CaseOutcome {
                spec: internal_spec(spec),
                payload_hash: String::new(),
                status: Status::Failed,
                reason: Some(reason),
                samples: Vec::new(),
            }),
        }
    }
    let mut environment = BTreeMap::new();
    environment.insert("os".into(), std::env::consts::OS.into());
    environment.insert("arch".into(), std::env::consts::ARCH.into());
    environment.insert("backend".into(), "bmux-internal-cpu-v1".into());
    environment.insert("harness_source_sha256".into(), crate::SOURCE_SHA256.into());
    environment.insert("harness_git_revision".into(), crate::GIT_REVISION.into());
    let capabilities = BTreeMap::from([
        ("backend".into(), "TerminalGridStream + bmux_tui plain-text Buffer + counting ANSI writer".into()),
        ("parser_bytes_per_second".into(), "bytes/s; higher better; process payload only; includes parser/screen-state mutation and bounded checks, excludes construction/setup".into()),
        ("grid_snapshot_ns".into(), "ns; lower better; try_snapshot(0, visible rows), including parser prefix; 32 MiB budget".into()),
        ("grid_visible_text_ns".into(), "ns; lower better; visible_text_lines plain-text extraction".into()),
        ("tui_plain_text_layout_paint_ns".into(), "ns; lower better; one-row split + Buffer::write_line; preallocated buffer and Line inputs; no grid styles".into()),
        ("tui_ansi_encode_ns".into(), "ns; lower better; full-frame ANSI encoding to counting writer, no terminal or transport".into()),
        ("visible_latency".into(), "unsupported: CPU stages are not visible presentation".into()),
        ("protocol_probe".into(), "unsupported: no terminal/protocol peer; config.probe has no effect".into()),
        ("grid_limits".into(), "GridLimits::default(); at most 32768 visible cells".into()),
        ("process_quantum".into(), "original workload chunks subdivided at 4096 bytes; streaming parser state retained".into()),
    ]);
    Ok(RunRecord { schema_version: SCHEMA_VERSION,
        run_id: format!("internal-{started_unix_ms}-{}-{}", std::process::id(), NEXT_RUN.fetch_add(1, Ordering::Relaxed)),
        harness_version: env!("CARGO_PKG_VERSION").into(), label, started_unix_ms,
        status: aggregate(&outcomes), case_order: outcomes.iter().map(|o| o.spec.id.clone()).collect(), config, environment, capabilities, outcomes,
        warnings: vec![
            "Internal CPU diagnostics cannot be compared to portable write acceptance or visible rendering latency; case IDs and payload hashes are adapter-version separated.".into(),
            "Parser construction, setup, teardown, source Line conversion and Buffer allocation are outside timed phases. Parser rate includes screen-state mutation and bounded cancellation checks.".into(),
            "TUI projection intentionally drops grid style metadata; this measures neutral plain-text layout/paint and full-frame encoding, not BMUX's retained compositor or application layout.".into(),
            "No pacing sleeps, protocol replies, real terminal, daemon, GPU, or compositor. Synchronized capability and paced cases are unsupported; probe setting is ignored.".into(),
            "Deadlines are cooperative between 4096-byte parser calls and bounded visible-grid phases; an in-flight library call cannot be preempted. Completed sample groups survive interruption.".into(),
        ] })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> RunConfig {
        let mut config = crate::config::resolve(None, "quick").unwrap();
        config.suites.clear();
        config.cases = vec!["plain-logs".into()];
        config.warmup = 0;
        config.samples = 2;
        config.max_bytes_per_sample = 32768;
        config.max_case_ms = 60_000;
        config.geometry = Geometry::default();
        config
    }
    #[test]
    fn actual_stages_have_distinct_honest_metrics_and_identity() {
        let config = config();
        let record = run(config.clone(), "internal".into(), &AtomicBool::new(false)).unwrap();
        assert_eq!(record.status, Status::Complete);
        let outcome = &record.outcomes[0];
        assert_eq!(outcome.spec.id, "internal-v1/plain-logs");
        assert_eq!(outcome.samples.len(), 10);
        let portable = crate::workload::generate(
            "plain-logs",
            config.geometry,
            config.seed,
            config.max_bytes_per_sample,
        )
        .unwrap();
        assert_ne!(outcome.payload_hash, portable.payload_hash);
        assert_eq!(outcome.payload_hash.len(), 64);
        for sample in &outcome.samples {
            assert!(sample.value.is_finite() && sample.value >= 0.0);
            assert_eq!(sample.write_calls, 0);
            assert_eq!(sample.missed_deadlines, 0);
            assert!(!sample.metric.contains("acceptance") && !sample.metric.contains("round_trip"));
            assert_eq!(
                sample.unit,
                if sample.metric == "parser_bytes_per_second" {
                    "bytes/s"
                } else {
                    "ns"
                }
            );
        }
        assert_eq!(record.case_order, ["internal-v1/plain-logs"]);
        assert_eq!(record.environment.len(), 5);
        assert_eq!(
            record.environment["harness_source_sha256"],
            crate::SOURCE_SHA256
        );
    }
    #[test]
    fn warmups_do_not_generate_records() {
        let mut config = config();
        config.warmup = 2;
        config.samples = 1;
        let record = run(config, String::new(), &AtomicBool::new(false)).unwrap();
        assert_eq!(record.outcomes[0].samples.len(), 5);
        assert!(record.outcomes[0].samples.iter().all(|s| s.index == 0));
    }
    #[test]
    fn cancellation_is_explicit_and_has_no_samples() {
        let record = run(config(), String::new(), &AtomicBool::new(true)).unwrap();
        assert_eq!(record.status, Status::Cancelled);
        assert!(record.outcomes[0].samples.is_empty());
    }
    #[test]
    fn checks_distinguish_deadline_and_cancellation() {
        let expired = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert_eq!(
            check(&AtomicBool::new(false), expired).unwrap_err().status,
            Status::Partial
        );
        assert_eq!(
            check(&AtomicBool::new(true), expired).unwrap_err().status,
            Status::Cancelled
        );
    }
    #[test]
    fn invalid_bounds_fail_before_work() {
        let mut invalid = config();
        invalid.samples = usize::MAX;
        assert!(run(invalid, String::new(), &AtomicBool::new(false)).is_err());
        let mut invalid = config();
        invalid.max_bytes_per_sample = usize::MAX;
        assert!(run(invalid, String::new(), &AtomicBool::new(false)).is_err());
        assert!(run(config(), "\x1b[2J".into(), &AtomicBool::new(false)).is_err());
    }
    #[test]
    fn capability_and_pacing_are_not_fake_cpu_measurements() {
        let mut config = config();
        config.synchronized_updates = true;
        config.cases = vec!["paced-updates".into(), "frames-synchronized".into()];
        let record = run(config, String::new(), &AtomicBool::new(false)).unwrap();
        assert_eq!(record.status, Status::Unsupported);
        assert!(
            record
                .outcomes
                .iter()
                .all(|o| o.status == Status::Unsupported && o.samples.is_empty())
        );
    }
    #[test]
    fn semantic_check_catches_empty_plain_log_grid() {
        assert!(verify_plain_logs("plain-logs", &[String::new()], Geometry::default()).is_err());
        assert!(
            verify_plain_logs(
                "plain-logs",
                &["INFO bmux job=0123".into()],
                Geometry::default()
            )
            .is_ok()
        );
    }
    #[test]
    fn counting_writer_does_not_retain_encoded_output() {
        let mut writer = CountingWriter::default();
        writer.write_all(b"abc").unwrap();
        writer.write_all(b"xyz").unwrap();
        assert_eq!(writer.bytes, 6);
        assert_eq!(std::mem::size_of_val(&writer), std::mem::size_of::<u64>());
    }
    #[test]
    fn interrupted_stage_never_claims_complete() {
        let config = config();
        let workload = crate::workload::generate(
            "plain-logs",
            config.geometry,
            config.seed,
            config.max_bytes_per_sample,
        )
        .unwrap();
        let result = stages(
            &workload,
            config.geometry,
            &AtomicBool::new(false),
            Instant::now(),
        );
        assert!(matches!(
            result,
            Err(Stop {
                status: Status::Partial,
                ..
            })
        ));
    }
    #[test]
    fn oversized_visible_geometry_is_explicitly_unsupported() {
        let config = config();
        let workload = crate::workload::generate(
            "plain-logs",
            config.geometry,
            config.seed,
            config.max_bytes_per_sample,
        )
        .unwrap();
        assert!(
            unsupported(
                &workload,
                Geometry {
                    columns: 256,
                    rows: 256
                }
            )
            .unwrap()
            .contains("visible cells")
        );
    }
    #[test]
    fn stream_retains_fragmented_escape_semantics() {
        let mut stream = TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();
        for byte in b"\x1b[2;3H\x1b[32mBMUX\x1b[0m" {
            stream.process(std::slice::from_ref(byte));
        }
        let lines = visible_text_lines(stream.grid(), 0, 24);
        assert!(lines[1].starts_with("  BMUX"));
    }
    #[test]
    fn probe_opt_in_never_creates_protocol_or_visible_latency_metrics() {
        let mut config = config();
        config.probe = true;
        config.samples = 1;
        let record = run(config, String::new(), &AtomicBool::new(false)).unwrap();
        assert_eq!(record.status, Status::Complete);
        assert_eq!(record.outcomes[0].samples.len(), 5);
        assert!(record.capabilities["protocol_probe"].starts_with("unsupported"));
        assert!(record.capabilities["visible_latency"].starts_with("unsupported"));
    }
    #[test]
    fn completed_samples_survive_later_interruption() {
        let config = config();
        let workload = crate::workload::generate(
            "plain-logs",
            config.geometry,
            config.seed,
            config.max_bytes_per_sample,
        )
        .unwrap();
        for status in [Status::Partial, Status::Cancelled, Status::Failed] {
            let mut iteration = 0;
            let outcome = measure_with(
                &workload,
                &config,
                &AtomicBool::new(false),
                |workload, geometry, cancelled, deadline| {
                    iteration += 1;
                    if iteration == 1 {
                        stages(workload, geometry, cancelled, deadline)
                    } else {
                        Err(Stop {
                            status,
                            reason: "injected interruption".into(),
                        })
                    }
                },
            );
            assert_eq!(outcome.status, status);
            assert_eq!(outcome.samples.len(), 5);
            assert!(outcome.samples.iter().all(|sample| sample.index == 0));
        }
    }

    #[test]
    fn payload_accounting_ignores_setup_and_teardown() {
        let config = config();
        let workload = crate::workload::generate(
            "plain-logs",
            config.geometry,
            config.seed,
            config.max_bytes_per_sample,
        )
        .unwrap();
        let stage = stages(
            &workload,
            config.geometry,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            stage.payload_bytes,
            workload.chunks.iter().map(|c| c.len() as u64).sum::<u64>()
        );
        assert!(stage.encoded_bytes > 0 && stage.process_calls > 0);
    }
}
