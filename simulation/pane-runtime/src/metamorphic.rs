//! `--metamorphic`: the same runs as Linux, macOS and Windows, compared.
//!
//! For each seed, the simulation runs three times, each in a process of its own: the same seed
//! and epoch offset (so the same profile and the same workload draws), as each personality in
//! turn, with the profile calmed ([`Profile::calm`](crate::profile::Profile::calm): what
//! makes an outcome hang on timing taken out, since timing differs between systems). The
//! systems differ, but at the level of panes they should end alike: as many panes opened,
//! resized and closed, and every run passing. A seed whose three runs differ is reported
//! with each run's summary and replay command.
//!
//! `SIMULATOR_RUNS` seeds (default 20) from `SIMULATOR_SEED` on; the other `SIMULATOR_*` and
//! `PANE_RUNTIME_SIM_*` settings pass through to every run.

use std::process::ExitCode;

/// The variable that asks a run to print its summary, and the line's prefix.
pub const SUMMARY: &str = "PANE_RUNTIME_SIM_SUMMARY";

/// The variable that calms a run's profile.
pub const CALM: &str = "PANE_RUNTIME_SIM_CALM";

/// The personalities each seed runs as.
const PERSONALITIES: [&str; 3] = ["linux", "macos", "windows"];

/// The epoch offset every run of a seed shares.
const EPOCH_OFFSET: u64 = 1_000;

/// Run the metamorphic comparison.
///
/// # Errors
///
/// * If this program cannot be started again
pub fn main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    // simulation-safe: the comparison runs outside every run, and starts them.
    let program = std::env::current_exe()?;
    let seeds: u64 = crate::host_var("SIMULATOR_RUNS")
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(20);
    let first = crate::host_var("SIMULATOR_SEED")
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(simvar::switchy::random::simulator::initial_seed);
    let mut differed = 0;
    for index in 0..seeds {
        let seed = first.wrapping_add(index);
        let mut summaries = Vec::new();
        for personality in PERSONALITIES {
            // simulation-safe: a run of its own, started from outside every run.
            let output = std::process::Command::new(&program)
                .env("SIMULATOR_SEED", seed.to_string())
                .env("SIMULATOR_EPOCH_OFFSET", EPOCH_OFFSET.to_string())
                .env("SIMULATOR_RUNS", "1")
                .env("SIMULATOR_PTY_PERSONALITY", personality)
                .env(SUMMARY, "1")
                .env(CALM, "1")
                .env("NO_TUI", "1")
                .output()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let summary = stdout
                .lines()
                .find_map(|line| line.strip_prefix(SUMMARY))
                .map_or_else(
                    || "no summary (the run died)".to_owned(),
                    |line| line.trim().to_owned(),
                );
            summaries.push((personality, summary));
        }
        if summaries
            .iter()
            .all(|(_, summary)| *summary == summaries[0].1)
        {
            continue;
        }
        differed += 1;
        println!("seed {seed}: the personalities ended differently");
        for (personality, summary) in &summaries {
            println!("  {personality:<8} {summary}");
            println!(
                "    replay: SIMULATOR_SEED={seed} SIMULATOR_EPOCH_OFFSET={EPOCH_OFFSET} \
                 SIMULATOR_PTY_PERSONALITY={personality} {CALM}=1 SIMULATOR_RUNS=1 {}",
                program.display()
            );
        }
    }
    println!("{seeds} seeds, {differed} ended differently across personalities");
    Ok(if differed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
