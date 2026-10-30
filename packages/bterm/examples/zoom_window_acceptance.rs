//! Opt-in native Application/GPU acceptance, with no daemon or user attachment.
//! Synthetic input enters Application directly, not the OS keyboard event queue.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use bmux_frontend_runtime::presentation::{PreparedDelivery, PresentationIdentity};
use bmux_tui::{
    damage::Damage, geometry::Rect, paint::LocalRect, presentation::PresentationState, text::Line,
};
use bterm::app::{Application, AttachWorker, PresentationViewport};
use bterm::configuration::NativeConfiguration;
use bterm::window::NativeOutput;
use bterm::zoom::{ZoomAction, ZoomBinding, ZoomModifier};
use bterm_native_platform::input::{
    InputEvent, KeyEvent, KeyPhase, LogicalKey, Modifiers, PhysicalKey, ScrollDelta,
};
use bterm_native_platform::{
    PlatformError, PlatformEvent, WindowApplication, WindowContext, WindowOptions,
};

const LIMIT: Duration = Duration::from_secs(60);
const WATCHDOG_LIMIT: Duration = Duration::from_secs(65);
const DELAY: Duration = Duration::from_millis(40);

#[derive(Clone, Copy, Debug)]
struct Commit {
    viewport: PresentationViewport,
    cell: [f32; 2],
    count: u64,
}

#[derive(Default)]
struct Shared {
    // Exactly one replaceable queued viewport, plus one in-flight delivery.
    pending: Option<PresentationViewport>,
    latest: Option<PresentationViewport>,
    stop: bool,
    commit: Option<Commit>,
    forwarded: Vec<InputEvent>,
    attempts: u64,
    rejected: u64,
    accepted: u64,
    invalidations: u64,
    reload: Option<NativeConfiguration>,
    reloads_taken: u64,
    inject_failure: bool,
    injected_rejections: u64,
    error: Option<String>,
}
type State = Arc<(Mutex<Shared>, Condvar)>;

struct Worker {
    state: State,
    producer: Option<JoinHandle<()>>,
}

impl Worker {
    fn enqueue(&self, viewport: PresentationViewport) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().unwrap();
        state.latest = Some(viewport);
        state.pending = Some(viewport);
        changed.notify_one();
    }
}

fn produce(state: &State, output: &NativeOutput) -> Result<()> {
    let mut revision = 0_u64;
    loop {
        let viewport = {
            let (lock, changed) = &**state;
            let mut shared = lock.lock().unwrap();
            while shared.pending.is_none() && !shared.stop {
                shared = changed.wait(shared).unwrap();
            }
            if shared.stop {
                return Ok(());
            }
            shared.pending.take().unwrap()
        };
        // Deliberately slower than the 16ms animation scheduler. Never hold the
        // mailbox lock while delaying, preparing, or awaiting synchronous output.
        thread::sleep(DELAY);
        revision += 1;
        let area = Rect::new(0, 0, viewport.columns, viewport.rows);
        ensure!(
            usize::from(area.width) * usize::from(area.height) <= 100_000,
            "acceptance viewport exceeds 100000-cell budget"
        );
        let mut presentation = PresentationState::new(area);
        let prepared = presentation.prepare(Damage::Full, |cx| {
            for row in 0..area.height {
                cx.write_line(LocalRect::new(0, i64::from(row), area.width, 1), &Line::from(
                    format!("native zoom acceptance {revision} row {row}: café e\u{301} 界 0123456789")
                ));
            }
        }).context("full preparation produced no delivery")?;
        let (inject_failure, committed_before) = {
            let mut shared = state.0.lock().unwrap();
            (
                std::mem::take(&mut shared.inject_failure),
                shared.commit.map_or(0, |commit| commit.count),
            )
        };
        let identity = PresentationIdentity {
            epoch: "00000000-0000-4000-8000-000000000001".parse()?,
            attached_id: "00000000-0000-4000-8000-000000000002".parse()?,
            revision,
            // A neighboring generation may become current during animation.
            // MAX cannot occur in this finite run, and is deliberately not
            // classified as superseded, so Application schedules its retry.
            // This is provenance rejection, not injected GPU/device failure.
            viewport_generation: if inject_failure {
                u64::MAX
            } else {
                viewport.generation
            },
        };
        state.0.lock().unwrap().attempts += 1;
        let result = presentation.present_prepared(Some(prepared), |prepared, _| {
            output.present(PreparedDelivery {
                identity,
                pointer_shape: None,
                prepared: Arc::new(prepared.clone()),
            })
        });
        if matches!(result, Ok(Some(()))) {
            let empty = presentation.prepare(Damage::None, |_| panic!("empty damage painted"));
            ensure!(
                empty.is_none(),
                "settled empty damage unexpectedly prepared output"
            );
            let skipped = presentation.present_prepared(empty, |_, _| -> Result<()> {
                bail!("empty damage reached output callback")
            })?;
            ensure!(skipped.is_none(), "empty damage did not return Ok(None)");
        }
        let mut shared = state.0.lock().unwrap();
        if inject_failure {
            ensure!(
                result.is_err(),
                "injected wrong-viewport delivery was accepted"
            );
            ensure!(
                shared.commit.map_or(0, |commit| commit.count) == committed_before,
                "rejected delivery advanced committed output"
            );
            shared.injected_rejections += 1;
        }
        match result {
            Ok(Some(())) => shared.accepted += 1,
            Ok(None) => bail!("full output unexpectedly skipped"),
            Err(error) => {
                shared.rejected += 1;
                // Supersession/suspension may reject a complete delivery. The
                // application controls retry; never fabricate a commit here.
                eprintln!(
                    "acceptance: rejected output generation {}: {error:#}",
                    viewport.generation
                );
            }
        }
    }
}

