//! Checks that the code a deterministic simulation runs does its threads, locks, time,
//! randomness, environment, processes, files and directory listings through what the simulation
//! controls (`switchy_async`, `switchy_thread`, `switchy_time`, `switchy_env`, `switchy_fs` and
//! the like), so a simulator build runs it as a pure function of its seed.
//!
//! A crate on a simulated path calls [`Scan`] from a test of its own. The scan reads every `.rs`
//! file under the crate's `src/` (skipping `#[cfg(test)]` items, comments and `*_tests.rs`
//! files) and reports each line that:
//!
//! * uses a forbidden primitive ([`FORBIDDEN`]) other than through a `switchy_` crate of the same
//!   name;
//! * imports `std::fs` in a group (`use std::{fs, io};`);
//! * declares process-wide mutable state (a `static` holding a lock, an atomic or a lazily
//!   initialised value), which concurrent simulated runs would share;
//! * chooses code by operating system outside the places the crate allows it, so the simulation
//!   (which behaves as Linux, macOS or Windows on any host) runs the paths each native build
//!   runs.
//!
//! Native backends (files that talk to one operating system and never run in simulation) are
//! exempt as a whole, as are the modules that implement the controlled versions; any other line
//! using the standard library's version directly must say why it is safe with a
//! `// simulation-safe: <reason>` comment (on it or on the line above).

use std::path::{Path, PathBuf};

/// Primitives a simulation cannot control.
pub const FORBIDDEN: &[&str] = &[
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
    // executable through a platform layer.
    "std::fs",
    "fs::",
    "tempfile::",
    "use tempfile",
    "current_exe(",
    "/proc/self",
    "bmux_plugin::retain_executable",
    "bmux_plugin::executable_sha256",
    // `Path`'s own file system queries always ask the host (use `switchy_fs`).
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
    // With several receivers, tokio's wakes them in an order no seed decides: use
    // switchy_async's (tokio's natively, deterministic in simulation).
    "tokio::sync::watch",
    "sync::{mpsc, oneshot, watch}",
    "new_v4",
    "rand::",
    // Iteration order differs between processes: use `BTreeMap`/`BTreeSet`, or mark a map only
    // ever looked up.
    "HashMap",
    "HashSet",
    // Addresses differ between runs: never order by them.
    "as_ptr(",
];

/// A scan of one crate's sources, with that crate's exemptions.
#[derive(Debug, Clone)]
pub struct Scan {
    src: PathBuf,
    only: Vec<String>,
    native: Vec<String>,
    exempt: Vec<String>,
    os_named_in: Vec<String>,
    forbidden: Vec<String>,
}

impl Scan {
    /// A scan of `manifest_dir/src` (pass `env!("CARGO_MANIFEST_DIR")`), with no exemptions.
    #[must_use]
    pub fn new(manifest_dir: &str) -> Self {
        Self {
            src: Path::new(manifest_dir).join("src"),
            only: Vec::new(),
            native: Vec::new(),
            exempt: Vec::new(),
            os_named_in: Vec::new(),
            forbidden: FORBIDDEN.iter().map(ToString::to_string).collect(),
        }
    }

    /// Scan only these files (by path prefix under `src/`): for a crate of which a simulation
    /// runs only some parts, the parts it runs.
    #[must_use]
    pub fn only(mut self, prefixes: &[&str]) -> Self {
        self.only.extend(prefixes.iter().map(ToString::to_string));
        self
    }

    /// Forbid these patterns too, on top of [`FORBIDDEN`].
    #[must_use]
    pub fn forbid(mut self, patterns: &[&str]) -> Self {
        self.forbidden
            .extend(patterns.iter().map(ToString::to_string));
        self
    }

    /// Native backends, by path prefix under `src/` (`platform/unix/`, `native.rs`): each talks
    /// to one operating system and never runs in simulation, so it is not scanned at all.
    #[must_use]
    pub fn native(mut self, prefixes: &[&str]) -> Self {
        self.native.extend(prefixes.iter().map(ToString::to_string));
        self
    }

    /// Files exempt as a whole, by exact path under `src/`: the controlled versions themselves,
    /// and code that only tests or benchmarks build.
    #[must_use]
    pub fn exempt(mut self, files: &[&str]) -> Self {
        self.exempt.extend(files.iter().map(ToString::to_string));
        self
    }

    /// Where operating systems may be named, by path prefix under `src/` (a platform layer).
    /// Everywhere else, choosing code by operating system is a violation.
    #[must_use]
    pub fn os_named_in(mut self, prefixes: &[&str]) -> Self {
        self.os_named_in
            .extend(prefixes.iter().map(ToString::to_string));
        self
    }

