//! Subprocess acceptance for read-only configuration inspection.
//!
//! Exercise the actual Cargo-built binary in a private environment. These tests
//! intentionally avoid assuming a diagnostic envelope or exposing raw BMUX data.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// Short canonical roots avoid macOS AF_UNIX limits. Every subprocess receives
/// only private HOME/XDG roots and explicitly selected inputs, never host selectors.
struct Inspection {
    root: PathBuf,
}

impl Inspection {
    fn new() -> Self {
        #[cfg(unix)]
        let temporary = Path::new("/tmp").canonicalize().unwrap();
        #[cfg(not(unix))]
        let temporary = std::env::temp_dir().canonicalize().unwrap();
        let root = loop {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let candidate = temporary.join(format!("btc-{}-{id}", std::process::id()));
            match std::fs::create_dir(&candidate) {
                Ok(()) => break candidate.canonicalize().unwrap(),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create private inspection root: {error}"),
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for name in ["h", "c", "d", "s", "r", "t", "k", "w"] {
            let path = root.join(name);
            std::fs::create_dir(&path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        Self { root }
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bterm"));
        command.env_clear().current_dir(self.root.join("w"));
        for (name, directory) in [
            ("HOME", "h"),
            ("XDG_CONFIG_HOME", "c"),
            ("XDG_DATA_HOME", "d"),
            ("XDG_STATE_HOME", "s"),
            ("XDG_RUNTIME_DIR", "r"),
            ("XDG_CACHE_HOME", "k"),
            ("TMPDIR", "t"),
        ] {
            command.env(name, self.root.join(directory));
        }
        // Inspection must not need any executable discovery, including a companion.
        command.env("PATH", self.root.join("w"));
        command
    }

    fn inspect(&self, arguments: &[&OsStr], environment: &[(&str, &OsStr)]) -> Output {
        let before = snapshot(&self.root);
        let mut command = self.command();
        command.args(arguments);
        for (name, value) in environment {
            command.env(name, value);
        }
        let output = command.output().expect("run Cargo-built bterm inspection");
        assert_eq!(
            snapshot(&self.root),
            before,
            "inspection changed private files"
        );
        output
    }
}

impl Drop for Inspection {
    fn drop(&mut self) {
        // Only the private directory created by this fixture is owned for cleanup.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Entry>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let key = path.strip_prefix(root).unwrap().to_path_buf();
            if metadata.file_type().is_symlink() {
                entries.insert(key, Entry::Symlink(std::fs::read_link(&path).unwrap()));
            } else if metadata.is_dir() {
                entries.insert(key, Entry::Directory);
                visit(root, &path, entries);
            } else {
                assert!(
                    metadata.is_file(),
                    "inspection created a special file: {path:?}"
                );
                entries.insert(key, Entry::File(std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn explanation(output: &Output) -> serde_json::Value {
    assert!(output.status.success(), "inspection failed: {output:?}");
    serde_json::from_slice(&output.stdout).expect("inspection emits JSON")
}

#[test]
fn superset_false_zero_and_secrets_are_inspected_without_writes() {
    let inspection = Inspection::new();
    let config = inspection.write(
        "only.toml",
        "[general]\nscrollback_limit = 0\n[bterm.window]\ndecorations = false\n[private_extension]\nsecret = 'acceptance-secret-never-emit'\n",
    );
    let managed = inspection.root.join("m");
    let output = inspection.inspect(
        &[
            OsStr::new("--managed-root"),
            managed.as_os_str(),
            OsStr::new("--config"),
            config.as_os_str(),
            OsStr::new("config"),
            OsStr::new("explain"),
        ],
        &[],
    );
    let value = explanation(&output);
    assert_eq!(value["bterm"]["window"]["decorations"], false);
    assert_eq!(value["execution"]["mode"], "managed");
    assert!(
        value["bmux"].is_string(),
        "BMUX config must remain redacted"
    );
    for bytes in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains("acceptance-secret-never-emit"));
    }
    assert!(!managed.exists(), "explain created the managed root");
}

#[test]
fn external_inspection_has_no_companion_dependency() {
    let inspection = Inspection::new();
    let external = inspection.root.join("external");
    let output = inspection.inspect(
        &[
            OsStr::new("--runtime-root"),
            external.as_os_str(),
            OsStr::new("config"),
            OsStr::new("explain"),
        ],
        &[],
    );
    let value = explanation(&output);
    assert_eq!(value["execution"]["mode"], "external");
    assert_eq!(
        value["execution"]["runtime_dir"],
        external.join("runtime").to_str().unwrap()
    );
    assert!(!external.exists());
}

#[test]
fn missing_selected_overlay_is_an_error_not_host_fallback() {
    let inspection = Inspection::new();
    let missing = inspection.root.join("missing.toml");
    for flag in ["--config", "--bmux-config"] {
        let output = inspection.inspect(
            &[
                OsStr::new(flag),
                missing.as_os_str(),
                OsStr::new("config"),
                OsStr::new("explain"),
            ],
            &[],
        );
        assert!(!output.status.success(), "missing {flag} was accepted");
        assert!(
            output.stdout.is_empty(),
            "failed inspection emitted a success document"
        );
    }
}

#[test]
fn profiles_and_partial_bmux_overlay_preserve_native_configuration() {
    let inspection = Inspection::new();
    let bterm = inspection.write(
        "bterm.toml",
        "[general]\nscrollback_limit = 0\n[bterm.window]\ndecorations = true\n[composition]\nactive_profile = 'native'\nlayer_order = ['config', 'profile:active']\n[composition.profiles.native.patch.bterm.window]\ndecorations = false\n",
    );
    let bmux = inspection.write("bmux.toml", "[general]\nscrollback_limit = 123\n");
    let output = inspection.inspect(
        &[
            OsStr::new("--config"),
            bterm.as_os_str(),
            OsStr::new("--bmux-config"),
            bmux.as_os_str(),
            OsStr::new("config"),
            OsStr::new("explain"),
        ],
        &[],
    );
    let value = explanation(&output);
    assert_eq!(value["bterm"]["window"]["decorations"], false);
    let sources = value["sources"].as_array().expect("source provenance");
    for (family, path) in [("bterm", bterm), ("bmux", bmux)] {
        assert!(sources.iter().any(|source| {
            source["family"] == family
                && source["path"] == path.to_str().unwrap()
                && source["present"] == true
        }));
    }
    // BMUX effective values are intentionally redacted. Exact numeric merge
    // precedence belongs in resolver tests, not inferred from a fingerprint.
}

#[test]
fn bterm_environment_overlay_and_cli_overlay_have_explicit_priority() {
    let inspection = Inspection::new();
    let primary = inspection.write("c/bterm/bterm.toml", "[bterm.window]\ndecorations = true\n");
    let overlay = inspection.write("environment.toml", "[bterm.window]\ndecorations = false\n");
    let selected = inspection.write("selected.toml", "[bterm.window]\ndecorations = true\n");
    let output = inspection.inspect(
        &[OsStr::new("config"), OsStr::new("explain")],
        &[("BTERM_CONFIG", overlay.as_os_str())],
    );
    assert_eq!(
        explanation(&output)["bterm"]["window"]["decorations"],
        false
    );
    let output = inspection.inspect(
        &[
            OsStr::new("--config"),
            selected.as_os_str(),
            OsStr::new("config"),
            OsStr::new("explain"),
        ],
        &[("BTERM_CONFIG", overlay.as_os_str())],
    );
    let value = explanation(&output);
    assert_eq!(value["bterm"]["window"]["decorations"], true);
    assert!(value["sources"].as_array().unwrap().iter().any(|source| {
        source["path"] == primary.to_str().unwrap() && source["present"] == true
    }));
}

#[test]
fn clean_sandbox_handoff_uses_only_captured_roots() {
    use sha2::{Digest, Sha256};

    let inspection = Inspection::new();
    let root = inspection.root.join("sandbox");
    for directory in [
        "config/bmux",
        "runtime",
        "data/bmux",
        "state",
        "logs",
        "tmp",
        "home",
    ] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    let artifact = inspection.write("sandbox/bmux-artifact", "fixture artifact, never executed");
    let digest = Sha256::digest(std::fs::read(&artifact).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let manifest = serde_json::json!({
        "id": "sandbox", "source": "sandbox-cli", "created_at_unix_ms": 1,
        "updated_at_unix_ms": 1, "pid": 1, "bmux_bin": artifact,
        "command": [], "env_mode": "clean", "status": "running",
        "exit_code": null, "kept": true,
        "paths": { "root": root, "logs": root.join("logs"),
            "runtime": root.join("runtime"), "state": root.join("state") }
    });
    inspection.write("sandbox/sandbox.json", &manifest.to_string());
    let descriptor = inspection.write(
        "sandbox/application-execution.json",
        &serde_json::json!({
            "version": 1, "root": root, "sandbox_id": "sandbox", "env_mode": "clean",
            "bmux_artifact": { "path": artifact, "sha256": digest }, "configuration": "Clean"
        })
        .to_string(),
    );
    let output = inspection.inspect(
        &[OsStr::new("config"), OsStr::new("explain")],
        &[("BTERM_EXECUTION_CONTEXT", descriptor.as_os_str())],
    );
    let value = explanation(&output);
    assert_eq!(value["execution"]["mode"], "sandbox-attach");
    assert_eq!(value["execution"]["sandbox_id"], "sandbox");
    for (field, relative) in [
        ("config_dir", "config/bmux"),
        ("runtime_dir", "runtime"),
        ("data_dir", "data/bmux"),
        ("state_dir", "state"),
    ] {
        assert_eq!(
            value["execution"][field],
            root.join(relative).to_str().unwrap()
        );
    }
}

#[test]
fn malformed_overlay_errors_do_not_echo_secret_source_values() {
    let inspection = Inspection::new();
    let config = inspection.write(
        "malformed.toml",
        "secret = 'acceptance-private-value' trailing-invalid\n",
    );
    let output = inspection.inspect(
        &[
            OsStr::new("--config"),
            config.as_os_str(),
            OsStr::new("config"),
            OsStr::new("explain"),
        ],
        &[],
    );
    assert!(!output.status.success());
    for bytes in [&output.stdout, &output.stderr] {
        assert!(
            !String::from_utf8_lossy(bytes).contains("acceptance-private-value"),
            "malformed-input diagnostics exposed a private source value"
        );
    }
}

#[test]
fn typed_configuration_errors_do_not_echo_secret_values() {
    let inspection = Inspection::new();
    for (name, text) in [
        (
            "invalid-bmux.toml",
            "[general]\nscrollback_limit = 'acceptance-private-value'\n",
        ),
        (
            "invalid-native.toml",
            "[bterm.window]\ndecorations = 'acceptance-private-value'\n",
        ),
    ] {
        let config = inspection.write(name, text);
        let output = inspection.inspect(
            &[
                OsStr::new("--config"),
                config.as_os_str(),
                OsStr::new("config"),
                OsStr::new("explain"),
            ],
            &[],
        );
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("acceptance-private-value"));
    }
}

#[test]
fn explicit_slot_inspection_uses_manifest_roots_without_launching() {
    let inspection = Inspection::new();
    let binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_bterm")).unwrap();
    let path = |name: &str| inspection.root.join(name).display().to_string();
    let manifest = inspection.write("slots.toml", &format!(
        "[slots.acceptance]\nbinary = {:?}\ninherit_base = false\nconfig_dir = {:?}\nruntime_dir = {:?}\ndata_dir = {:?}\nstate_dir = {:?}\nlog_dir = {:?}\n",
        binary.display().to_string(), path("c"), path("r"), path("d"), path("s"), path("t"),
    ));
    inspection.write("c/bmux.toml", "[general]\nscrollback_limit = 4321\n");
    let output = inspection.inspect(
        &[
            OsStr::new("--slot"),
            OsStr::new("acceptance"),
            OsStr::new("--slots-manifest"),
            manifest.as_os_str(),
            OsStr::new("config"),
            OsStr::new("explain"),
        ],
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["execution"]["mode"], "slot-attach");
    assert_eq!(result["execution"]["runtime_dir"], path("r"));
    assert_eq!(result["bterm"]["window"]["decorations"], true);
    let missing = inspection.inspect(
        &[
            OsStr::new("--slot"),
            OsStr::new("undeclared"),
            OsStr::new("--slots-manifest"),
            manifest.as_os_str(),
            OsStr::new("config"),
            OsStr::new("explain"),
        ],
        &[],
    );
    assert!(!missing.status.success());
}

#[test]
fn partial_execution_context_fails_closed() {
    let inspection = Inspection::new();
    let missing = inspection.root.join("missing-context.json");
    let output = inspection.inspect(
        &[OsStr::new("config"), OsStr::new("explain")],
        &[("BTERM_EXECUTION_CONTEXT", missing.as_os_str())],
    );
    assert!(
        !output.status.success(),
        "partial context fell back to host configuration"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn help_uses_cargo_binary_without_touching_private_environment() {
    let inspection = Inspection::new();
    inspection.write("h/sentinel", "private home must remain unchanged\n");
    let output = inspection.inspect(&[OsStr::new("--help")], &[]);
    assert!(output.status.success(), "help failed: {output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("bterm"), "unexpected help: {help}");
}