impl AttachWorker for Worker {
    fn take_native_configuration(&mut self) -> Option<NativeConfiguration> {
        let mut state = self.state.0.lock().unwrap();
        let reload = state.reload.take();
        if reload.is_some() {
            state.reloads_taken += 1;
        }
        reload
    }
    fn start(&mut self, output: NativeOutput, viewport: PresentationViewport) -> Result<()> {
        ensure!(self.producer.is_none(), "worker started twice");
        let state = Arc::clone(&self.state);
        self.producer = Some(
            thread::Builder::new()
                .name("zoom-acceptance-output".into())
                .spawn(move || {
                    if let Err(error) = produce(&state, &output) {
                        state.0.lock().unwrap().error = Some(format!("{error:#}"));
                    }
                })?,
        );
        self.enqueue(viewport);
        Ok(())
    }
    fn event(&mut self, event: PlatformEvent, viewport: PresentationViewport) -> Result<()> {
        match event {
            PlatformEvent::Resized(_) | PlatformEvent::Resumed => self.enqueue(viewport),
            PlatformEvent::Input(input) => {
                let mut state = self.state.0.lock().unwrap();
                ensure!(state.forwarded.len() < 64, "unexpected input flood");
                state.forwarded.push(input);
            }
            _ => {}
        }
        Ok(())
    }
    fn retry_presentation(&mut self, generation: u64) -> Result<()> {
        let latest = self.state.0.lock().unwrap().latest;
        if let Some(viewport) = latest.filter(|viewport| viewport.generation == generation) {
            self.enqueue(viewport);
        }
        Ok(())
    }
    fn detach(&mut self) {
        let (lock, changed) = &*self.state;
        lock.lock().unwrap().stop = true;
        changed.notify_one();
        if let Some(producer) = self.producer.take() {
            // Application closes NativeOutput before detaching, cancelling any
            // blocked receipt. A separate process watchdog bounds shutdown too.
            let _ = producer.join();
        }
    }
    fn check(&mut self) -> Result<bool> {
        if let Some(error) = &self.state.0.lock().unwrap().error {
            bail!("producer: {error}");
        }
        Ok(false)
    }
    fn presented(
        &mut self,
        identity: PresentationIdentity,
        viewport: PresentationViewport,
        cell: [f32; 2],
    ) {
        let mut state = self.state.0.lock().unwrap();
        if identity.viewport_generation != viewport.generation {
            state.error = Some("commit identity/viewport mismatch".into());
        }
        let count = state.commit.map_or(1, |commit| commit.count + 1);
        state.commit = Some(Commit {
            viewport,
            cell,
            count,
        });
    }
    fn invalidate_presentation(&mut self) {
        self.state.0.lock().unwrap().invalidations += 1;
    }
    fn accessibility(
        &mut self,
        _: PresentationIdentity,
        _: bterm_native_platform::accessibility::AccessibilityAction,
    ) -> Result<()> {
        Ok(())
    }
}

