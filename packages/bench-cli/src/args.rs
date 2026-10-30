use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "bmux-bench",
    version,
    about = "Portable terminal benchmarks; write acceptance is not visible rendering"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// List deterministic benchmark cases without opening a terminal.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Describe one case and its measurement limitations.
    Explain {
        case: String,
        #[arg(long)]
        json: bool,
    },
    /// Measure the controlling terminal (/dev/tty); never uses stdout as a sink.
    Run(RunArgs),
    /// Compare two versioned artifact bundles.
    Compare {
        base: PathBuf,
        candidate: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Render a stored run without opening a terminal.
    Report {
        run: PathBuf,
        #[arg(long, value_enum, default_value = "markdown")]
        format: ReportFormat,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ReportFormat {
    Markdown,
    Json,
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Built-in or configured profile (default: quick).
    #[arg(long, default_value = "quick")]
    pub profile: String,
    /// Suite or catalog category; repeat to select several.
    #[arg(long)]
    pub category: Vec<String>,
    /// Exact case identifier; repeat to select several.
    #[arg(long = "case")]
    pub cases: Vec<String>,
    #[arg(long)]
    pub samples: Option<usize>,
    /// Strict TOML profile configuration.
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long, default_value = "")]
    pub label: String,
    /// Destination artifact bundle directory (default: bmux-bench-results/<run-id>).
    #[arg(long, conflicts_with = "no_artifacts")]
    pub output: Option<PathBuf>,
    /// Explicitly disable artifact retention.
    #[arg(long, conflicts_with = "output")]
    pub no_artifacts: bool,
    /// Benchmark in-process BMUX primitives without opening a terminal.
    #[cfg(feature = "internal")]
    #[arg(long)]
    pub internal: bool,
    /// Disable inline decoration (real-terminal measurements still require a tty).
    #[arg(long)]
    pub plain: bool,
    /// Emit exactly one RunRecord JSON value on stdout; benchmark writes go to /dev/tty.
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub seed: Option<u64>,
    #[arg(long)]
    pub columns: Option<u16>,
    #[arg(long)]
    pub rows: Option<u16>,
    /// Request cursor-position response timing, not a rendering fence.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub probe: Option<bool>,
    /// Explicitly enable/disable synchronized-update workloads; not auto-detected.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub synchronized_updates: Option<bool>,
}
