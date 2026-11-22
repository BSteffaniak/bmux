//! Everything the pane runtime does with tasks, threads, locks, time, randomness, the
//! environment, processes, files and directory listings goes through what a simulation controls
//! (`switchy_async`, `switchy_thread`, `switchy_time`, `switchy_env`, `switchy_fs` and the
//! plugin's `ids`, `dir_order` and `run_shared` modules, and the platform layer's `files`), so a
//! simulator build runs it deterministically.
//!
//! Native backends (the platform layer's files that talk to one operating system and never run
//! in simulation) are exempt as a whole, as are the modules that implement the controlled
//! versions; any other line using the standard library's version directly must say why it is
//! safe with a `// simulation-safe: <reason>` comment (on it or on the line above).
//!
//! Only the platform layer (`src/platform/`) names an operating system: core logic is the same on
//! every one, so the simulation (which behaves as Linux, macOS or Windows on any host) runs the
//! paths each native build runs.

use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "std::thread::spawn",
    "std::thread::Builder",
    "std::thread::sleep",
    "std::thread::scope",
    "std::sync::mpsc",
    "std::sync::Mutex",
    "std::sync::RwLock",
    "std::sync::Condvar",
    "std::sync::atomic",
    "std::time::Instant",
    "std::time::SystemTime",
    "SystemTime::now",
    "std::process::Command",
    "std::env::var",
    "env::var(",
    "env::var_os(",
    "env::vars(",
    // Files go through `switchy_fs` (the run's file system in simulation), and this process's
    // executable through the platform layer's `files`.
    "std::fs",
    "fs::",
    "tempfile::",
    "use tempfile",
    "current_exe(",
    "/proc/self",
    "bmux_plugin::retain_executable",
    "bmux_plugin::executable_sha256",
    // `Path`'s own file system queries always ask the host (use `crate::files` or
    // `switchy_fs`).
    ").is_dir()",
    ").is_file()",
    ".exists()",
    ".try_exists(",
    ".canonicalize()",
    ".read_dir()",
    ".read_link()",
    ".symlink_metadata()",
    "tokio::spawn",
    "tokio::time",
    "tokio::task",
    "tokio::runtime",
    "tokio::select",
    "tokio::test",
    "new_v4",
    "rand::",
    "HashMap",
    "HashSet",
    // Addresses differ between runs: never order by them.
    "as_ptr(",
];

/// Whether `line` declares process-wide mutable state, which concurrent simulated runs would
/// share (use `run_shared`, one per run in simulation).
fn process_wide(line: &str) -> bool {
    line.trim_start().starts_with("static ")
        && ["OnceLock", "LazyLock", "Atomic", "Mutex", "RwLock"]
            .iter()
            .any(|kind| line.contains(kind))
}

/// Whether `line` imports `std::fs` in a group (`use std::{fs, io};`), which the forbidden
/// patterns cannot see.
fn imports_std_fs(line: &str) -> bool {
    line.contains("std::{")
        && line
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|word| word == "fs")
}

/// Native backends: each talks to one operating system and never runs in simulation.
const NATIVE: &[&str] = &["platform/unix/", "platform/windows/", "platform/native.rs"];

/// The controlled versions themselves, and code that only tests build.
const EXEMPT: &[&str] = &[
    "ids.rs",
    "dir_order.rs",
    "run_shared.rs",
    "runtime/ingest_bench.rs",
];

/// Where operating systems may be named.
const PLATFORM: &str = "platform/";

/// Whether `line` chooses code by operating system.
fn names_an_operating_system(line: &str) -> bool {
    line.contains("cfg")
        && ["unix", "windows", "target_os", "target_family"]
            .iter()
            .any(|name| line.contains(name))
}

/// Whether `line` uses `pattern` other than through a `switchy_` crate of the same name.
fn uses(line: &str, pattern: &str) -> bool {
    line.match_indices(pattern)
        .any(|(index, _)| !line[..index].ends_with("switchy_"))
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The lines of `source` outside `#[cfg(test)]` items and comments.
fn production_lines(source: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut skipping = false;
    let mut depth: i64 = 0;
    let mut entered = false;
    for (number, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if !skipping
            && (trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("#[cfg(all(test,"))
        {
            skipping = true;
            depth = 0;
            entered = false;
            continue;
        }
        if skipping {
            for c in line.chars() {
                match c {
                    '{' => {
                        depth += 1;
                        entered = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            // An item without a body (`mod tests;`, `use ...;`) ends at its semicolon.
            if (entered && depth <= 0) || (!entered && trimmed.ends_with(';')) {
                skipping = false;
            }
            continue;
        }
        if !trimmed.starts_with("//") {
            lines.push((number + 1, line));
        }
    }
    lines
}

#[test]
fn simulated_primitives_only() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    files.sort();
    let mut violations = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if NATIVE.iter().any(|native| relative.starts_with(native))
            || EXEMPT.contains(&relative.as_str())
            || relative.ends_with("_tests.rs")
        {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let raw: Vec<&str> = source.lines().collect();
        for (number, line) in production_lines(&source) {
            if !relative.starts_with(PLATFORM) && names_an_operating_system(line) {
                violations.push(format!("{relative}:{number}: names an OS: {}", line.trim()));
            }
            // The reason may also stand on the line above.
            if line.contains("// simulation-safe:")
                || (number >= 2 && raw[number - 2].contains("// simulation-safe:"))
            {
                continue;
            }
            if imports_std_fs(line) {
                violations.push(format!("{relative}:{number}: {}", line.trim()));
            }
            if process_wide(line) {
                violations.push(format!(
                    "{relative}:{number}: process-wide: {}",
                    line.trim()
                ));
            }
            for pattern in FORBIDDEN {
                if uses(line, pattern) {
                    violations.push(format!("{relative}:{number}: {}", line.trim()));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "use what the simulation controls (or mark the line `// simulation-safe: <reason>`), \
         and name operating systems only in src/platform/:\n{}",
        violations.join("\n")
    );
}
