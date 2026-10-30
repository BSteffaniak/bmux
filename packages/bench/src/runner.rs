//! Bounded producer measurements. Neither write acceptance nor CPR proves visibility.
use crate::model::{
    BenchResult, CaseOutcome, Geometry, Progress, RunConfig, RunRecord, SCHEMA_VERSION, Sample,
    Status, Workload,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const IO_LIMIT: Duration = Duration::from_millis(100);
const CLEANUP_LIMIT: Duration = Duration::from_millis(250);

pub trait Backend {
    fn geometry(&mut self) -> BenchResult<Geometry>;
    fn write(&mut self, bytes: &[u8], timeout: Duration) -> BenchResult<()>;
    /// A bounded protocol round trip, never a rendering fence; None means unsupported/absent.
    fn probe(&mut self, timeout: Duration) -> BenchResult<Option<Duration>>;
    fn restore(&mut self) -> BenchResult<()>;
}

// Also restores on frontend callback panic. Explicit cleanup reports errors; Drop is a retry.
struct Session<'a, B: Backend> {
    backend: &'a mut B,
    cleanup: Vec<u8>,
    restored: bool,
}
impl<B: Backend> Session<'_, B> {
    fn cleanup(&mut self) -> BenchResult<()> {
        if self.cleanup.is_empty() {
            return Ok(());
        }
        self.backend.write(&self.cleanup, CLEANUP_LIMIT)?;
        self.cleanup.clear();
        Ok(())
    }
    fn finish(&mut self) -> BenchResult<()> {
        let cleanup = self.cleanup();
        let restore = self.backend.restore();
        self.restored = restore.is_ok();
        match (cleanup, restore) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
            (Err(a), Err(b)) => Err(format!("{a}; restoration: {b}")),
        }
    }
}
impl<B: Backend> Drop for Session<'_, B> {
    fn drop(&mut self) {
        let _ = self.cleanup();
        if !self.restored {
            let _ = self.backend.restore();
        }
    }
}

#[derive(Debug)]
struct Stop {
    status: Status,
    reason: String,
}
impl Stop {
    fn failed(reason: String) -> Self {
        Self {
            status: Status::Failed,
            reason,
        }
    }
}

fn checkpoint(cancelled: &AtomicBool, deadline: Instant) -> Result<(), Stop> {
    if cancelled.load(Ordering::Relaxed) {
        Err(Stop {
            status: Status::Cancelled,
            reason: "frontend requested cancellation".into(),
        })
    } else if Instant::now() >= deadline {
        Err(Stop {
            status: Status::Partial,
            reason: "case deadline exceeded".into(),
        })
    } else {
        Ok(())
    }
}
fn budget(deadline: Instant, limit: Duration) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .min(limit)
}
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

struct Measurement {
    elapsed: Duration,
    write_time: Duration,
    bytes: u64,
    calls: u64,
    misses: u64,
    probe: Option<Duration>,
    baseline_probe: Option<Duration>,
}