fn primary() -> Modifiers {
    Modifiers {
        super_key: cfg!(target_os = "macos"),
        control: !cfg!(target_os = "macos"),
        ..Modifiers::default()
    }
}

struct Harness {
    application: Application<Worker>,
    state: State,
    passed: Arc<AtomicBool>,
    started: Instant,
    stage_started: Instant,
    stage: usize,
    baseline: Option<[f32; 2]>,
    dpi: Option<f64>,
    before: u64,
    policy: NativeConfiguration,
    reload_generation: u64,
    tiny_seen: u64,
    tiny_since: Instant,
}

impl Harness {
    fn inject(
        &mut self,
        context: &mut WindowContext<'_>,
        input: InputEvent,
    ) -> Result<(), PlatformError> {
        self.application.event(context, PlatformEvent::Input(input))
    }
    fn key(
        &mut self,
        context: &mut WindowContext<'_>,
        text: &str,
        modifiers: Modifiers,
    ) -> Result<(), PlatformError> {
        for phase in [KeyPhase::Press, KeyPhase::Repeat, KeyPhase::Release] {
            // Repeats are part of acceptance; use an exact-size binding or
            // compensate expected multiplicative targets for in/out repeats.
            self.inject(
                context,
                InputEvent::Key(KeyEvent {
                    physical: PhysicalKey::Code("acceptance-position".into()),
                    logical: LogicalKey::Character(text.into()),
                    phase,
                    modifiers,
                    text: None,
                }),
            )?;
        }
        Ok(())
    }
    // One sequential acceptance state machine keeps each expected ratio beside
    // its injected transition; splitting it would obscure stage ordering.
    #[allow(clippy::too_many_lines)]
    fn drive(&mut self, context: &mut WindowContext<'_>) -> Result<(), PlatformError> {
        let fail = |message: String| PlatformError::Application(message);
        if self.started.elapsed() > LIMIT {
            return Err(fail(format!(
                "acceptance timed out at stage {}",
                self.stage
            )));
        }
        let (commit, forwarded, error) = {
            let state = self.state.0.lock().unwrap();
            (state.commit, state.forwarded.len(), state.error.clone())
        };
        if let Some(error) = error {
            return Err(fail(error));
        }
        let Some(commit) = commit else {
            return Ok(());
        };
        if let Some(dpi) = self.dpi {
            if context.metrics().scale_factor != dpi {
                return Err(fail(
                    "DPI changed during controlled run; rerun on one display".into(),
                ));
            }
        } else {
            self.dpi = Some(context.metrics().scale_factor);
        }
        if self.baseline.is_none() {
            self.baseline = Some(commit.cell);
            self.before = commit.count;
            self.stage_started = Instant::now();
            // An unmatched key must reach the worker (all three phases), while
            // all following configured key/wheel events must be consumed.
            self.key(context, "u", Modifiers::default())?;
            self.key(context, "=", primary())?;
            self.stage = 1;
            return Ok(());
        }
        let ratio = match self.stage {
            1 => 1.25_f64.powi(2), // press + repeat
            2 | 4 => 1.0,
            3 | 6 => 17.75 / 16.0,
            5 => 1.25_f64.sqrt(),
            7 => 20.25 / 16.0,
            8 | 9 => (20.25 / 16.0) * (18.0 / 16.0),
            10 => 17.75 / 16.0,
            11 | 12 => 18.0 / 16.0,
            13..=15 => 1.0,
            _ => return Err(fail("invalid harness stage".into())),
        };
        let baseline = self.baseline.unwrap();
        let expected_spacing = if (8..=12).contains(&self.stage) {
            1.25 * ratio * 16.0 / 18.0
        } else {
            0.5 * ratio
        };
        let expected = [
            f64::from(baseline[0]) * ratio + self.dpi.unwrap() * (expected_spacing - 0.5 * ratio),
            f64::from(baseline[1]) * ratio,
        ];
        let matches = if matches!(self.stage, 13 | 14) {
            let cells = u64::from(commit.viewport.columns) * u64::from(commit.viewport.rows);
            if cells > 100_000 {
                return Err(fail(format!(
                    "tiny zoom committed {cells} cells, above capacity"
                )));
            }
            // Require actual tiny geometry, not the pre-request reset frame.
            commit
                .cell
                .iter()
                .zip(baseline)
                .all(|(actual, base)| *actual < base * 0.9)
        } else {
            commit
                .cell
                .iter()
                .zip(expected)
                .all(|(actual, expected)| (f64::from(*actual) - expected).abs() < 0.03)
        };
        if self.stage == 13 {
            if self.tiny_seen != commit.count {
                self.tiny_seen = commit.count;
                self.tiny_since = Instant::now();
                return Ok(());
            }
            if self.tiny_since.elapsed() < Duration::from_millis(500) {
                return Ok(());
            }
        }
        if self.stage == 14
            && (commit.count != self.before || commit.viewport.generation != self.reload_generation)
        {
            return Err(fail(
                "capacity-clamped tiny zoom kept repainting while idle".into(),
            ));
        }
        // Invalid reload must leave usable committed output untouched; unlike
        // successful visual changes it must not require a new commit.
        if self.stage == 9 {
            let state = self.state.0.lock().unwrap();
            if state.reloads_taken < 2 {
                return Ok(());
            }
            if commit.viewport.generation != self.reload_generation {
                return Err(fail(
                    "invalid reload changed committed viewport generation".into(),
                ));
            }
        }
        if self.stage == 8 && commit.viewport.generation <= self.reload_generation {
            return Ok(());
        }
        // Allow animation to settle and require actual new successful output.
        if !matches
            || (!matches!(self.stage, 9 | 14) && commit.count <= self.before)
            || self.stage_started.elapsed() < Duration::from_millis(250)
        {
            return Ok(());
        }
        if forwarded != 3 {
            return Err(fail(format!(
                "zoom leaked input: worker received {forwarded} events, expected 3"
            )));
        }
        println!(
            "stage={} commits={} viewport={} cell={:?} ratio={ratio:.6}",
            self.stage, commit.count, commit.viewport.generation, commit.cell
        );
        self.before = commit.count;
        self.stage_started = Instant::now();
        match self.stage {
            1 => self.key(context, "-", primary())?,
            2 => self.key(context, "s", primary())?,
            3 => self.key(context, "0", primary())?,
            4 => self.inject(
                context,
                InputEvent::Scroll {
                    delta: ScrollDelta::Pixels { x: 0.0, y: 20.0 },
                    modifiers: primary(),
                },
            )?,
            5 => self.key(context, "s", primary())?,
            6 => {
                // Multiple targets while 40ms preparation is in flight. Only a
                // coalesced pending viewport is retained by the worker.
                self.key(context, "=", primary())?;
                self.key(context, "-", primary())?;
                self.key(context, "f", primary())?;
            }
            7 => {
                let mut native = self.policy.clone();
                native.typography.size = 18.0;
                native.typography.letter_spacing = 1.25;
                self.policy = native.clone();
                self.reload_generation = commit.viewport.generation;
                self.state.0.lock().unwrap().reload = Some(native);
                self.application.event(context, PlatformEvent::Wake)?;
            }
            8 => {
                let mut invalid = self.policy.clone();
                invalid.typography.size = f32::NAN;
                self.reload_generation = commit.viewport.generation;
                self.state.0.lock().unwrap().reload = Some(invalid);
                self.application.event(context, PlatformEvent::Wake)?;
            }
            9 => self.key(context, "s", primary())?,
            10 => self.key(context, "0", primary())?,
            11 => {
                let (lock, changed) = &*self.state;
                let mut state = lock.lock().unwrap();
                state.inject_failure = true;
                state.pending = state.latest;
                changed.notify_one();
            }
            12 => {
                if self.state.0.lock().unwrap().injected_rejections != 1 {
                    return Err(fail("missing injected output rejection/recovery".into()));
                }
                let mut native = self.policy.clone();
                native.typography.size = 16.0;
                native.typography.letter_spacing = 0.5;
                native.zoom.min_scale = 0.01;
                native.zoom.bindings.push(ZoomBinding {
                    key: "t".into(),
                    modifiers: vec![ZoomModifier::Primary],
                    action: ZoomAction::SetScale(0.01),
                });
                self.policy = native.clone();
                self.state.0.lock().unwrap().reload = Some(native);
                self.application.event(context, PlatformEvent::Wake)?;
                self.key(context, "t", primary())?;
            }
            13 => {
                self.reload_generation = commit.viewport.generation;
                println!(
                    "tiny-capacity committed cells={} generation={}",
                    u64::from(commit.viewport.columns) * u64::from(commit.viewport.rows),
                    commit.viewport.generation
                );
            }
            14 => self.key(context, "0", primary())?,
            15 => {
                self.application
                    .event(context, PlatformEvent::CloseRequested)?;
                self.passed.store(true, Ordering::Release);
                let state = self.state.0.lock().unwrap();
                println!(
                    "PASS native Application/GPU acceptance: attempts={} accepted={} rejected={} invalidations={} committed={}",
                    state.attempts,
                    state.accepted,
                    state.rejected,
                    state.invalidations,
                    commit.count
                );
                drop(state);
                return Ok(());
            }
            _ => unreachable!(),
        }
        self.stage += 1;
        Ok(())
    }
}

