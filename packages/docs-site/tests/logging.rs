//! Run each scenario in a fresh process: logging globals cannot be reset.

use std::process::{Command, Stdio};

#[path = "../src/logging.rs"]
mod logging;

#[test]
fn logging_child() {
    let Ok(scenario) = std::env::var("BMUX_DOCS_LOGGING_TEST") else {
        return;
    };

    logging::init().expect("initialize stderr logging");
    if scenario == "duplicate" {
        assert!(logging::init().is_err(), "duplicate init must fail");
        log::error!(target: "docs_site_test", "DOCS_DUPLICATE_OK");
    }

    log::error!(target: "docs_site_test", "DOCS_LOG_ERROR");
    log::warn!(target: "docs_site_test", "DOCS_LOG_WARN");
    log::info!(target: "docs_site_test", "DOCS_LOG_INFO");
    log::debug!(target: "docs_site_test", "DOCS_LOG_DEBUG");
    log::trace!(target: "docs_site_test", "DOCS_LOG_TRACE");
    log::error!(target: "unrelated", "DOCS_OTHER_ERROR");
    log::info!(target: "unrelated", "DOCS_OTHER_INFO");
    tracing::error!(target: "docs_site_test", "DOCS_TRACING_ERROR");
    tracing::info!(target: "docs_site_test", "DOCS_TRACING_INFO");
}

fn run_child(scenario: &str, filter: Option<&str>) -> String {
    let directory = tempfile::tempdir().expect("isolated working directory");
    let stderr_path = directory.path().join("stderr");
    let stderr = std::fs::File::create(&stderr_path).expect("redirect stderr to a file");
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", "logging_child", "--nocapture"])
        .env_clear()
        .env("BMUX_DOCS_LOGGING_TEST", scenario)
        // These must not override RUST_LOG or cause runtime directories.
        .env("MOOSICBOX_LOG", "trace")
        .env("HOME", directory.path().join("home"))
        .env("XDG_STATE_HOME", directory.path().join("state"))
        .env("XDG_DATA_HOME", directory.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("RUST_LOG_STYLE", "always")
        .current_dir(directory.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    if let Some(filter) = filter {
        command.env("RUST_LOG", filter);
    }
    let output = command.output().expect("run logging subprocess");
    let stderr = std::fs::read_to_string(stderr_path).expect("read redirected stderr");
    assert!(output.status.success(), "child failed: {stderr}");
    // libtest prints its own progress on stdout, but logging must not do so.
    assert!(!String::from_utf8_lossy(&output.stdout).contains("DOCS_"));
    assert!(
        !stderr.contains('\u{1b}'),
        "redirected stderr must not use ANSI"
    );
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("inspect isolated directory")
            .count(),
        1,
        "logging must create no files or runtime directories"
    );
    stderr
}

#[test]
fn defaults_to_errors_and_bridges_log_records() {
    let stderr = run_child("default", None);
    assert_eq!(stderr.matches("DOCS_LOG_ERROR").count(), 1);
    assert_eq!(stderr.matches("DOCS_TRACING_ERROR").count(), 1);
    assert!(stderr.contains("DOCS_OTHER_ERROR"));
    assert!(stderr.contains("docs_site_test"));
    for suppressed in [
        "DOCS_LOG_WARN",
        "DOCS_LOG_INFO",
        "DOCS_LOG_DEBUG",
        "DOCS_LOG_TRACE",
        "DOCS_OTHER_INFO",
        "DOCS_TRACING_INFO",
    ] {
        assert!(!stderr.contains(suppressed), "unexpected {suppressed}");
    }
}

#[test]
fn empty_filter_defaults_to_errors() {
    let stderr = run_child("empty", Some(""));
    assert!(stderr.contains("DOCS_LOG_ERROR"));
    assert!(!stderr.contains("DOCS_LOG_WARN"));
    assert!(!stderr.contains("DOCS_TRACING_INFO"));
}

#[test]
fn invalid_filter_defaults_to_errors_with_diagnostic() {
    let stderr = run_child("invalid", Some("docs_site_test=bogus"));
    assert!(stderr.contains("ignoring"));
    assert!(stderr.contains("docs_site_test=bogus"));
    assert!(stderr.contains("DOCS_LOG_ERROR"));
    assert!(stderr.contains("DOCS_TRACING_ERROR"));
    assert!(!stderr.contains("DOCS_LOG_INFO"));
    assert!(!stderr.contains("DOCS_TRACING_INFO"));
}

#[test]
fn invalid_directive_keeps_valid_filter_with_diagnostic() {
    let stderr = run_child("partly-invalid", Some("docs_site_test=bogus,info"));
    assert!(stderr.contains("ignoring"));
    assert!(stderr.contains("docs_site_test=bogus"));
    assert!(stderr.contains("DOCS_LOG_INFO"));
    assert!(stderr.contains("DOCS_OTHER_INFO"));
    assert!(stderr.contains("DOCS_TRACING_INFO"));
    assert!(!stderr.contains("DOCS_LOG_DEBUG"));
}

#[test]
fn global_level_filters_redirected_stderr() {
    let stderr = run_child("debug", Some("debug"));
    assert!(stderr.contains("DOCS_LOG_DEBUG"));
    assert!(stderr.contains("DOCS_OTHER_INFO"));
    assert!(stderr.contains("DOCS_TRACING_INFO"));
    assert!(!stderr.contains("DOCS_LOG_TRACE"));
}

#[test]
fn target_directives_filter_log_and_tracing() {
    let stderr = run_child("target", Some("off,docs_site_test=info"));
    assert!(stderr.contains("DOCS_LOG_ERROR"));
    assert!(stderr.contains("DOCS_LOG_WARN"));
    assert!(stderr.contains("DOCS_LOG_INFO"));
    assert!(stderr.contains("DOCS_TRACING_INFO"));
    assert!(!stderr.contains("DOCS_LOG_DEBUG"));
    assert!(!stderr.contains("DOCS_LOG_TRACE"));
    assert!(!stderr.contains("DOCS_OTHER_ERROR"));
    assert!(!stderr.contains("DOCS_OTHER_INFO"));
}

#[test]
fn off_disables_both_sources() {
    let stderr = run_child("off", Some("off"));
    assert!(stderr.is_empty());
}

#[test]
fn duplicate_init_fails_without_replacing_the_logger() {
    let stderr = run_child("duplicate", None);
    assert_eq!(stderr.matches("DOCS_DUPLICATE_OK").count(), 1);
    assert_eq!(stderr.matches("DOCS_LOG_ERROR").count(), 1);
    assert_eq!(stderr.matches("DOCS_TRACING_ERROR").count(), 1);
    assert!(!stderr.contains("DOCS_LOG_INFO"));
}
