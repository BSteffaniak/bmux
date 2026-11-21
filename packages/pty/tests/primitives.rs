//! Everything this crate does with threads, channels, locks and time goes through
//! `switchy_thread` and `switchy_time`, so a simulator build schedules all of it
//! deterministically. Only code marked native-only (Windows and other hosts that are never
//! simulated) may use the standard library's versions directly.

use std::path::Path;

const FORBIDDEN: &[&str] = &[
    "std::thread::",
    "std::sync::mpsc",
    "std::sync::Mutex",
    "std::sync::atomic",
    "std::sync::{",
    "std::time::Instant",
    "Instant::now",
    "SystemTime::now",
    "std::process::Command",
    "HashMap",
    "HashSet",
];

/// The lines of `source` outside `// native-only: begin` … `// native-only: end` regions.
fn checked_lines(source: &str) -> Vec<(usize, &str)> {
    let mut native_only = false;
    let mut lines = Vec::new();
    for (number, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("// native-only: begin") {
            native_only = true;
        } else if trimmed.starts_with("// native-only: end") {
            native_only = false;
        } else if !native_only && !trimmed.starts_with("//") {
            lines.push((number + 1, line));
        }
    }
    lines
}

#[test]
fn threads_channels_and_time_go_through_switchy() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        // The Windows pseudo-console runner is native-only as a whole.
        if path.extension().is_none_or(|e| e != "rs") || path.ends_with("conpty.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        for (number, line) in checked_lines(&source) {
            for pattern in FORBIDDEN {
                // The doc example of spawn_named names std::thread::spawn in prose.
                if line.contains(pattern) && !line.contains("[`std::thread::spawn`]") {
                    violations.push(format!("{}:{number}: {}", path.display(), line.trim()));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "use switchy_thread / switchy_time instead of:\n{}",
        violations.join("\n")
    );
}
