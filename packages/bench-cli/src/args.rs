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
    /// Play a frame corpus into this terminal at a fixed rate; inside a BMUX pane whose attach
    /// client runs with BMUX_REPLAY_LOG, report end-to-end latency per frame.
    Replay(ReplayArgs),
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

#[derive(Debug, Args)]
pub(crate) struct ReplayArgs {
    /// Corpus file (records of an 8-byte little-endian length, then a frame).
    pub corpus: Option<std::path::PathBuf>,
    /// A generated corpus at this terminal's size, when no file is given.
    #[arg(long, conflicts_with = "corpus")]
    pub generated: Option<String>,
    /// Frames to generate.
    #[arg(long, default_value_t = 300)]
    pub frames: usize,
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    /// The attach client's BMUX_REPLAY_LOG file, read after playing.
    #[arg(long)]
    pub log: Option<std::path::PathBuf>,
}
