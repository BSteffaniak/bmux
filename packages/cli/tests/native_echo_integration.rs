//! Real daemon + native key reducer + retained headless sink. No window/GPU/display claim.
#![cfg(unix)]
#[allow(dead_code)] // Reuse paths/binary selection, not the ambient-environment startup helpers.
mod support;

use anyhow::{Context, Result, bail, ensure};
use bmux_cli::attach::{
    AttachExitReason, NativeAttachEvent, NativeAttachHost, initialize_native_runtime_state,
    run_native_with_client,
};
use bmux_config::{BmuxConfig, ConfigLoadOverrides, ConfigPaths};
use bmux_frontend_runtime::{
    input::{InputEvent, KeyEvent, KeyPhase, LogicalKey, Modifiers, NamedKey, PhysicalKey},
    presentation::{PreparedDelivery, PresentationIdentity},
};
use bmux_pane_runtime_plugin_api::pane_runtime_commands::client as panes;
use std::{
    io::{Read as _, Seek as _, SeekFrom},
    os::unix::fs::PermissionsExt as _,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

static NATIVE_RUNTIME: Mutex<()> = Mutex::new(());

const DEADLINE: Duration = Duration::from_secs(10);
const WORKLOAD: &str = r#"#!/bin/sh
exec 2> "$0.stderr"
printf 'started pid=%s argv0=%s cwd=%s\n' "$$" "$0" "$PWD" > "$0.progress"
stty -echo -icanon min 1 time 0 || exit 1
printf 'ready\n' >> "$0.progress"
printf '\033[2J\033[HECHO_READY\r\n'
n=0
while [ "$n" -lt 128 ] && IFS= read -r token; do
    [ "${#token}" -eq 1 ] || { printf 'invalid token length=%s\n' "${#token}" >&2; exit 2; }
    n=$((n + 1))
    printf '\033[2;1H\033[2KACK:%06d:%s\r\n' "$n" "$token"
done
"#;

struct Daemon {
    env: support::ServerEnv,
    child: Child,
}

fn isolated_command(env: &support::ServerEnv, args: &[&str]) -> Command {
    let mut command = env.command(args);
    // env_clear also removes inherited BMUX targets, overlays, attachment tokens,
    // shell startup hooks, and managed-bundle authority. Never mutate test-process env.
    command
        .env_clear()
        .current_dir(env.root())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", env.root().join("home"))
        .env("TMPDIR", env.root().join("tmp"))
        .env("XDG_CONFIG_HOME", &env.config_dir)
        .env("XDG_RUNTIME_DIR", &env.runtime_dir)
        .env("XDG_DATA_HOME", &env.data_dir)
        .env("XDG_STATE_HOME", &env.state_dir)
        .env("XDG_CACHE_HOME", env.root().join("cache"))
        .env("BMUX_CONFIG_DIR", &env.config_dir)
        .env("BMUX_RUNTIME_DIR", &env.runtime_dir)
        .env("BMUX_DATA_DIR", &env.data_dir)
        .env("BMUX_STATE_DIR", &env.state_dir)
        .env("BMUX_LOG_DIR", &env.log_dir)
        .env("BMUX_CONFIG", env.config_dir.join("bmux.toml"))
        .env("BMUX_NO_BASE_CONFIG", "1")
        .env("BMUX_RUNTIME_NAME", "default")
        .env("BMUX_TARGET", "local")
        .env("TERM", "xterm-256color")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    command
}

fn bounded_tail(path: &std::path::Path) -> String {
    let read = || -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(8192)))?;
        let mut bytes = Vec::new();
        file.take(8192).read_to_end(&mut bytes)?;
        Ok(format!("{:?}", String::from_utf8_lossy(&bytes)))
    };
    read().unwrap_or_else(|error| format!("unavailable: {error}"))
}

