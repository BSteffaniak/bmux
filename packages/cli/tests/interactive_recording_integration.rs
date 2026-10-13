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

// Kept inline to preserve exclusive ownership of this acceptance test. Both
// sides use only Python's stdlib, real controlling terminals, and owned state.
const RETAINED_RENDERER_ACCEPTANCE: &str = r#"
import errno
import fcntl
import json
import os
import re
import select
import struct
import subprocess
import sys
import termios
import time

root, binary, owner_fixture = sys.argv[1:]
N = 64
BUDGET = 1.0
native = r'''
import json, os, select, sys, termios, time, tty
assert all(os.isatty(fd) for fd in (0, 1, 2))
assert os.tcgetpgrp(0) == os.getpgrp()
before = termios.tcgetattr(0)
tty.setraw(0)
def emit(data):
    os.write(1, data)
def read_key(expected):
    key = os.read(0, 1)
    assert key == expected, (key, expected)
emit(b'\x1b[?1049h\x1b[2J\x1b[HBOOTGATE')
try:
    read_key(b'!')
    emit(b'\x1b[1;1HREADYBURST')
    for phase, key in [('quiet', b'x'), ('busy', b'y')]:
        if phase == 'busy':
            read_key(b'g')
        emit(b'\x1b[2;1H')
        count = 0
        ticks = 0
        echo = bytearray()
        while count < 64:
            ready, _, _ = select.select([0], [], [], 0.002 if phase == 'busy' else 10)
            if phase == 'busy':
                ticks += 1
                emit(('\x1b[5;1HLIVE%08d' % ticks).encode())
            if ready:
                read_key(key)
                echo += key
                count += 1
                emit(b'\x1b[2;1H' + bytes(echo))
        consumed = time.monotonic()
        with open(sys.argv[1] + '/' + phase + '-consumed.json', 'w') as output:
            json.dump({'consumed': consumed, 'count': count, 'ticks': ticks}, output)
        emit(('\x1b[3;1HACK' + phase.upper()).encode())
        if phase == 'quiet':
            read_key(b'i')
            emit(b'\x1b[4;1HIDLEGATE')
        else:
            # Keep output alive until the outer terminal acknowledges progress.
            while not select.select([0], [], [], 0.002)[0]:
                ticks += 1
                emit(('\x1b[5;1HLIVE%08d' % ticks).encode())
            read_key(b'q')
    emit(b'\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[6;1HPROTOCOLGATE')
    expected = ('hé界'.encode() + b'\x1bOA' + b'\x1b[200~' +
                'paste界\n'.encode() + b'\x1b[201~\x1b[<0;7;4M\x1b[<0;7;4m')
    received = bytearray()
    deadline = time.monotonic() + 2
    while len(received) < len(expected):
        remaining = deadline - time.monotonic()
        assert remaining > 0 and select.select([0], [], [], remaining)[0], (
            'protocol timeout', bytes(received).hex(), expected.hex())
        received += os.read(0, len(expected) - len(received))
    assert received == expected, ('protocol mismatch', bytes(received).hex(), expected.hex())
    emit(b'\x1b[6;1HPROTOCOLACK ' + bytes(received).hex().encode())
    read_key(b'!')
    emit(b'\x1b[?1l\x1b[?2004l\x1b[?1000l\x1b[?1006l\x1b[7;1HFINALTAIL')
    read_key(b'!')
    emit(b'\x1b[?1049l')
finally:
    termios.tcsetattr(0, termios.TCSANOW, before)
'''

