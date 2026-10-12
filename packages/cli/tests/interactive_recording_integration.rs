//! Real interactive CLI acceptance. No mock transport, PATH BMUX, or Cargo calls.
//! Coordinator runs this target against development and explicitly staged payloads.
#![cfg(unix)]

use serde_json::Value;
use serial_test::serial;
use sha2::{Digest, Sha256};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn sha256_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut encoded, "{byte:02x}").expect("writing SHA-256 into String");
    }
    encoded
}

struct Fixture {
    root: PathBuf,
    binary: PathBuf,
    short_tmp: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let fixture_base = std::env::var_os("BMUX_RECORDING_TEST_TMP_ROOT")
            .map_or_else(std::env::temp_dir, PathBuf::from);
        assert!(fixture_base.is_absolute(), "fixture base must be absolute");
        let root = fixture_base.join(format!("bmux-ir-{}", uuid::Uuid::new_v4().simple()));
        let socket_base = std::env::var_os("BMUX_RECORDING_TEST_SOCKET_ROOT")
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
            .canonicalize()
            .expect("canonical socket base");
        assert!(socket_base.is_absolute(), "socket base must be absolute");
        let short_tmp = socket_base.join(format!(
            "t-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        // create_dir is exclusive; collisions fail instead of adopting old state.
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&short_tmp)
            .expect("exclusive short owned TMPDIR");
        assert_eq!(
            short_tmp.canonicalize().expect("short TMPDIR canonical"),
            short_tmp
        );
        assert_eq!(
            fs::symlink_metadata(&short_tmp)
                .expect("short TMPDIR metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(
            short_tmp.as_os_str().len() <= 25,
            "TMPDIR must leave Unix endpoint budget"
        );
        for directory in [
            "home",
            "config",
            "data",
            "runtime",
            "state",
            "logs",
            "tmp",
            "recordings",
        ] {
            fs::create_dir_all(root.join(directory)).expect("owned fixture directory");
        }
        fs::write(root.join("config/bmux.toml"), b"").expect("isolated config");
        let binary = match (
            std::env::var_os("BMUX_RECORDING_TEST_BINARY"),
            std::env::var_os("BMUX_RECORDING_TEST_INSTALL_ROOT"),
        ) {
            (None, None) => PathBuf::from(env!("CARGO_BIN_EXE_bmux")),
            (Some(binary), Some(installation)) => {
                let installation = PathBuf::from(installation)
                    .canonicalize()
                    .expect("installation root");
                let binary = PathBuf::from(binary);
                assert!(binary.is_absolute());
                assert!(
                    fs::symlink_metadata(&binary)
                        .expect("installed binary")
                        .is_file()
                );
                let binary = binary.canonicalize().expect("installed binary path");
                assert!(
                    binary.starts_with(&installation),
                    "binary outside owned installation"
                );
                let inventory =
                    fs::read(installation.join("artifacts.json")).expect("installed inventory");
                let parsed: Value = serde_json::from_slice(&inventory).expect("valid inventory");
                assert_eq!(parsed["schema_version"], 1);
                let artifacts = parsed["artifacts"].as_array().expect("committed artifacts");
                assert!(
                    artifacts
                        .iter()
                        .any(|artifact| artifact["relative_path"]
                            == "bmux-pane-execution-supervisor")
                );
                for artifact in artifacts {
                    let relative =
                        Path::new(artifact["relative_path"].as_str().expect("artifact path"));
                    assert!(
                        relative
                            .components()
                            .all(|component| matches!(component, std::path::Component::Normal(_))),
                        "unsafe artifact path"
                    );
                    let path = installation.join(relative);
                    assert!(
                        fs::symlink_metadata(&path)
                            .expect("installed artifact")
                            .is_file()
                    );
                    let bytes = fs::read(path).expect("installed artifact bytes");
                    assert_eq!(
                        artifact["bytes"].as_u64(),
                        Some(u64::try_from(bytes.len()).expect("artifact length"))
                    );
                    assert_eq!(
                        artifact["sha256"].as_str(),
                        Some(sha256_hex(&bytes).as_str())
                    );
                }
                fs::write(root.join("installation-identity.json"), serde_json::to_vec(&serde_json::json!({
                    "binary": binary, "binary_sha256": sha256_hex(&fs::read(&binary).expect("binary bytes")),
                    "inventory_sha256": sha256_hex(&inventory),
                })).expect("identity JSON")).expect("retain installed identity");
                binary
            }
            _ => panic!("installed acceptance requires both explicit binary and install root"),
        };
        assert!(binary.is_absolute());
        fs::write(
            root.join("short-tmpdir.txt"),
            short_tmp.as_os_str().as_encoded_bytes(),
        )
        .expect("retain owned TMPDIR locator");
        Self {
            root,
            binary,
            short_tmp,
        }
    }

    fn isolate(&self, command: &mut Command) {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BMUX_") {
                command.env_remove(key);
            }
        }
        for (key, directory) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "tmp"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("BMUX_CONFIG_DIR", "config"),
            ("BMUX_DATA_DIR", "data"),
            ("BMUX_STATE_DIR", "state"),
            ("BMUX_RUNTIME_DIR", "runtime"),
            ("BMUX_LOG_DIR", "logs"),
            ("BMUX_RECORDINGS_DIR", "recordings"),
        ] {
            command.env(key, self.root.join(directory));
        }
        command
            .env("TMPDIR", &self.short_tmp)
            .env("BMUX_NO_BASE_CONFIG", "1")
            .env("BMUX_RUNTIME_NAME", "default")
            .env("BMUX_TARGET", "local")
            .env("TERM", "xterm-256color")
            .current_dir(&self.root);
    }

    fn interactive(&self, mode: &str) -> (Value, Vec<u8>) {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/interactive-recording");
        let mut command = Command::new("python3");
        command
            .arg(fixtures.join("outer_pty.py"))
            .arg(&self.root)
            .arg(&self.binary)
            .arg(mode)
            .arg(fixtures.join("tui.py"));
        self.isolate(&mut command);
        let output = command
            .stdin(Stdio::null())
            .output()
            .expect("required Python 3 real PTY driver");
        assert!(
            output.status.success(),
            "driver failed; root={}; stderr={}",
            self.root.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        let report = serde_json::from_slice(
            &fs::read(self.root.join("outer-report.json")).expect("outer report"),
        )
        .expect("report JSON");
        let transcript = fs::read(self.root.join("outer-transcript.bin")).expect("outer evidence");
        (report, transcript)
    }

    fn retain_notice(&self) {
        // Never remove uncertain live-runtime state in Drop. Coordinator can clean
        // this exact owned root after inspecting cleanup acknowledgment/result.
        eprintln!(
            "interactive recording evidence retained at {}",
            self.root.display()
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.retain_notice();
    }
}

fn final_json(transcript: &[u8]) -> Value {
    transcript
        .split(|byte| *byte == b'\n')
        .rev()
        .find_map(|line| {
            let start = line.iter().position(|byte| *byte == b'{')?;
            serde_json::from_slice::<Value>(&line[start..]).ok()
        })
        .expect("final structured result after restored terminal")
}

fn assert_complete(fixture: &Fixture, transcript: &[u8], expected_exit: u64) {
    let result = final_json(transcript);
    assert_eq!(result["schema_version"], 4);
    let status = &result["status"];
    assert_eq!(status["exit_code"], expected_exit);
    for field in [
        "target_exited",
        "terminal_session_ended",
        "capture_eof",
        "output_drained",
        "capture_finalized",
        "durable_recording_complete",
        "timing_complete",
    ] {
        assert_eq!(status[field], true, "missing completion: {field}: {result}");
    }
    assert_eq!(status["recovery_required"], false);
    assert!(
        status["errors"]
            .as_array()
            .expect("errors array")
            .is_empty()
    );
    let raw = fs::read(fixture.root.join("exports/output.pty")).expect("canonical raw export");
    assert!(
        raw.starts_with(b"BYTE0\x1b[?1049h"),
        "lost output from byte zero"
    );
    assert!(
        raw.ends_with(b"\x1b[42mFINALMARKER\x1b[0m\x1b[?1049l"),
        "lost unread final tail"
    );
    assert!(
        raw.windows(b"RESIZE:93x31".len())
            .any(|window| window == b"RESIZE:93x31")
    );
    for field in [
        "output_watermark",
        "capture_watermark",
        "recording_watermark",
    ] {
        assert_eq!(
            status[field].as_u64(),
            Some(u64::try_from(raw.len()).expect("raw length"))
        );
    }
    let events =
        fs::read_to_string(fixture.root.join("exports/events.jsonl")).expect("canonical events");
    let events: Vec<Value> = events
        .lines()
        .map(|line| serde_json::from_str(line).expect("event JSON"))
        .collect();
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "pane_output_raw")
    );
    assert!(
        fs::read_to_string(fixture.root.join("exports/output.txt"))
            .expect("text export")
            .contains("FINALMARKER")
    );
    assert!(
        !result["artifacts"]
            .as_array()
            .expect("artifact paths")
            .is_empty()
    );
}

