//! Reusable, daemon-free frontend for BMUX portable terminal benchmarks.
mod args;
mod signals;
mod ui;

use args::{Cli, Command, ReportFormat, RunArgs};
use bmux_bench::model::{RunConfig, RunRecord, Status};
use bmux_bench::runner::Backend;
use bmux_bench::{analysis, artifact, config, runner, terminal, workload};
use clap::Parser;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Run with process arguments. Does not exit the caller's process.
pub fn run() -> Result<u8, String> {
    run_from(std::env::args_os())
}

/// Run with a clap-compatible argv, including a program name as its first item.
/// Help/version return success. Errors are returned to the embedding application.
pub fn run_from<I, T>(args: I) -> Result<u8, String>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    dispatch(args, None)
}

/// Run without installing or changing any signal dispositions. The caller owns
/// cancellation and must arrange terminal-safe shutdown by setting this flag.
pub fn run_from_with_cancel<I, T>(args: I, cancelled: Arc<AtomicBool>) -> Result<u8, String>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    dispatch(args, Some(cancelled))
}

fn dispatch<I, T>(args: I, cancelled: Option<Arc<AtomicBool>>) -> Result<u8, String>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            return emit(&error.to_string()).map(|()| 0);
        }
        Err(error) => return Err(error.to_string()),
    };
    match cli.command {
        Command::List { json } => {
            let cases = workload::catalog();
            if json {
                emit_json(serde_json::to_string_pretty(&cases))?;
            } else {
                let mut text = String::from("CASE\tCATEGORY\tTITLE\n");
                for case in cases {
                    text.push_str(&format!(
                        "{}\t{}\t{}\n",
                        safe(&case.id),
                        safe(&case.category),
                        safe(&case.title)
                    ));
                }
                emit(&text)?;
            }
            Ok(0)
        }
        Command::Explain { case, json } => {
            let spec = workload::catalog()
                .into_iter()
                .find(|spec| spec.id == case)
                .ok_or_else(|| format!("unknown benchmark case: {}", safe(&case)))?;
            if json {
                emit_json(serde_json::to_string_pretty(&spec))?;
            } else {
                emit(&format!(
                    "{} — {}\nCategory: {}\nSuites: {}\nRevision: {}\n{}\nDiagnostic: {}\nSynchronized updates required: {}\n\nWrite acceptance and protocol replies do not prove visible rendering.\n",
                    safe(&spec.id),
                    safe(&spec.title),
                    safe(&spec.category),
                    safe(&spec.suites.join(", ")),
                    spec.revision,
                    safe(&spec.description),
                    safe(&spec.diagnostic),
                    spec.requires_sync
                ))?;
            }
            Ok(0)
        }
        Command::Run(args) => match cancelled {
            Some(cancelled) => execute_run_with_cancel(&args, cancelled, false),
            None => execute_run(&args),
        },
        Command::Compare {
            base,
            candidate,
            json,
        } => {
            let base = artifact::read_bundle(&base)?;
            let candidate = artifact::read_bundle(&candidate)?;
            let comparisons = analysis::compare(&base, &candidate)?;
            if json {
                emit_json(serde_json::to_string_pretty(&comparisons))?;
            } else {
                let mut text =
                    String::from("Comparison (uncertainty and compatibility checked per metric)\n");
                for item in comparisons {
                    text.push_str(&format!(
                        "{} / {}: {:?}{} — {}\n",
                        safe(&item.case_id),
                        safe(&item.metric),
                        item.change,
                        item.percent_change
                            .map_or_else(String::new, |p| format!(" ({p:+.2}%)")),
                        safe(&item.reason)
                    ));
                }
                emit(&text)?;
            }
            Ok(0)
        }
        Command::Report { run, format } => {
            let record = artifact::read_bundle(&run)?;
            match format {
                ReportFormat::Markdown => {
                    emit(&analysis::report(&record, &analysis::summarize(&record)?))?
                }
                ReportFormat::Json => {
                    emit_json(serde_json::to_string_pretty(&analysis::summarize(&record)?))?
                }
            }
            Ok(0)
        }
    }
}