    /// Every violation, as `file:line: what`.
    ///
    /// # Panics
    ///
    /// * If the sources cannot be read
    #[must_use]
    pub fn violations(&self) -> Vec<String> {
        let mut files = Vec::new();
        sources(&self.src, &mut files);
        files.sort();
        let mut violations = Vec::new();
        for path in files {
            let relative = path
                .strip_prefix(&self.src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if (!self.only.is_empty()
                && !self.only.iter().any(|only| relative.starts_with(only.as_str())))
                || self.native.iter().any(|native| relative.starts_with(native.as_str()))
                || self.exempt.contains(&relative)
                || relative.ends_with("_tests.rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let os_allowed = self
                .os_named_in
                .iter()
                .any(|prefix| relative.starts_with(prefix.as_str()));
            check_source(
                &relative,
                &source,
                &self.forbidden,
                os_allowed,
                &mut violations,
            );
        }
        violations
    }

    /// Fail with every violation, if there are any.
    ///
    /// # Panics
    ///
    /// * If the crate uses a primitive the simulation does not control, or the sources cannot
    ///   be read
    pub fn assert_clean(&self) {
        let violations = self.violations();
        assert!(
            violations.is_empty(),
            "use what the simulation controls (or mark the line `// simulation-safe: <reason>`), \
             and name operating systems only in {:?}:\n{}",
            self.os_named_in,
            violations.join("\n")
        );
    }
}

/// Check one file's `source` (at `relative`), pushing what it violates onto `violations`.
fn check_source(
    relative: &str,
    source: &str,
    forbidden: &[String],
    os_allowed: bool,
    violations: &mut Vec<String>,
) {
    let raw: Vec<&str> = source.lines().collect();
    for (number, line) in production_lines(source) {
        if !os_allowed && names_an_operating_system(line) {
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
        for pattern in forbidden {
            if uses(line, pattern) {
                violations.push(format!("{relative}:{number}: {}", line.trim()));
            }
        }
    }
}

/// Whether `line` declares process-wide mutable state, which concurrent simulated runs would
/// share (keep it per run in simulation).
fn process_wide(line: &str) -> bool {
    let item = line.trim_start();
    let item = item
        .strip_prefix("pub(crate) ")
        .or_else(|| item.strip_prefix("pub(super) "))
        .or_else(|| item.strip_prefix("pub "))
        .unwrap_or(item);
    item.starts_with("static ")
        && ["OnceLock", "LazyLock", "OnceCell", "Lazy<", "Atomic", "Mutex", "RwLock"]
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

/// The lines of `source` outside `#[cfg(test)]` items, `// native-only: begin` …
/// `// native-only: end` regions and comments, numbered from one.
fn production_lines(source: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut skipping = false;
    let mut native_only = false;
    let mut depth: i64 = 0;
    let mut entered = false;
    for (number, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if !skipping && trimmed.starts_with("// native-only: begin") {
            native_only = true;
        } else if native_only && trimmed.starts_with("// native-only: end") {
            native_only = false;
            continue;
        }
        if native_only {
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn check(source: &str, os_allowed: bool) -> Vec<String> {
        let forbidden: Vec<String> = FORBIDDEN.iter().map(ToString::to_string).collect();
        let mut violations = Vec::new();
        check_source("x.rs", source, &forbidden, os_allowed, &mut violations);
        violations
    }

    #[test]
    fn forbidden_primitives_are_reported() {
        assert_eq!(check("let m = std::sync::Mutex::new(0);", false).len(), 1);
        assert_eq!(check("let m: HashMap<u8, u8> = HashMap::new();", false).len(), 1);
    }

    #[test]
    fn switchy_versions_are_allowed() {
        assert!(check("use switchy_thread::sync::Mutex;", false).is_empty());
        assert!(check("let n = switchy_time::now();", false).is_empty());
        assert!(check("switchy_fs::sync::read(path)", false).is_empty());
    }

    #[test]
    fn a_reason_on_the_line_or_above_opts_out() {
        assert!(check("let m = std::sync::Mutex::new(0); // simulation-safe: x", false).is_empty());
        assert!(check("// simulation-safe: x\nlet m = std::sync::Mutex::new(0);", false).is_empty());
    }

    #[test]
    fn test_items_and_comments_are_skipped() {
        let source = "// std::sync::Mutex\n#[cfg(test)]\nmod tests {\n    use std::fs;\n}\nfn f() {}\n";
        assert!(check(source, false).is_empty());
        assert!(check("#[cfg(test)]\nmod tests;\nuse std::fs;", false).len() == 1);
    }

    #[test]
    fn native_only_regions_are_skipped() {
        let source = "// native-only: begin\nuse std::sync::Mutex;\n#[cfg(windows)]\n// native-only: end\nuse std::sync::Mutex;";
        assert_eq!(check(source, false), ["x.rs:5: use std::sync::Mutex;"]);
    }

    #[test]
    fn grouped_std_fs_imports_are_reported() {
        assert_eq!(check("use std::{fs, io};", false).len(), 1);
        assert!(check("use std::{io, path};", false).is_empty());
    }

    #[test]
    fn statics_holding_state_are_reported_whatever_their_visibility() {
        assert_eq!(check("static X: OnceLock<u8> = OnceLock::new();", false).len(), 1);
        assert_eq!(check("pub static X: AtomicU64 = AtomicU64::new(0);", false).len(), 1);
        assert_eq!(check("pub(crate) static X: Mutex<u8> = Mutex::new(0);", false).len(), 1);
        assert!(check("static NAMES: &[&str] = &[];", false).is_empty());
    }

    #[test]
    fn operating_systems_are_named_only_where_allowed() {
        assert_eq!(check("#[cfg(unix)]", false).len(), 1);
        assert!(check("#[cfg(unix)]", true).is_empty());
        // A reason does not excuse naming an operating system.
        assert_eq!(check("#[cfg(windows)] // simulation-safe: x", false).len(), 1);
    }
}