#[test]
#[serial]
fn direct_interactive_record_routes_keyboard_resize_and_finalizes() {
    let fixture = Fixture::new();
    let (report, transcript) = fixture.interactive("normal");
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["restored"], true, "outer termios not restored");
    assert_eq!(report["sent_input"], true);
    assert_complete(&fixture, &transcript, 0);
    let raw = fs::read(fixture.root.join("exports/output.pty")).unwrap();
    assert!(
        raw.windows("KEY:quit héllo\n".len())
            .any(|window| window == "KEY:quit héllo\n".as_bytes())
    );
}

#[test]
#[serial]
fn interactive_nonzero_and_control_c_restore_terminal() {
    for mode in ["nonzero", "ctrl-c"] {
        let fixture = Fixture::new();
        let (report, transcript) = fixture.interactive(mode);
        let exit = if mode == "nonzero" { 7 } else { 0 };
        assert_eq!(report["exit_code"], exit);
        assert_eq!(report["restored"], true);
        assert_complete(&fixture, &transcript, exit);
    }
}

#[test]
#[serial]
fn interactive_launch_error_restores_terminal() {
    let fixture = Fixture::new();
    let (report, _) = fixture.interactive("launch-error");
    assert_ne!(report["exit_code"], 0);
    assert_eq!(report["restored"], true);
    assert!(!fixture.root.join("exports/output.pty").exists());
}