fn resolve_run(args: &RunArgs) -> Result<RunConfig, String> {
    let source = args
        .config
        .as_ref()
        .map(|path| {
            let file = std::fs::File::open(path)
                .map_err(|e| format!("open config {}: {e}", path.display()))?;
            let mut text = String::new();
            file.take(65_537)
                .read_to_string(&mut text)
                .map_err(|e| format!("read config {}: {e}", path.display()))?;
            if text.len() > 65_536 {
                return Err("configuration exceeds 65536 bytes".into());
            }
            Ok::<_, String>(text)
        })
        .transpose()?;
    let mut resolved = config::resolve(source.as_deref(), &args.profile)?;
    if !args.category.is_empty() {
        resolved.suites.clone_from(&args.category);
    }
    if !args.cases.is_empty() {
        resolved.cases.clone_from(&args.cases);
        if args.category.is_empty() {
            resolved.suites.clear();
        }
    }
    if let Some(samples) = args.samples {
        resolved.samples = samples;
    }
    if let Some(seed) = args.seed {
        resolved.seed = seed;
    }
    if let Some(columns) = args.columns {
        resolved.geometry.columns = columns;
    }
    if let Some(rows) = args.rows {
        resolved.geometry.rows = rows;
    }
    if let Some(probe) = args.probe {
        resolved.probe = probe;
    }
    if let Some(sync) = args.synchronized_updates {
        resolved.synchronized_updates = sync;
    }
    config::validate(&resolved)?;
    // Validate selection before touching terminal modes or installing signal handling.
    config::select(&resolved)?;
    Ok(resolved)
}

fn execute_run(args: &RunArgs) -> Result<u8, String> {
    // Reject malformed invocations before acquiring process-wide signal ownership.
    if args.label.len() > 256 || args.label.chars().any(char::is_control) {
        return Err("label must be at most 256 bytes without control characters".into());
    }
    resolve_run(args)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut guard = signals::Guard::install(Arc::clone(&cancelled))?;
    let result = execute_run_with_cancel(args, cancelled, true);
    match (result, guard.restore()) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}; {cleanup}")),
    }
}

fn execute_run_with_cancel(
    args: &RunArgs,
    cancelled: Arc<AtomicBool>,
    scoped: bool,
) -> Result<u8, String> {
    if args.label.len() > 256 || args.label.chars().any(char::is_control) {
        return Err("label must be at most 256 bytes without control characters".into());
    }
    let mut resolved = resolve_run(args)?;
    // Portable defaults use the actual viewport. Explicit profile/CLI geometry
    // remains a precondition, never silently resized during a run.
    #[cfg(feature = "internal")]
    let portable = !args.internal;
    #[cfg(not(feature = "internal"))]
    let portable = true;
    if portable && args.config.is_none() && args.columns.is_none() && args.rows.is_none() {
        let (columns, rows) = crossterm::terminal::size()
            .map_err(|error| format!("detect terminal geometry: {error}"))?;
        resolved.geometry = bmux_bench::Geometry { columns, rows };
        config::validate(&resolved)?;
    }
    let interactive = ui::enabled(args.plain, args.json);
    if interactive {
        ui::announce(&format!(
            "BMUX bench · {} · {} samples · {}×{}",
            safe(&resolved.profile),
            resolved.samples,
            resolved.geometry.columns,
            resolved.geometry.rows
        ))?;
    }
    #[cfg(feature = "internal")]
    let mut record = if args.internal {
        // Internal measurements do not borrow the controlling terminal at all.
        bmux_bench::internal::run(resolved, args.label.clone(), &cancelled)?
    } else {
        run_terminal(resolved, args.label.clone(), &cancelled, interactive)?
    };
    #[cfg(not(feature = "internal"))]
    let mut record = run_terminal(resolved, args.label.clone(), &cancelled, interactive)?;
    if scoped {
        signals::refresh(&cancelled);
    }
    if cancelled.load(Ordering::Relaxed) && record.status != Status::Cancelled {
        record.warnings.push(format!(
            "Cancellation requested before result finalization (runner status {:?}).",
            record.status
        ));
        // Measurements already settled. Preserve their status instead of creating
        // a cancelled envelope containing only complete outcomes (invalid artifact).
    }
    finalize_run(args, &record, interactive)
}

fn run_terminal(
    config: RunConfig,
    label: String,
    cancelled: &AtomicBool,
    interactive: bool,
) -> Result<RunRecord, String> {
    let categories = workload::catalog()
        .into_iter()
        .map(|case| (case.id, case.category))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut progress = ui::ProgressDisplay::new(interactive, categories);
    let mut backend = terminal::TerminalBackend::open()?;
    // Runner callbacks are synchronous and occur only outside timed writes/probes.
    // The display uses BMUX's raw-safe inline encoder, never termios ownership.
    let result = runner::run(&mut backend, config, label, cancelled, |phase| {
        if progress.update(&phase).is_err() {
            cancelled.store(true, Ordering::Relaxed);
        }
    });
    let restored = backend.restore();
    drop(backend);
    let result = match (result, restored) {
        (Ok(record), Ok(())) => Ok(record),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(format!("terminal restoration failed: {cleanup}")),
        (Err(error), Err(cleanup)) => {
            Err(format!("{error}; terminal restoration failed: {cleanup}"))
        }
    };
    match (result, progress.finish()) {
        (Ok(record), Ok(())) => Ok(record),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(display)) => Err(format!("{error}; {display}")),
    }
}