fn emit<B: Backend>(
    backend: &mut B,
    workload: &Workload,
    config: &RunConfig,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<Measurement, Stop> {
    let started = Instant::now();
    let mut measured = Measurement {
        elapsed: Duration::ZERO,
        write_time: Duration::ZERO,
        bytes: 0,
        calls: 0,
        misses: 0,
        probe: None,
        baseline_probe: None,
    };
    for (index, chunk) in workload.chunks.iter().enumerate() {
        checkpoint(cancelled, deadline)?;
        if workload.interval_ns != 0 {
            let offset = workload
                .interval_ns
                .checked_mul(
                    u64::try_from(index)
                        .map_err(|_| Stop::failed("chunk index overflow".into()))?,
                )
                .ok_or_else(|| Stop::failed("pacing interval overflow".into()))?;
            let target = started
                .checked_add(Duration::from_nanos(offset))
                .ok_or_else(|| Stop::failed("pacing deadline overflow".into()))?;
            while Instant::now() < target {
                checkpoint(cancelled, deadline)?;
                std::thread::sleep(
                    target
                        .saturating_duration_since(Instant::now())
                        .min(budget(deadline, Duration::from_millis(5))),
                );
            }
            // A miss means at least one entire producer interval late, not nanosecond jitter.
            if Instant::now().saturating_duration_since(target)
                >= Duration::from_nanos(workload.interval_ns)
            {
                measured.misses += 1;
            }
        }
        checkpoint(cancelled, deadline)?;
        let write_started = Instant::now();
        backend
            .write(chunk, budget(deadline, IO_LIMIT))
            .map_err(Stop::failed)?;
        measured.write_time += write_started.elapsed();
        measured.bytes +=
            u64::try_from(chunk.len()).map_err(|_| Stop::failed("byte count overflow".into()))?;
        measured.calls += 1;
        checkpoint(cancelled, deadline)?;
    }
    measured.elapsed = started.elapsed();
    if config.probe {
        checkpoint(cancelled, deadline)?;
        measured.probe = backend
            .probe(budget(
                deadline,
                Duration::from_millis(config.probe_timeout_ms),
            ))
            .map_err(Stop::failed)?;
        checkpoint(cancelled, deadline)?;
    }
    Ok(measured)
}

fn append_samples(outcome: &mut CaseOutcome, workload: &Workload, index: usize, m: &Measurement) {
    let mut add = |metric: &str, unit: &str, value: f64, elapsed: Duration| {
        outcome.samples.push(Sample {
            case_id: workload.spec.id.clone(),
            index,
            metric: metric.into(),
            value,
            unit: unit.into(),
            elapsed_ns: nanos(elapsed),
            bytes: m.bytes,
            operations: workload.logical_operations,
            write_calls: m.calls,
            missed_deadlines: m.misses,
        });
    };
    if workload.interval_ns == 0 {
        add(
            "write_acceptance_bytes_per_second",
            "bytes/s",
            m.bytes as f64 / m.elapsed.as_secs_f64().max(1e-9),
            m.elapsed,
        );
    } else {
        add(
            "producer_write_ns",
            "ns",
            nanos(m.write_time) as f64,
            m.write_time,
        );
        add(
            "emission_deadline_misses",
            "count",
            m.misses as f64,
            m.elapsed,
        );
    }
    if let Some(round_trip) = m.baseline_probe {
        add(
            "protocol_baseline_round_trip_ns",
            "ns",
            nanos(round_trip) as f64,
            round_trip,
        );
    }
    if let Some(round_trip) = m.probe {
        add(
            "protocol_round_trip_ns",
            "ns",
            nanos(round_trip) as f64,
            round_trip,
        );
    }
}

fn measure_case<B: Backend>(
    session: &mut Session<'_, B>,
    workload: &Workload,
    config: &RunConfig,
    cancelled: &AtomicBool,
    progress: &mut impl FnMut(Progress),
) -> CaseOutcome {
    let mut outcome = CaseOutcome {
        spec: workload.spec.clone(),
        payload_hash: workload.payload_hash.clone(),
        status: Status::Complete,
        reason: None,
        samples: Vec::new(),
    };
    if workload.spec.requires_sync && !config.synchronized_updates {
        outcome.status = Status::Unsupported;
        outcome.reason = Some(
            "synchronized updates require explicit opt-in; TERM is not capability evidence".into(),
        );
        return outcome;
    }
    let Some(deadline) = Instant::now().checked_add(Duration::from_millis(config.max_case_ms))
    else {
        outcome.status = Status::Failed;
        outcome.reason = Some("case deadline overflow".into());
        return outcome;
    };
    for iteration in 0..config.warmup + config.samples {
        let result = (|| {
            checkpoint(cancelled, deadline)?;
            if session.backend.geometry().map_err(Stop::failed)? != config.geometry {
                return Err(Stop::failed("geometry changed; case invalidated".into()));
            }
            if iteration < config.warmup {
                progress(Progress::Warmup {
                    id: workload.spec.id.clone(),
                });
            } else {
                progress(Progress::Sampling {
                    id: workload.spec.id.clone(),
                });
            }
            checkpoint(cancelled, deadline)?;
            // Register cleanup before setup: a failed/partial setup may have changed owned modes.
            let baseline_probe = if config.probe {
                session
                    .backend
                    .probe(budget(
                        deadline,
                        Duration::from_millis(config.probe_timeout_ms),
                    ))
                    .map_err(Stop::failed)?
            } else {
                None
            };
            session.cleanup.clone_from(&workload.teardown);
            session
                .backend
                .write(&workload.setup, budget(deadline, IO_LIMIT))
                .map_err(Stop::failed)?;
            let measurement = emit(session.backend, workload, config, cancelled, deadline);
            let geometry = session.backend.geometry().map_err(Stop::failed);
            let cleanup = session.cleanup().map_err(Stop::failed);
            // Geometry/cleanup failures take precedence over a successful emission.
            cleanup?;
            if geometry? != config.geometry {
                return Err(Stop::failed("geometry changed; case invalidated".into()));
            }
            let mut measurement = measurement?;
            measurement.baseline_probe = baseline_probe;
            if iteration >= config.warmup {
                append_samples(
                    &mut outcome,
                    workload,
                    iteration - config.warmup,
                    &measurement,
                );
                if config.probe && measurement.probe.is_none() {
                    outcome.status = Status::Partial;
                    outcome.reason = Some("protocol probe unsupported or no bounded reply; write metrics are not visible latency".into());
                }
            }
            Ok(())
        })();
        if let Err(stop) = result {
            outcome.status = if stop.status == Status::Failed
                && (!outcome.samples.is_empty() || Instant::now() >= deadline)
            {
                Status::Partial
            } else {
                stop.status
            };
            if stop.reason.starts_with("geometry changed") {
                outcome.status = Status::Failed;
                outcome.samples.clear();
            }
            outcome.reason = Some(stop.reason);
            if let Err(cleanup) = session.cleanup() {
                outcome.status = Status::Failed;
                outcome.reason = Some(format!(
                    "{}; teardown failed: {cleanup}",
                    outcome.reason.as_deref().unwrap_or_default()
                ));
            }
            break;
        }
    }
    outcome
}

fn overall(outcomes: &[CaseOutcome]) -> Status {
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

fn environment() -> (BTreeMap<String, String>, Vec<String>) {
    let mut values = BTreeMap::new();
    for name in [
        "TERM",
        "COLORTERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "LANG",
        "LC_ALL",
    ] {
        if let Ok(value) = std::env::var(name) {
            values.insert(
                name.into(),
                value
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect(),
            );
        }
    }
    values.insert("os".into(), std::env::consts::OS.into());
    values.insert("harness_source_sha256".into(), crate::SOURCE_SHA256.into());
    values.insert("harness_git_revision".into(), crate::GIT_REVISION.into());
    values.insert(
        "terminal_identity_confidence".into(),
        "environment hints only; version/configuration require operator confirmation".into(),
    );
    values.insert("arch".into(), std::env::consts::ARCH.into());
    values.insert(
        "harness_build".into(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
        .into(),
    );
    if let Ok(parallelism) = std::thread::available_parallelism() {
        values.insert("logical_parallelism".into(), parallelism.to_string());
    }
    let mut warnings = vec!["Write acceptance measures producer/transport acceptance, not visible rendering. CPR is a protocol response, not a rendering fence.".into(),
        "Visible latency is unsupported: no portable bounded presentation fence is available.".into(),
        "Paced deadline misses count emissions at least one full producer interval late.".into()];
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        warnings.push("SSH transport may affect measurements.".into());
    }
    if ["TMUX", "STY", "BMUX", "BMUX_SESSION"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        warnings.push("Nested terminal multiplexer detected; results include its transport and protocol behavior.".into());
    }
    (values, warnings)
}

/// Run against the caller's backend; no daemon or process is launched.
/// Even configuration/selection failures attempt terminal restoration.
pub fn run(
    backend: &mut impl Backend,
    config: RunConfig,
    label: String,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> BenchResult<RunRecord> {
    let mut session = Session {
        backend,
        cleanup: Vec::new(),
        restored: false,
    };
    let result = (|| {
        crate::config::validate(&config)?;
        if label.len() > 256 || label.chars().any(char::is_control) {
            return Err("label must be at most 256 bytes without control characters".into());
        }
        let selected = crate::config::select(&config)?;
        // Keep stable catalog order; the seed controls payloads, not implicit shuffling.
        let catalog = crate::workload::catalog();
        if selected.iter().any(|spec| {
            !catalog
                .iter()
                .any(|entry| entry.id == spec.id && entry.revision == spec.revision)
        }) {
            return Err("selected case is not in the current workload catalog".into());
        }
        let started_unix_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_millis(),
        )
        .map_err(|_| "wall-clock timestamp overflow")?;
        let (environment, mut warnings) = environment();
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            "visible_latency".into(),
            "unsupported: no presentation fence".into(),
        );
        capabilities.insert(
            "protocol_probe".into(),
            if config.probe {
                "requested: bounded DSR cursor report, not a rendering fence"
            } else {
                "disabled"
            }
            .into(),
        );
        capabilities.insert(
            "synchronized_updates".into(),
            if config.synchronized_updates {
                "explicit user opt-in; not auto-detected"
            } else {
                "disabled"
            }
            .into(),
        );
        capabilities.insert(
            "case_order".into(),
            "stable catalog order; config.seed determines workload bytes".into(),
        );
        capabilities.insert(
            "write_acceptance_bytes_per_second".into(),
            "bytes/s; higher is better; acceptance only".into(),
        );
        capabilities.insert(
            "protocol_round_trip_ns".into(),
            "ns; lower is better; protocol only; paired with protocol_baseline_round_trip_ns"
                .into(),
        );
        let case_order = selected.iter().map(|spec| spec.id.clone()).collect();
        let mut outcomes = Vec::with_capacity(selected.len());
        let mut interrupted = false;
        for (index, spec) in selected.iter().enumerate() {
            let workload = crate::workload::generate(
                &spec.id,
                config.geometry,
                config.seed,
                config.max_bytes_per_sample,
            );
            let outcome = match workload {
                Err(reason) => CaseOutcome {
                    spec: spec.clone(),
                    payload_hash: String::new(),
                    status: Status::Failed,
                    reason: Some(reason),
                    samples: Vec::new(),
                },
                Ok(workload) => {
                    if interrupted || cancelled.load(Ordering::Relaxed) {
                        CaseOutcome {
                            spec: workload.spec,
                            payload_hash: workload.payload_hash,
                            status: Status::Cancelled,
                            reason: Some("not started after interruption".into()),
                            samples: Vec::new(),
                        }
                    } else {
                        progress(Progress::CaseStarted {
                            id: spec.id.clone(),
                            index,
                            total: selected.len(),
                        });
                        measure_case(&mut session, &workload, &config, cancelled, &mut progress)
                    }
                }
            };
            interrupted |= outcome.status == Status::Cancelled;
            progress(Progress::CaseFinished {
                id: spec.id.clone(),
                status: outcome.status,
            });
            outcomes.push(outcome);
            // A failed teardown must not be followed by another case.
            if !session.cleanup.is_empty() {
                interrupted = true;
            }
        }
        let mut status = overall(&outcomes);
        if let Err(reason) = session.finish() {
            warnings.push(format!("Terminal teardown/restoration failed: {reason}"));
            status = Status::Failed;
        }
        Ok(RunRecord {
            schema_version: SCHEMA_VERSION,
            run_id: format!(
                "{started_unix_ms}-{}-{:016x}",
                std::process::id(),
                config.seed
            ),
            harness_version: env!("CARGO_PKG_VERSION").into(),
            label,
            started_unix_ms,
            status,
            config,
            environment,
            capabilities,
            case_order,
            outcomes,
            warnings,
        })
    })();
    if result.is_err()
        && let Err(cleanup) = session.finish()
    {
        return Err(format!(
            "{}; terminal restoration failed: {cleanup}",
            result.err().unwrap_or_default()
        ));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct Fake {
        writes: Vec<Vec<u8>>,
        geometry: Geometry,
        reads: usize,
        resize_at: Option<usize>,
        fail_at: Option<usize>,
        fail_restore: bool,
        fail_cleanup: bool,
        restores: usize,
        reply: Option<Duration>,
        io_delay: Duration,
        setup_delay: Duration,
        during_write: Rc<Cell<bool>>,
        cancel_on_payload: Option<std::sync::Arc<AtomicBool>>,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                writes: Vec::new(),
                geometry: Geometry::default(),
                reads: 0,
                resize_at: None,
                fail_at: None,
                fail_restore: false,
                fail_cleanup: false,
                restores: 0,
                reply: Some(Duration::from_micros(12)),
                io_delay: Duration::ZERO,
                setup_delay: Duration::ZERO,
                during_write: Rc::new(Cell::new(false)),
                cancel_on_payload: None,
            }
        }
    }
    impl Backend for Fake {
        fn geometry(&mut self) -> BenchResult<Geometry> {
            self.reads += 1;
            if self.resize_at == Some(self.reads) {
                self.geometry.columns += 1;
            }
            Ok(self.geometry)
        }
        fn write(&mut self, bytes: &[u8], timeout: Duration) -> BenchResult<()> {
            assert!(timeout <= CLEANUP_LIMIT);
            self.during_write.set(true);
            self.writes.push(bytes.to_vec());
            let result = if self.fail_at == Some(self.writes.len())
                || (self.fail_cleanup && bytes == b"reset")
            {
                Err("injected write deadline failure".into())
            } else {
                if bytes == b"setup" || bytes == b"reset" {
                    std::thread::sleep(self.setup_delay);
                } else {
                    std::thread::sleep(self.io_delay);
                    if let Some(cancelled) = &self.cancel_on_payload {
                        cancelled.store(true, Ordering::Relaxed);
                    }
                }
                Ok(())
            };
            self.during_write.set(false);
            result
        }
        fn probe(&mut self, _: Duration) -> BenchResult<Option<Duration>> {
            Ok(self.reply)
        }
        fn restore(&mut self) -> BenchResult<()> {
            self.restores += 1;
            if self.fail_restore {
                Err("injected restore failure".into())
            } else {
                Ok(())
            }
        }
    }
    fn config() -> RunConfig {
        let mut config = crate::config::resolve(None, "quick").unwrap();
        config.suites.clear();
        config.cases = vec!["plain-logs".into()];
        config.samples = 2;
        config.warmup = 0;
        config.max_case_ms = 1000;
        config.probe = false;
        config.geometry = Geometry::default();
        config
    }
    fn workload() -> Workload {
        let mut workload =
            crate::workload::generate("plain-logs", Geometry::default(), 7, 4096).unwrap();
        workload.setup = b"setup".to_vec();
        workload.teardown = b"reset".to_vec();
        workload.chunks = vec![b"payload".to_vec()];
        workload.logical_operations = 1;
        workload
    }
    fn case(
        fake: &mut Fake,
        workload: &Workload,
        config: &RunConfig,
        cancelled: &AtomicBool,
    ) -> CaseOutcome {
        let mut session = Session {
            backend: fake,
            cleanup: Vec::new(),
            restored: false,
        };
        let result = measure_case(&mut session, workload, config, cancelled, &mut |_| {});
        let _ = session.finish();
        result
    }
    #[test]
    fn timing_excludes_setup_reset_and_progress() {
        let mut fake = Fake {
            setup_delay: Duration::from_millis(15),
            ..Fake::default()
        };
        let mut config = config();
        config.samples = 1;
        let during_write = Rc::clone(&fake.during_write);
        let mut session = Session {
            backend: &mut fake,
            cleanup: Vec::new(),
            restored: false,
        };
        let wall_started = Instant::now();
        let outcome = measure_case(
            &mut session,
            &workload(),
            &config,
            &AtomicBool::new(false),
            &mut |_| {
                assert!(!during_write.get());
                std::thread::sleep(Duration::from_millis(15));
            },
        );
        assert_eq!(outcome.status, Status::Complete);
        assert!(
            wall_started
                .elapsed()
                .saturating_sub(Duration::from_nanos(outcome.samples[0].elapsed_ns))
                >= Duration::from_millis(45)
        );
        assert_eq!(
            session.backend.writes,
            [b"setup".to_vec(), b"payload".to_vec(), b"reset".to_vec()]
        );
    }
    #[test]
    fn cancellation_preserves_prior_samples_and_always_tears_down() {
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let mut fake = Fake::default();
        let mut session = Session {
            backend: &mut fake,
            cleanup: Vec::new(),
            restored: false,
        };
        let outcome = measure_case(&mut session, &workload(), &config(), &flag, &mut |_| {});
        assert_eq!(outcome.samples.len(), 2);
        session.finish().unwrap();
        drop(session);
        fake.cancel_on_payload = Some(std::sync::Arc::clone(&flag));
        let outcome = case(&mut fake, &workload(), &config(), &flag);
        assert_eq!(outcome.status, Status::Cancelled);
        assert_eq!(fake.writes.last().unwrap(), b"reset");
        assert!(fake.restores >= 2);
    }
    #[test]
    fn cancellation_before_second_sample_keeps_completed_first_sample() {
        let flag = AtomicBool::new(false);
        let mut fake = Fake::default();
        let mut session = Session {
            backend: &mut fake,
            cleanup: Vec::new(),
            restored: false,
        };
        let mut sampling = 0;
        let outcome = measure_case(&mut session, &workload(), &config(), &flag, &mut |event| {
            if matches!(event, Progress::Sampling { .. }) {
                sampling += 1;
                if sampling == 2 {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        });
        assert_eq!(outcome.status, Status::Cancelled);
        assert_eq!(outcome.samples.len(), 1);
        assert_eq!(session.backend.writes.last().unwrap(), b"reset");
        session.finish().unwrap();
    }

    #[test]
    fn second_sample_failure_retains_first() {
        let mut fake = Fake {
            fail_at: Some(5),
            ..Fake::default()
        };
        let outcome = case(&mut fake, &workload(), &config(), &AtomicBool::new(false));
        assert_eq!(outcome.status, Status::Partial);
        assert_eq!(outcome.samples.len(), 1);
        assert_eq!(fake.writes.last().unwrap(), b"reset");
    }
    #[test]
    fn setup_failure_still_attempts_teardown() {
        let mut fake = Fake {
            fail_at: Some(1),
            ..Fake::default()
        };
        let outcome = case(&mut fake, &workload(), &config(), &AtomicBool::new(false));
        assert_eq!(outcome.status, Status::Failed);
        assert_eq!(fake.writes, [b"setup".to_vec(), b"reset".to_vec()]);
        assert_eq!(fake.restores, 1);
    }
    #[test]
    fn geometry_change_invalidates_prior_samples() {
        let mut fake = Fake {
            resize_at: Some(4),
            ..Fake::default()
        };
        let outcome = case(&mut fake, &workload(), &config(), &AtomicBool::new(false));
        assert_eq!(outcome.status, Status::Failed);
        assert!(outcome.samples.is_empty());
    }
    #[test]
    fn deadline_is_explicit_partial() {
        let mut fake = Fake {
            io_delay: Duration::from_millis(5),
            ..Fake::default()
        };
        let mut config = config();
        config.max_case_ms = 1;
        let outcome = case(&mut fake, &workload(), &config, &AtomicBool::new(false));
        assert_eq!(outcome.status, Status::Partial);
        assert_eq!(fake.writes.last().unwrap(), b"reset");
    }
    #[test]
    fn probes_are_protocol_only_and_absence_is_not_zero_latency() {
        let mut config = config();
        config.probe = true;
        let mut fake = Fake {
            reply: None,
            ..Fake::default()
        };
        let absent = case(&mut fake, &workload(), &config, &AtomicBool::new(false));
        assert_eq!(absent.status, Status::Partial);
        assert!(
            absent
                .samples
                .iter()
                .all(|s| s.metric == "write_acceptance_bytes_per_second" && s.unit == "bytes/s")
        );
        fake.reply = Some(Duration::from_nanos(45));
        let present = case(&mut fake, &workload(), &config, &AtomicBool::new(false));
        assert_eq!(present.status, Status::Complete);
        assert!(
            present
                .samples
                .iter()
                .any(|s| s.metric == "protocol_round_trip_ns" && s.unit == "ns" && s.value == 45.0)
        );
        assert!(present.samples.iter().all(|s| !s.metric.contains("render")));
    }
    #[test]
    fn pacing_reports_producer_metrics_not_rendering_or_throughput() {
        let mut workload = workload();
        workload.interval_ns = 1_000_000;
        let outcome = case(
            &mut Fake::default(),
            &workload,
            &config(),
            &AtomicBool::new(false),
        );
        assert!(outcome.samples.iter().all(|s| {
            ["producer_write_ns", "emission_deadline_misses"].contains(&s.metric.as_str())
        }));
        assert_eq!(outcome.samples.len(), 4);
    }
    #[test]
    fn sync_requires_explicit_opt_in() {
        let mut workload = workload();
        workload.spec.requires_sync = true;
        let mut fake = Fake::default();
        let outcome = case(&mut fake, &workload, &config(), &AtomicBool::new(false));
        assert_eq!(outcome.status, Status::Unsupported);
        assert!(fake.writes.is_empty());
    }
    #[test]
    fn warmups_are_not_samples() {
        let mut config = config();
        config.warmup = 2;
        let outcome = case(
            &mut Fake::default(),
            &workload(),
            &config,
            &AtomicBool::new(false),
        );
        assert_eq!(outcome.samples.len(), config.samples);
        assert_eq!(outcome.samples[0].index, 0);
    }
    #[test]
    fn restore_failure_is_reported_without_losing_measurements() {
        let mut fake = Fake {
            fail_restore: true,
            ..Fake::default()
        };
        let record = run(
            &mut fake,
            config(),
            "test".into(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(record.status, Status::Failed);
        assert!(!record.outcomes[0].samples.is_empty());
        assert!(
            record
                .warnings
                .iter()
                .any(|w| w.contains("restore failure"))
        );
        assert!(fake.restores >= 2);
    }
    #[test]
    fn invalid_config_and_pre_cancel_restore_without_output() {
        let mut fake = Fake::default();
        let mut invalid = config();
        invalid.samples = usize::MAX;
        assert!(
            run(
                &mut fake,
                invalid,
                "test".into(),
                &AtomicBool::new(false),
                |_| {}
            )
            .is_err()
        );
        assert!(fake.writes.is_empty());
        assert!(fake.restores > 0);
        let record = run(
            &mut fake,
            config(),
            "test".into(),
            &AtomicBool::new(true),
            |_| {},
        )
        .unwrap();
        assert_eq!(record.status, Status::Cancelled);
        assert!(fake.writes.is_empty());
        assert_eq!(record.case_order, ["plain-logs"]);
        assert_eq!(record.config.seed, config().seed);
        assert!(record.capabilities["visible_latency"].starts_with("unsupported"));
    }
    #[test]
    fn teardown_failure_is_not_success() {
        let mut fake = Fake {
            fail_cleanup: true,
            ..Fake::default()
        };
        let outcome = case(&mut fake, &workload(), &config(), &AtomicBool::new(false));
        assert_eq!(outcome.status, Status::Failed);
        assert!(fake.restores > 0);
    }
    #[test]
    fn callback_panic_attempts_restoration() {
        let mut fake = Fake::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run(
                &mut fake,
                config(),
                "test".into(),
                &AtomicBool::new(false),
                |_| panic!("frontend"),
            );
        }));
        assert!(result.is_err());
        assert!(fake.restores > 0);
    }
}