#[test]
#[serial]
fn interactive_record_rejects_non_terminal_without_launching() {
    let fixture = Fixture::new();
    let (report, transcript) = fixture.interactive("no-tty");
    assert_ne!(report["exit_code"], 0);
    assert!(String::from_utf8_lossy(&transcript).contains("terminal"));
    assert!(!fixture.root.join("forbidden-launch").exists());
    assert!(!fixture.root.join("exports").exists());
}

#[test]
#[serial]
fn interactive_gif_preserves_output_and_resize() {
    let fixture = Fixture::new();
    let (report, transcript) = fixture.interactive("gif");
    assert_eq!(report["restored"], true);
    assert_complete(&fixture, &transcript, 0);
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options
        .read_info(fs::File::open(fixture.root.join("animation.gif")).expect("explicit GIF"))
        .expect("decode real recording GIF");
    let mut red = false;
    let mut green = false;
    let mut frames = 0;
    while let Some(frame) = decoder.read_next_frame().expect("GIF frame") {
        frames += 1;
        assert!(frames <= 1000);
        for pixel in frame.buffer.chunks_exact(4).filter(|pixel| pixel[3] != 0) {
            red |= pixel[0] > 80 && u16::from(pixel[0]) > u16::from(pixel[1]) * 2;
            green |= pixel[1] > 80 && u16::from(pixel[1]) > u16::from(pixel[0]) * 2;
        }
    }
    assert!(
        frames > 1 && red && green,
        "lost initial or final colored source frame"
    );
}