# Small incremental observer for the ASCII fixture's visible rows. Matching raw
# substrings would wrongly reject valid retained deltas which echo only suffixes.
class Screen:
    def __init__(self):
        self.rows = [bytearray(b' ' * 80) for _ in range(24)]
        self.x = self.y = 0
        self.saved = (0, 0)
        self.pending = b''
        self.last = 32
    def put(self, byte):
        if self.x >= 80:
            self.x = 0
            self.y = min(23, self.y + 1)
        self.rows[self.y][self.x] = byte
        self.x += 1
        self.last = byte
    def feed(self, data):
        data = self.pending + data
        self.pending = b''
        i = 0
        while i < len(data):
            byte = data[i]
            if byte == 27:
                if i + 1 == len(data):
                    break
                if data[i + 1] == 91:
                    match = re.match(rb'\x1b\[([0-?]*)([ -/]*)([@-~])', data[i:])
                    if not match:
                        break
                    params, _, command = match.groups()
                    p = [int(v or b'0') for v in params.split(b';')] if re.fullmatch(rb'[0-9;]*', params) else []
                    n = (p[0] or 1) if p else 1
                    if command in (b'H', b'f'):
                        self.y = min(23, max(0, (p[0] or 1) - 1)) if p else 0
                        self.x = min(79, max(0, (p[1] or 1) - 1)) if len(p) > 1 else 0
                    elif command == b'G': self.x = min(79, n - 1)
                    elif command == b'd': self.y = min(23, n - 1)
                    elif command == b'A': self.y = max(0, self.y - n)
                    elif command == b'B': self.y = min(23, self.y + n)
                    elif command == b'C': self.x = min(79, self.x + n)
                    elif command == b'D': self.x = max(0, self.x - n)
                    elif command == b'E': self.y, self.x = min(23, self.y + n), 0
                    elif command == b'F': self.y, self.x = max(0, self.y - n), 0
                    elif command == b'b':
                        for _ in range(n): self.put(self.last)
                    elif command == b'K':
                        mode = p[0] if p else 0
                        start, end = (0, 80) if mode == 2 else ((0, self.x + 1) if mode == 1 else (self.x, 80))
                        self.rows[self.y][start:end] = b' ' * (end - start)
                    elif command == b'J' and p and p[0] in (2, 3):
                        self.rows = [bytearray(b' ' * 80) for _ in range(24)]
                    elif command == b's': self.saved = (self.x, self.y)
                    elif command == b'u': self.x, self.y = self.saved
                    i += len(match.group())
                    continue
                if data[i + 1] == 93:
                    match = re.match(rb'\x1b\].*?(?:\x07|\x1b\\)', data[i:], re.S)
                    if not match: break
                    i += len(match.group())
                    continue
                if data[i + 1] == 55: self.saved = (self.x, self.y)
                if data[i + 1] == 56: self.x, self.y = self.saved
                i += 2
                continue
            if byte == 13: self.x = 0
            elif byte == 10: self.y = min(23, self.y + 1)
            elif byte == 8: self.x = max(0, self.x - 1)
            elif 32 <= byte < 127: self.put(byte)
            i += 1
        self.pending = data[i:]
        assert len(self.pending) < 65536, 'unbounded incomplete escape'
    def contains(self, marker):
        return any(marker in row for row in self.rows)
    def tick(self):
        for row in self.rows:
            match = re.search(rb'LIVE(\d{8})', row)
            if match: return int(match.group(1))
        return 0

master, slave = os.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
args = [binary, '--runtime', 'default', '--target', 'local', '--config',
        root + '/config/bmux.toml', 'record', '--output', root + '/exports',
        '--export', 'raw', '--export', 'text', '--export', 'events', '--json',
        '--', sys.executable, '-c', native, root]
child = subprocess.Popen([sys.executable, owner_fixture, '--terminal-owner', root] + args,
                         stdin=slave, stdout=slave, stderr=slave, close_fds=True,
                         preexec_fn=controlling_terminal)
os.set_blocking(master, False)
screen = Screen()
transcript = bytearray()
measurements = {}
def pump(timeout):
    if not select.select([master], [], [], max(0, timeout))[0]: return False
    try:
        data = os.read(master, 65536)
    except OSError as error:
        if error.errno == errno.EIO: return False
        raise
    if not data: return False
    transcript.extend(data)
    assert len(transcript) <= 8 * 1024 * 1024, 'outer evidence exceeded 8 MiB'
    screen.feed(data)
    return True
def wait_for(predicate, label, budget=10, completion=False):
    start = time.monotonic()
    while not predicate():
        elapsed = time.monotonic() - start
        assert elapsed < budget, '%s: elapsed=%.6fs budget=%.6fs measurements=%r rows=%r' % (
            label, elapsed, budget, measurements, screen.rows[:6])
        assert completion or child.poll() is None, '%s: owner exited, measurements=%r' % (label, measurements)
        pump(min(0.05, budget - elapsed))
def send(data):
    assert os.write(master, data) == len(data), 'partial outer input write'