fn wait_child(child: &mut Child, deadline: Duration) -> Result<std::process::ExitStatus> {
    let until = Instant::now() + deadline;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= until {
            bail!("owned child exit deadline expired");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Daemon {
    fn evidence(&self) -> String {
        let mut reports = [
            "echo-server.stdout",
            "echo-server.stderr",
            "echo-stop.stdout",
            "echo-stop.stderr",
            "echo-workload.stderr",
            "echo-workload.progress",
        ]
        .into_iter()
        .map(|name| format!("{name}: {}", bounded_tail(&self.env.root().join(name))))
        .collect::<Vec<_>>();
        if let Ok(entries) = std::fs::read_dir(&self.env.log_dir) {
            for entry in entries.take(4).flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    reports.push(format!(
                        "log {:?}: {}",
                        entry.file_name(),
                        bounded_tail(&entry.path())
                    ));
                }
            }
        }
        reports.join("\n")
    }

    fn start() -> Result<Self> {
        let env = support::ServerEnv::new("native-echo");
        std::fs::set_permissions(env.root(), std::fs::Permissions::from_mode(0o700))?;
        for name in ["home", "tmp", "cache"] {
            let path = env.root().join(name);
            std::fs::create_dir(&path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        let workload = env.root().join("echo-workload");
        std::fs::write(&workload, WORKLOAD)?;
        std::fs::set_permissions(&workload, std::fs::Permissions::from_mode(0o700))?;
        env.write_config(&format!(
            "[general]\ndefault_shell = {}\n[behavior]\nrestore_last_layout = false\npane_shell_integration = false\n",
            serde_json::to_string(&workload.to_string_lossy())?,
        ));
        let child = isolated_command(
            &env,
            &[
                "--runtime",
                "default",
                "--target",
                "local",
                "server",
                "start",
                "--foreground-internal",
            ],
        )
        .stdout(std::fs::File::create(
            env.root().join("echo-server.stdout"),
        )?)
        .stderr(std::fs::File::create(
            env.root().join("echo-server.stderr"),
        )?)
        .spawn()?;
        Ok(Self { env, child })
    }

    fn paths(&self) -> ConfigPaths {
        ConfigPaths::new(
            self.env.config_dir.clone(),
            self.env.runtime_dir.clone(),
            self.env.data_dir.clone(),
            self.env.state_dir.clone(),
        )
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            ensure!(status.success(), "owned daemon exited unsuccessfully");
            return Ok(());
        }
        let mut stop = isolated_command(
            &self.env,
            &[
                "--runtime",
                "default",
                "--target",
                "local",
                "server",
                "stop",
            ],
        )
        .stdout(std::fs::File::create(
            self.env.root().join("echo-stop.stdout"),
        )?)
        .stderr(std::fs::File::create(
            self.env.root().join("echo-stop.stderr"),
        )?)
        .spawn()?;
        let result = wait_child(&mut stop, DEADLINE).context("stage=stop-command-exit");
        if result.is_err() {
            let _ = stop.kill();
            let _ = stop.wait();
        }
        ensure!(result?.success(), "owned server stop failed");
        ensure!(
            wait_child(&mut self.child, DEADLINE)
                .context("stage=daemon-exit")?
                .success(),
            "owned daemon failed"
        );
        Ok(())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.stop().is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn key(logical: LogicalKey, text: Option<String>) -> NativeAttachEvent {
    NativeAttachEvent::Input(InputEvent::Key(KeyEvent {
        physical: PhysicalKey::Unidentified,
        logical,
        phase: KeyPhase::Press,
        modifiers: Modifiers::default(),
        text,
    }))
}

struct Expected {
    text: String,
    admitted: Instant,
}

type Acceptance = (PresentationIdentity, u64);

struct SinkEvidence {
    attempts: u64,
    last_identity: Option<PresentationIdentity>,
    last_rows: Vec<String>,
    last_rejection: Option<String>,
}

struct RetainedSink {
    expected_session: uuid::Uuid,
    viewport_generation: u64,
    expected: Arc<Mutex<Option<Expected>>>,
    accepted: tokio::sync::mpsc::Sender<Acceptance>,
    retained: Option<PreparedDelivery>,
    evidence: Arc<Mutex<SinkEvidence>>,
}

fn contains_exact_marker(row: &str, marker: &str) -> bool {
    row.split(|character: char| {
        !character.is_ascii_alphanumeric() && character != ':' && character != '_'
    })
    .any(|word| word == marker)
}

impl RetainedSink {
    fn accept(&mut self, delivery: PreparedDelivery) -> Result<()> {
        let result = self.accept_inner(delivery);
        if let Err(error) = &result
            && let Ok(mut evidence) = self.evidence.lock()
        {
            evidence.last_rejection = Some(format!("{error:#}"));
        }
        result
    }

    fn accept_inner(&mut self, delivery: PreparedDelivery) -> Result<()> {
        {
            let mut evidence = self
                .evidence
                .lock()
                .map_err(|_| anyhow::anyhow!("sink evidence poisoned"))?;
            evidence.attempts = evidence.attempts.saturating_add(1);
            evidence.last_identity = Some(delivery.identity);
            let buffer = delivery.prepared.buffer();
            evidence.last_rows = (buffer.area().y..buffer.area().bottom())
                .take(24)
                .filter_map(|y| buffer.row_symbols(y))
                .map(|row| row.chars().take(160).collect())
                .collect();
        }
        ensure!(
            delivery.identity.attached_id == self.expected_session,
            "wrong attached resource: expected={} actual={:?}",
            self.expected_session,
            delivery.identity
        );
        ensure!(
            delivery.identity.viewport_generation == self.viewport_generation,
            "wrong viewport"
        );
        if let Some(last) = self.retained.as_ref().map(|delivery| delivery.identity) {
            ensure!(
                delivery.identity.epoch == last.epoch,
                "unexpected reconnect"
            );
            ensure!(
                delivery.identity.revision > last.revision,
                "non-advancing output"
            );
        }
        // Inspect the canonical complete retained output, not a parallel screen model.
        let buffer = delivery.prepared.buffer();
        ensure!(buffer.cells().len() <= 80 * 24, "unbounded prepared output");
        let mut pending = self
            .expected
            .lock()
            .map_err(|_| anyhow::anyhow!("expectation poisoned"))?;
        let matched = pending.as_ref().is_some_and(|wanted| {
            (buffer.area().y..buffer.area().bottom()).any(|y| {
                buffer
                    .row_symbols(y)
                    .is_some_and(|row| contains_exact_marker(&row, &wanted.text))
            })
        });
        let identity = delivery.identity;
        // One complete immutable retained delivery is the actual headless sink.
        // No pixels or metadata become an independent scene authority.
        let notification = if matched {
            let wanted = pending.as_ref().context("matched expectation missing")?;
            let permit = self
                .accepted
                .try_reserve()
                .context("bounded acceptance notification unavailable")?;
            Some((permit, wanted.admitted))
        } else {
            None
        };
        // No fallible operation follows replacing the retained sink output.
        self.retained = Some(delivery);
        if let Some((permit, admitted)) = notification {
            let elapsed = u64::try_from(admitted.elapsed().as_nanos()).unwrap_or(u64::MAX);
            *pending = None;
            permit.send((identity, elapsed));
        }
        // Same-thread controller cannot consume notification until this returns.
        Ok(())
    }
}

async fn echo_controller(
    handle: &bmux_cli::attach::NativeAttachHandle,
    expected: &Mutex<Option<Expected>>,
    accepted: &mut tokio::sync::mpsc::Receiver<Acceptance>,
    committed: tokio::sync::oneshot::Receiver<()>,
    samples: usize,
    warmup: usize,
    paste: bool,
) -> Result<Vec<u64>> {
    tokio::time::timeout(DEADLINE, committed)
        .await
        .context("stage=attach-committed timeout")?
        .context("stage=attach-committed sender dropped")?;
    tokio::time::timeout(DEADLINE, accepted.recv())
        .await
        .context("stage=ready-retained-output timeout waiting ECHO_READY")?
        .context("stage=ready-retained-output channel closed")?;
    let mut timings = Vec::with_capacity(samples);
    for index in 0..samples + warmup {
        let token = char::from(b'a' + u8::try_from(index % 26)?).to_string();
        *expected
            .lock()
            .map_err(|_| anyhow::anyhow!("expectation poisoned"))? = Some(Expected {
            text: format!("ACK:{:06}:{token}", index + 1),
            admitted: Instant::now(),
        });
        if paste {
            handle.try_send(NativeAttachEvent::PasteFromClipboard)?;
        } else {
            // Real native character and Enter events, not direct PTY input.
            handle
                .try_send(key(LogicalKey::Character(token.clone()), Some(token)))
                .with_context(|| format!("stage=character-admission sequence={}", index + 1))?;
            handle
                .try_send(key(LogicalKey::Named(NamedKey::Enter), None))
                .with_context(|| format!("stage=enter-admission sequence={}", index + 1))?;
        }
        let (_, elapsed) = tokio::time::timeout(DEADLINE, accepted.recv())
            .await
            .with_context(|| format!("stage=ack-retained-output timeout sequence={}", index + 1))?
            .context("stage=ack-retained-output channel closed")?;
        if index >= warmup {
            timings.push(elapsed);
        }
    }
    Ok(timings)
}

async fn exercise(
    samples: usize,
    warmup: usize,
    resize_at_startup: bool,
    paste: bool,
) -> Result<Vec<u64>> {
    ensure!(
        samples > 0 && samples + warmup < 128,
        "workload sample budget exceeded",
    );
    let mut daemon = Daemon::start().context("stage=daemon-spawn")?;
    let evidence = Arc::new(Mutex::new(SinkEvidence {
        attempts: 0,
        last_identity: None,
        last_rows: Vec::new(),
        last_rejection: None,
    }));
    let result = exercise_attached(
        &mut daemon,
        Arc::clone(&evidence),
        samples,
        warmup,
        resize_at_startup,
        paste,
    )
    .await;
    // Always observe real stop/exit even when the ready/ACK controller failed.
    let teardown = daemon.stop();
    if result.is_err() || teardown.is_err() {
        let failure = format!(
            "echo failure: run={:?}; teardown={:?}; fixture={} binary={}\n{}\n{}",
            result.as_ref().err(),
            teardown.as_ref().err(),
            daemon.env.root().display(),
            support::bmux_binary().display(),
            sink_evidence(&evidence),
            daemon.evidence(),
        );
        if teardown.is_err() {
            let killed = daemon.child.kill();
            let reaped = daemon.child.wait();
            eprintln!("echo fallback daemon cleanup: kill={killed:?} wait={reaped:?}");
        }
        // ServerEnv removes evidence on a returned Err (unlike a panic). The child
        // has been stopped/reaped above; intentionally retain only this failed
        // fixture and its ownership metadata for coordinator inspection.
        std::mem::forget(daemon);
        bail!("{failure}");
    }
    result
}

fn sink_evidence(evidence: &Mutex<SinkEvidence>) -> String {
    evidence.lock().map_or_else(
        |_| "sink evidence poisoned".into(),
        |evidence| {
            format!(
                "sink attempts={} last_identity={:?} rejection={:?} rows={:?}",
                evidence.attempts,
                evidence.last_identity,
                evidence.last_rejection,
                evidence.last_rows
            )
        },
    )
}

async fn exercise_attached(
    daemon: &mut Daemon,
    evidence: Arc<Mutex<SinkEvidence>>,
    samples: usize,
    warmup: usize,
    resize_at_startup: bool,
    paste: bool,
) -> Result<Vec<u64>> {
    let paths = daemon.paths();
    let mut config = BmuxConfig::load_isolated_with_paths_and_overrides(
        &paths,
        &ConfigLoadOverrides::default(),
    )?;
    if paste {
        config.behavior.bracketed_paste = false;
    }
    let until = Instant::now() + Duration::from_secs(30);
    let mut client = loop {
        ensure!(
            daemon.child.try_wait()?.is_none(),
            "owned daemon exited during readiness"
        );
        match tokio::time::timeout(
            Duration::from_secs(1),
            bmux_client::BmuxClient::connect_with_paths_and_timeout(
                &paths,
                "native-echo",
                Duration::from_secs(1),
            ),
        )
        .await
        {
            Ok(Ok(client)) => break client,
            _ if Instant::now() < until => tokio::time::sleep(Duration::from_millis(25)).await,
            _ => bail!("owned daemon handshake deadline expired"),
        }
    };
    client
        .require_service_contracts()
        .context("stage=service-contracts")?;
    let readiness =
        bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::runtime_readiness(
            &mut client,
        )
        .await
        .context("stage=helper-readiness query")?;
    ensure!(
        readiness.runtime_dir == paths.runtime_dir.to_string_lossy(),
        "stage=helper-readiness runtime mismatch"
    );
    ensure!(
        readiness.helper_ready,
        "stage=helper-readiness unavailable: {:?}",
        readiness.helper_error
    );
    let session = panes::new_session_with_runtime(&mut client, Some("native-echo".into()))
        .await
        .context("stage=session-create transport")?
        .map_err(|error| anyhow::anyhow!("stage=session-create: {error:?}"))?
        .session_id;
    let expected = Arc::new(Mutex::new(Some(Expected {
        text: "ECHO_READY".into(),
        admitted: Instant::now(),
    })));
    let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::channel(1);
    let mut sink = RetainedSink {
        expected_session: session,
        viewport_generation: if resize_at_startup { 3 } else { 1 },
        expected: Arc::clone(&expected),
        accepted: accepted_tx,
        retained: None,
        evidence,
    };
    let (handle, mut host) =
        NativeAttachHost::channel(80, 24, 1, move |delivery| sink.accept(delivery))?;
    if resize_at_startup {
        for viewport_generation in [2, 3] {
            handle.try_send(NativeAttachEvent::Resize {
                columns: 80,
                rows: 24,
                viewport_generation,
            })?;
        }
    }
    if paste {
        host.set_clipboard(|request| {
            ensure!(
                matches!(request, bmux_cli::attach::NativeClipboardRequest::ReadText),
                "unexpected clipboard write"
            );
            Ok(Some("a\n".into()))
        });
    }
    host.set_config_paths(paths.clone());
    let reload_paths = paths.clone();
    host.set_config_loader(move || {
        BmuxConfig::load_isolated_with_paths_and_overrides(
            &reload_paths,
            &ConfigLoadOverrides::default(),
        )
    });
    let (committed_tx, committed_rx) = tokio::sync::oneshot::channel();
    host.set_attach_committed(move || {
        let _ = committed_tx.send(());
    });
    initialize_native_runtime_state(&config, &paths)
        .context("stage=native-plugin-initialization")?;
    let controller = async {
        let result = echo_controller(
            &handle,
            &expected,
            &mut accepted_rx,
            committed_rx,
            samples,
            warmup,
            paste,
        )
        .await;
        // Even a missing ACK must request detach before surfacing the failure.
        let close = handle.try_send(NativeAttachEvent::Close);
        result.and_then(|timings| {
            close.context("stage=detach-admission")?;
            Ok(timings)
        })
    };
    let target = session.to_string();
    let (attached, timings) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(
            run_native_with_client(client, &target, config, host),
            controller
        )
    })
    .await
    .context("stage=attach-run/controller/detach overall deadline expired")?;
    let outcome = attached.with_context(|| {
        format!(
            "stage=attach-run controller_error={:?}",
            timings.as_ref().err()
        )
    })?;
    ensure!(
        outcome.exit_reason == AttachExitReason::Detached,
        "unexpected attach exit"
    );
    ensure!(outcome.status_code == 0, "native attach failed");
    timings
}

fn run(samples: usize, warmup: usize) -> Result<Vec<u64>> {
    // Native configuration/plugin state is thread-local. Poll attach and controller
    // together here, never spawn the attach future onto a migrating executor worker.
    let _guard = NATIVE_RUNTIME.lock().unwrap();
    // Native plugin initialization uses process-global registries. These real
    // runtime exercises must not initialize them concurrently in cargo test.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(exercise(samples, warmup, false, false))
}

#[test]
fn retained_markers_match_borders_but_not_other_sequences() {
    assert!(contains_exact_marker(
        "│ECHO_READY                    │",
        "ECHO_READY"
    ));
    assert!(contains_exact_marker(
        "│ACK:000001:a                  │",
        "ACK:000001:a"
    ));
    assert!(!contains_exact_marker("ACK:000002:a", "ACK:000001:a"));
    assert!(!contains_exact_marker("ACK:000001:ab", "ACK:000001:a"));
}

#[test]
fn startup_resize_catches_up_before_native_key_echo() -> Result<()> {
    let _guard = NATIVE_RUNTIME.lock().unwrap();
    let samples = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(exercise(2, 0, true, false))?;
    ensure!(samples.len() == 2, "missing resized echo samples");
    Ok(())
}

#[test]
fn native_clipboard_paste_reaches_pty_without_outer_capture() -> Result<()> {
    let _guard = NATIVE_RUNTIME.lock().unwrap();
    let samples = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(exercise(1, 0, false, true))?;
    ensure!(samples.len() == 1, "missing clipboard paste ACK");
    Ok(())
}

#[test]
fn native_key_echo_reaches_complete_retained_acceptance() -> Result<()> {
    ensure!(run(4, 0)?.len() == 4, "missing echo samples");
    Ok(())
}

#[test]
#[ignore = "opt-in real-daemon admission-to-retained-output benchmark; no GPU/display"]
fn native_key_input_admission_to_retained_output_acceptance_benchmark() -> Result<()> {
    let mut samples = run(96, 8)?;
    samples.sort_unstable();
    println!(
        "native_key_admission_to_retained_acceptance samples={} p50_ns={} p95_ns={} p99_ns={}",
        samples.len(),
        samples[47],
        samples[91],
        samples[95],
    );
    Ok(())
}