/// Default names include a fresh nonce as well as the portable run identity.
/// Identity is sanitized into one path component; imported IDs cannot escape root.
fn artifact_path(args: &RunArgs, run_id: &str) -> Option<PathBuf> {
    if args.no_artifacts {
        return None;
    }
    if let Some(output) = &args.output {
        return Some(output.clone());
    }
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id: String = run_id
        .chars()
        .take(128)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let id = if id.is_empty() { "run" } else { &id };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    Some(Path::new("bmux-bench-results").join(format!(
        "{id}-{nonce:x}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )))
}

fn retain_artifacts(args: &RunArgs, record: &RunRecord) -> Result<Option<PathBuf>, String> {
    let output = artifact_path(args, &record.run_id);
    if let Some(path) = &output {
        // Only the default root is created automatically. Explicit --output retains
        // core's contract that its parent must already exist.
        if args.output.is_none() {
            std::fs::create_dir_all("bmux-bench-results")
                .map_err(|e| format!("create default artifact directory: {e}"))?;
        }
        artifact::write_bundle(path, record)?;
    }
    Ok(output)
}

fn finalize_run(args: &RunArgs, record: &RunRecord, interactive: bool) -> Result<u8, String> {
    // Both runners are finished and real-terminal restoration precedes all I/O here.
    let output = retain_artifacts(args, record)?;
    if args.json {
        emit_json(serde_json::to_string_pretty(record))?;
    } else {
        if interactive {
            ui::announce("BMUX bench · measurement finished")?;
        }
        emit(&human_summary(record, output.as_deref())?)?;
    }
    Ok(match record.status {
        Status::Complete => 0,
        Status::Cancelled => 130,
        _ => 1,
    })
}

fn human_summary(record: &RunRecord, output: Option<&Path>) -> Result<String, String> {
    let summary = analysis::summarize(record)?;
    let mut text = format!(
        "Run {} ({}) — {:?}\n",
        safe(&record.run_id),
        safe(&record.label),
        record.status
    );
    text.push_str("Metrics measure internal processing / accepted writes / protocol response / pacing as labeled, NOT completed visible rendering.\n");
    let mut categories =
        std::collections::BTreeMap::<&str, Vec<&bmux_bench::model::CaseOutcome>>::new();
    for outcome in &record.outcomes {
        categories
            .entry(&outcome.spec.category)
            .or_default()
            .push(outcome);
    }
    for (category, outcomes) in categories {
        text.push_str(&format!("\n{}\n", safe(category)));
        for outcome in outcomes {
            text.push_str(&format!(
                "  {}: {:?}, {} recorded samples\n",
                safe(&outcome.spec.id),
                outcome.status,
                outcome.samples.len()
            ));
            if let Some(reason) = &outcome.reason {
                text.push_str(&format!("    {}\n", safe(reason)));
            }
            for metric in summary
                .metrics
                .iter()
                .filter(|metric| metric.case_id == outcome.spec.id)
            {
                text.push_str(&format!(
                    "    {}: median {:.3} {}, MAD {:.3}, CI [{:.3}, {:.3}], n={}\n",
                    safe(&metric.metric),
                    metric.median,
                    safe(&metric.unit),
                    metric.mad,
                    metric.ci_low,
                    metric.ci_high,
                    metric.count
                ));
                for warning in &metric.warnings {
                    text.push_str(&format!("    warning: {}\n", safe(warning)));
                }
            }
        }
    }
    for warning in &record.warnings {
        text.push_str(&format!("warning: {}\n", safe(warning)));
    }
    if let Some(path) = output {
        text.push_str(&format!(
            "\nArtifacts: {}\n",
            safe(&path.display().to_string())
        ));
    } else {
        text.push_str("\nArtifact retention explicitly disabled (--no-artifacts).\n");
    }
    Ok(text)
}

// Never replay terminal control sequences from labels, imported artifacts or paths.
fn safe(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

fn emit(text: &str) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| format!("write stdout: {e}"))
}

fn emit_json(serialized: serde_json::Result<String>) -> Result<(), String> {
    let mut text = serialized.map_err(|e| format!("serialize result: {e}"))?;
    text.push('\n');
    emit(&text)
}

#[cfg(test)]
mod tests;