def burst(phase, key, minimum_tick=0):
    start = time.monotonic()
    send(key * N)
    try:
        wait_for(lambda: screen.contains(b'ACK' + phase.upper().encode()) and
                 screen.contains(key * N) and screen.tick() >= minimum_tick,
                 phase + ' consumed and visible echo/output progress', BUDGET)
    except AssertionError as error:
        evidence = {'keys_requested': N, 'elapsed_seconds': time.monotonic() - start,
                    'budget_seconds': BUDGET, 'visible_echo_keys': max(
                        (bytes(row).count(key) for row in screen.rows), default=0),
                    'visible_tick': screen.tick(), 'minimum_tick': minimum_tick}
        try:
            with open(root + '/' + phase + '-consumed.json') as source:
                evidence['native_consumption'] = json.load(source)
        except FileNotFoundError:
            evidence['native_consumption'] = 'not yet complete'
        measurements[phase] = evidence
        raise AssertionError('%s; exact burst evidence=%r' % (error, evidence)) from error
    visible = time.monotonic() - start
    with open(root + '/' + phase + '-consumed.json') as source:
        consumed = json.load(source)
    measurements[phase] = {'keys': consumed['count'], 'consumed_seconds': consumed['consumed'] - start,
                           'visible_seconds': visible, 'budget_seconds': BUDGET,
                           'output_ticks_at_consumption': consumed['ticks']}
    assert consumed['count'] == N and 0 <= consumed['consumed'] - start < BUDGET and visible < BUDGET, measurements
try:
    # This includes isolated server bootstrap and executable identity hashing
    # (the debug executable can exceed 1 GiB), not renderer latency. Start the
    # unchanged one-second input/render budgets only after native readiness.
    wait_for(lambda: screen.contains(b'BOOTGATE'), 'initial rendered readiness', budget=60)
    send(b'!')
    wait_for(lambda: screen.contains(b'READYBURST'), 'isolated input readiness')
    steady_start = len(transcript)
    burst('quiet', b'x')
    send(b'i')
    wait_for(lambda: screen.contains(b'IDLEGATE'), 'static frame readiness')
    # Bounded event-driven settling, then a full observation window. A sleep
    # alone cannot distinguish startup traffic from routine unchanged presents.
    settling = time.monotonic()
    last_activity = settling
    while time.monotonic() - last_activity < 0.15:
        assert time.monotonic() - settling < 2, 'idle never settled: bytes=%d elapsed=%.6f' % (
            len(transcript) - steady_start, time.monotonic() - settling)
        if pump(0.15 - (time.monotonic() - last_activity)):
            last_activity = time.monotonic()
    idle_start = len(transcript)
    start = time.monotonic()
    while time.monotonic() - start < 0.30:
        pump(0.30 - (time.monotonic() - start))
    measurements['idle'] = {'seconds': time.monotonic() - start, 'bytes': len(transcript) - idle_start}
    assert len(transcript) == idle_start, 'unchanged frame emitted traffic: %r bytes=%r' % (
        measurements, bytes(transcript[idle_start:idle_start + 256]))
    send(b'g')
    wait_for(lambda: screen.tick() >= 10, 'continuous output readiness')
    before = screen.tick()
    busy_start = time.monotonic()
    burst('busy', b'y', before + 10)
    measurements['fairness'] = {'before_tick': before, 'after_tick': screen.tick(),
                                'seconds': time.monotonic() - busy_start,
                                'budget_seconds': BUDGET}
    steady = bytes(transcript[steady_start:])
    clears = len(re.findall(rb'\x1b\[(?:0*2|0*3)J', steady))
    measurements['routine_full_clears'] = clears
    assert clears == 0, 'routine full clear after readiness: %r' % measurements
    send(b'q')
    wait_for(lambda: screen.contains(b'PROTOCOLGATE'), 'application input-mode readiness')
    protocol_start = time.monotonic()
    send('hé界'.encode() + b'\x1b[A\x1b[200~' + 'paste界\n'.encode() +
         b'\x1b[201~\x1b[<0;7;4M\x1b[<0;7;4m')
    wait_for(lambda: screen.contains(b'PROTOCOLACK'), 'Unicode/cursor/paste/mouse round trip', 2)
    measurements['protocol'] = {'seconds': time.monotonic() - protocol_start, 'budget_seconds': 2}
    send(b'!')
    wait_for(lambda: screen.contains(b'FINALTAIL'), 'visible final tail')
    send(b'!')
    wait_for(lambda: child.poll() is not None, 'restored owner completion', 30, completion=True)
    while pump(0): pass
    assert child.returncode == 0, 'terminal owner failed'
    with open(root + '/owner-report.json') as source:
        report = json.load(source)
    assert report['restored'] and report['exit_code'] == 0, report
    assert b'OUTER_OWNER_RESTORATION_CHECKED' in transcript, 'missing post-restoration terminal use'
    # The owner compares termios after BMUX exits; terminal escape restoration
    # must also precede JSON, not merely happen during process teardown.
    json_start = -1
    position = 0
    for line in bytes(transcript).splitlines(keepends=True):
        brace = line.find(b'{')
        if brace >= 0:
            try:
                value = json.loads(line[brace:])
                if value.get('schema_version') == 4:
                    json_start = position + brace
            except (ValueError, AttributeError):
                pass
        position += len(line)
    restore = transcript.rfind(b'\x1b[?1049l', 0, json_start)
    assert json_start >= 0 and restore >= 0, 'missing restore-before-JSON: %r' % transcript[-4096:]
    report['measurements'] = measurements
    print(json.dumps(report))
