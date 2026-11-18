//! `bmux-bench-work`: turn coverage profiles into work reports and check them against a baseline
//! (see `bmux_bench::work` and `scripts/perf-render.py work`).
//!
//! ```text
//! bmux-bench-work summarize --show SHOW.txt --scenario NAME --frames N [--top 40]
//! bmux-bench-work check --report REPORT.json --baseline BASELINE.json
//! ```
//!
//! `summarize` reads `llvm-profdata show --all-functions --counts` output and prints the
//! scenario's work as JSON; `check` prints regressions and improvements, failing on a regression.

use std::path::PathBuf;
use std::process::ExitCode;

use bmux_bench::work::{WorkReport, parse_profdata_show, summarize};

fn value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1).cloned())
}

fn required(args: &[String], flag: &str) -> Result<String, String> {
    value(args, flag).ok_or_else(|| format!("missing {flag}"))
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    match args.first().map(String::as_str) {
        Some("summarize") => {
            let show = std::fs::read_to_string(required(args, "--show")?)
                .map_err(|error| error.to_string())?;
            let frames = required(args, "--frames")?
                .parse()
                .map_err(|_| "--frames is not a number")?;
            let top = value(args, "--top")
                .map_or(Ok(40), |top| top.parse())
                .map_err(|_| "--top is not a number")?;
            let scenario = summarize(
                &required(args, "--scenario")?,
                frames,
                &parse_profdata_show(&show),
                top,
            );
            println!(
                "{}",
                serde_json::to_string(&scenario).map_err(|error| error.to_string())?
            );
            Ok(ExitCode::SUCCESS)
        }
        Some("check") => {
            let read = |flag| {
                required(args, flag)
                    .map(PathBuf::from)
                    .and_then(|path| WorkReport::read(&path).map_err(|error| error.to_string()))
            };
            let (report, baseline) = (read("--report")?, read("--baseline")?);
            for improvement in report.improvements(&baseline) {
                println!("improved (the baseline can be lowered): {improvement}");
            }
            let regressions = report.regressions(&baseline);
            for regression in &regressions {
                println!("regression: {regression}");
            }
            Ok(if regressions.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        _ => Err("usage: bmux-bench-work summarize|check ...".into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args).unwrap_or_else(|error| {
        eprintln!("bmux-bench-work: {error}");
        ExitCode::FAILURE
    })
}