impl WindowApplication for Harness {
    fn ready_to_show(&self) -> bool {
        self.application.ready_to_show()
    }
    fn event(
        &mut self,
        context: &mut WindowContext<'_>,
        event: PlatformEvent,
    ) -> Result<(), PlatformError> {
        if matches!(event, PlatformEvent::CloseRequested) {
            return Err(PlatformError::Application(
                "window closed before acceptance completed".into(),
            ));
        }
        // Do not let real user input contaminate the controlled synthetic test.
        if !matches!(
            event,
            PlatformEvent::Input(_) | PlatformEvent::PasteRequested
        ) {
            self.application.event(context, event)?;
        }
        self.drive(context)?;
        if !self.passed.load(Ordering::Acquire) {
            context.request_redraw_at(Instant::now() + Duration::from_millis(10));
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    ensure!(
        std::env::args().skip(1).collect::<Vec<_>>() == ["--window"],
        "opt in explicitly: zoom_window_acceptance --window (opens an owned native window)"
    );
    let passed = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let watchdog_done = Arc::clone(&finished);
    // Bounds even an OS/GPU callback or receipt shutdown that stops servicing
    // event-loop timers. This standalone process owns no external resources.
    let _watchdog = thread::Builder::new()
        .name("zoom-acceptance-watchdog".into())
        .spawn(move || {
            let deadline = Instant::now() + WATCHDOG_LIMIT;
            while Instant::now() < deadline {
                if watchdog_done.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
            eprintln!("FAIL: native acceptance process exceeded 65s watchdog");
            std::process::exit(1);
        })?;
    let state: State = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
    let mut native = NativeConfiguration::default();
    native.zoom.remember = false;
    native.zoom.step = 1.25;
    native.zoom.wheel.sensitivity = 1.0;
    // Nonzero spacing detects font-only rather than whole-window scaling.
    native.typography.letter_spacing = 0.5;
    for (key, size) in [("s", 17.75), ("f", 20.25)] {
        native.zoom.bindings.push(ZoomBinding {
            key: key.into(),
            modifiers: vec![ZoomModifier::Primary],
            action: ZoomAction::SetSize(size),
        });
    }
    let policy = native.clone();
    let application = Application::configured(
        Worker {
            state: Arc::clone(&state),
            producer: None,
        },
        native,
        None,
    )?;
    let now = Instant::now();
    let result = bterm_native_platform::run(
        WindowOptions {
            title: "Bterm isolated zoom acceptance (finite)".into(),
            ..WindowOptions::default()
        },
        Harness {
            application,
            state,
            passed: Arc::clone(&passed),
            started: now,
            stage_started: now,
            stage: 0,
            baseline: None,
            dpi: None,
            before: 0,
            policy,
            reload_generation: 0,
            tiny_seen: 0,
            tiny_since: now,
        },
    );
    finished.store(true, Ordering::Release);
    result?;
    ensure!(
        passed.load(Ordering::Acquire),
        "event loop ended before acceptance passed"
    );
    Ok(())
}