finally:
    with open(root + '/retained-outer-transcript.bin', 'wb') as output:
        output.write(transcript)
    with open(root + '/retained-measurements.json', 'w') as output:
        json.dump(measurements, output)
    if child.poll() is None:
        child.kill()  # only our exact owner handle; retain uncertain runtime state
        child.wait(timeout=5)
    os.close(master)
    os.close(slave)
"#;

#[test]
#[serial]
fn interactive_record_retained_renderer_burst_fairness_and_idle() {
    let fixture = Fixture::new();
    let owner = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/interactive-recording/outer_pty.py");
    let mut command = Command::new("python3");
    command
        .arg("-c")
        .arg(RETAINED_RENDERER_ACCEPTANCE)
        .arg(&fixture.root)
        .arg(&fixture.binary)
        .arg(owner);
    fixture.isolate(&mut command);
    let output = command
        .stdin(Stdio::null())
        .output()
        .expect("required Python 3 real controlling PTY acceptance");
    assert!(
        output.status.success(),
        "retained renderer acceptance failed; root={}; stdout={}; stderr={}",
        fixture.root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("measured acceptance report");
    eprintln!("retained renderer measurements: {}", report["measurements"]);
    let transcript = fs::read(fixture.root.join("retained-outer-transcript.bin"))
        .expect("outer rendered evidence");
    let result = final_json(&transcript);
    assert_eq!(result["status"]["exit_code"], 0);
    assert_eq!(result["status"]["durable_recording_complete"], true);
    let raw = fs::read(fixture.root.join("exports/output.pty")).expect("canonical raw export");
    for key in [b'x', b'y'] {
        let echo = [key; 64];
        assert!(
            raw.windows(echo.len()).any(|window| window == echo),
            "missing complete 64-key native echo: {report}"
        );
    }
    assert!(
        raw.ends_with(b"\x1b[7;1HFINALTAIL\x1b[?1049l"),
        "missing canonical final tail: {report}"
    );
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

#[cfg(target_os = "linux")]
#[test]
#[serial]
fn interactive_restores_terminal_while_descendant_capture_remains_pending() {
    let fixture = Fixture::new();
    struct ReleaseSlave(PathBuf);
    impl Drop for ReleaseSlave {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, b"");
        }
    }
    let release = ReleaseSlave(fixture.root.join("release-slave"));
    let (report, transcript) = fixture.interactive("pending-capture");
    assert_eq!(report["restored"], true);
    assert_eq!(report["exit_code"], 1);
    let result = final_json(&transcript);
    let status = &result["status"];
    assert_eq!(status["target_exited"], true);
    assert_eq!(status["exit_code"], 0);
    assert_eq!(status["output_drained"], false);
    assert_eq!(status["capture_finalized"], false);
    assert_eq!(status["durable_recording_complete"], false);
    assert_eq!(result["terminal_restored"], true);
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("capture pending"),
        "{result}"
    );
    assert!(!fixture.root.join("exports").exists());
    drop(release); // fixture-owned descriptor holder exits voluntarily.
    let run = status["run_id"].as_str().unwrap();
    let mut wait = Command::new(&fixture.binary);
    fixture.isolate(&mut wait);
    let output = wait
        .args([
            "--runtime",
            "default",
            "--target",
            "local",
            "recording",
            "process",
            "wait",
            run,
            "--exit",
            "--timeout",
            "10s",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(observed["output_drained"], true, "{observed}");
    let mut finish = Command::new(&fixture.binary);
    fixture.isolate(&mut finish);
    let output = finish
        .args([
            "--runtime",
            "default",
            "--target",
            "local",
            "recording",
            "process",
            "finish",
            run,
            "--output",
            fixture.root.join("completed").to_str().unwrap(),
            "--export",
            "raw",
            "--cleanup-server",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root.join("completed/output.pty").is_file());
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
