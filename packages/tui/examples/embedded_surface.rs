//! Caller-owned live dashboard, outside slides/presenter/playbooks.
//!
//! Run `--headless --frames 6` for finite acceptance, or `--terminal --frames 60`
//! for ANSI output using the ordinary Terminal retained presenter primitive.
//! This is a neutral dynamic sensor demonstration, not a BMUX PTY/session.
use std::{
    io::{self, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use bmux_tui::{
    component::{Component, Constraints, LayoutCx, LayoutNode},
    damage::Damage,
    embedded_surface::{EmbeddedSurface, RetainedSurface, SurfaceIdentity, SurfaceLimits},
    geometry::{Rect, Size},
    paint::LocalRect,
    presentation::{PresentationLimits, PresentationState},
    style::Style,
    terminal::{DrawStats, Terminal},
    text::Line,
};

const SOURCE: Rect = Rect::new(0, 0, 28, 3);
const VIEW: Rect = Rect::new(0, 0, 34, 7);

#[derive(Default)]
struct Slot {
    latest: Option<RetainedSurface>,
    published: u64,
    coalesced: u64,
    cancelled: bool,
    error: Option<String>,
}

/// One complete replaceable snapshot, never a partial delta or unbounded queue.
/// Superseded snapshots are explicitly counted; the next read repairs with full
/// cells and a reconciled image scene. No paint callback runs on this thread.
pub(crate) struct Producer {
    shared: Arc<(Mutex<Slot>, Condvar)>,
    worker: Option<JoinHandle<()>>,
    joined: Arc<AtomicBool>,
}

impl Producer {
    pub(crate) fn start(period: Duration) -> Self {
        assert!(!period.is_zero());
        let shared = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
        let worker_state = Arc::clone(&shared);
        let worker = thread::spawn(move || {
            let started = Instant::now();
            let source = PresentationState::new(SOURCE);
            let mut revision = 0;
            loop {
                revision += 1;
                // Sampling belongs to the producer clock, not the consumer's
                // frame loop. This demo reports actual elapsed acquisition time.
                let elapsed = started.elapsed().as_millis();
                let sample = source
                    .prepare_bounded(
                        PresentationLimits {
                            max_cells: 84,
                            max_entries: 16,
                            max_bytes: 8192,
                        },
                        Damage::Full,
                        |cx| {
                            cx.write_line(
                                LocalRect::new(0, 0, 28, 1),
                                &Line::raw("Live acquisition clock"),
                            );
                            cx.write_line(
                                LocalRect::new(0, 1, 28, 1),
                                &Line::raw(format!("sample {revision}")),
                            );
                            cx.write_line(
                                LocalRect::new(0, 2, 28, 1),
                                &Line::raw(format!("elapsed {elapsed} ms")),
                            );
                        },
                    )
                    .map_err(|error| format!("source admission: {error:?}"))
                    .and_then(|prepared| {
                        let prepared = prepared.ok_or_else(|| "empty source output".to_owned())?;
                        RetainedSurface::new(
                            SurfaceIdentity {
                                owner: "sensor/acquisition-clock".into(),
                                generation: 1,
                                revision,
                            },
                            Arc::new(prepared.buffer().clone()),
                            Arc::new(prepared.image_scene().clone()),
                            SurfaceLimits {
                                max_cells: 84,
                                max_bytes: 8192,
                            },
                        )
                        .map_err(|error| format!("snapshot admission: {error:?}"))
                    });
                let (lock, wake) = &*worker_state;
                let mut slot = lock.lock().expect("producer slot poisoned");
                if slot.cancelled {
                    break;
                }
                match sample {
                    Ok(sample) => {
                        if slot.latest.replace(sample).is_some() {
                            slot.coalesced += 1;
                        }
                        slot.published = revision;
                    }
                    Err(error) => {
                        slot.error = Some(error);
                        wake.notify_all();
                        break;
                    }
                }
                wake.notify_all();
                // Cancellation wakes the sleeper; joining never waits for an
                // entire sample interval. Spurious wakes do not speed sampling.
                let (slot, _) = wake
                    .wait_timeout_while(slot, period, |slot| !slot.cancelled)
                    .expect("producer slot poisoned");
                if slot.cancelled {
                    break;
                }
            }
        });
        Self {
            shared,
            worker: Some(worker),
            joined: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn latest_at_least(&self, revision: u64) -> Result<RetainedSurface, String> {
        let (lock, wake) = &*self.shared;
        let slot = lock.lock().map_err(|_| "producer slot poisoned")?;
        let (mut slot, _) = wake
            .wait_timeout_while(slot, Duration::from_secs(3), |slot| {
                !slot.cancelled
                    && slot.error.is_none()
                    && slot
                        .latest
                        .as_ref()
                        .is_none_or(|s| s.identity().revision < revision)
            })
            .map_err(|_| "producer slot poisoned")?;
        if let Some(error) = &slot.error {
            return Err(error.clone());
        }
        if slot.cancelled {
            return Err("producer cancelled".into());
        }
        if slot
            .latest
            .as_ref()
            .is_none_or(|s| s.identity().revision < revision)
        {
            return Err("producer deadline exceeded".into());
        }
        slot.latest
            .take()
            .ok_or_else(|| "missing full snapshot".into())
    }

    pub(crate) fn counts(&self) -> (u64, u64) {
        let slot = self.shared.0.lock().expect("producer slot poisoned");
        (slot.published, slot.coalesced)
    }

    #[cfg(test)]
    pub(crate) fn cleanup_probe(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.joined)
    }

    pub(crate) fn stop(&mut self) -> Result<(), String> {
        {
            // Even a poisoned slot must not detach caller-owned work.
            let mut slot = self
                .shared
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.cancelled = true;
            slot.latest = None;
            self.shared.1.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            self.joined.store(true, Ordering::SeqCst);
            result.map_err(|_| "producer panicked")?;
        }
        Ok(())
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        // All ordinary exits, including output errors, cancel and join owned work.
        let _ = self.stop();
    }
}

pub(crate) struct Dashboard<W> {
    terminal: Terminal<W>,
    committed: Option<(RetainedSurface, LayoutNode)>,
}

impl<W: Write> Dashboard<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            terminal: Terminal::new(writer, VIEW),
            committed: None,
        }
    }

    pub(crate) fn present(&mut self, surface: &RetainedSurface) -> io::Result<DrawStats> {
        if let Some((current, _)) = &self.committed {
            let old = current.identity();
            let new = surface.identity();
            if old.owner != new.owner
                || old.generation != new.generation
                || new.revision <= old.revision
            {
                return Err(io::Error::other("stale or foreign snapshot"));
            }
        }
        let component = EmbeddedSurface::new(surface);
        let layout = component.layout(
            Constraints::tight(Size::new(VIEW.width, VIEW.height)),
            &mut LayoutCx::default(),
        );
        let report = self.terminal.draw(|cx| {
            cx.fill(
                LocalRect::new(0, 0, VIEW.width, VIEW.height),
                " ",
                Style::new(),
            );
            cx.write_line(
                LocalRect::new(0, 0, VIEW.width, 1),
                &Line::raw("Caller-owned live dashboard"),
            );
            component.paint(&layout, cx);
        })?;
        // Geometry and revision are published only after the Terminal flush
        // commits. A failed flush leaves the old identity/route authoritative.
        self.committed = Some((surface.clone(), layout));
        Ok(report)
    }

    pub(crate) fn committed_identity(&self) -> Option<&SurfaceIdentity> {
        self.committed
            .as_ref()
            .map(|(surface, _)| surface.identity())
    }

    pub(crate) fn accepts_input(&self, identity: &SurfaceIdentity) -> bool {
        let Some((surface, layout)) = &self.committed else {
            return false;
        };
        // Input/delayed callbacks must be tagged with exactly the displayed
        // revision; no producer callback can overwrite consumer commit state.
        EmbeddedSurface::new(surface)
            .input_point(layout, 3, 2)
            .is_some_and(|point| &point.identity == identity)
    }
}

pub(crate) fn run<W: Write>(writer: W, frames: u64) -> Result<(u64, u64), String> {
    let mut producer = Producer::start(Duration::from_millis(5));
    let mut dashboard = Dashboard::new(writer);
    let mut next = 5;
    for _ in 0..frames {
        // Deliberately slow reader: independent sampling keeps running and
        // replaces the one retained pending snapshot while output is idle.
        thread::sleep(Duration::from_millis(35));
        let snapshot = producer.latest_at_least(next)?;
        dashboard
            .present(&snapshot)
            .map_err(|error| format!("dashboard output: {error}"))?;
        let identity = dashboard.committed_identity().ok_or("missing commit")?;
        if !dashboard.accepts_input(identity) {
            return Err("committed input geometry mismatch".into());
        }
        next = identity.revision + 5;
    }
    let counts = producer.counts();
    producer.stop()?;
    if frames > 1 && (counts.0 <= 1 || counts.1 == 0) {
        return Err("live/coalescing acceptance not observed".into());
    }
    Ok(counts)
}

#[cfg(not(test))]
fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .ok_or("usage: embedded_surface --headless|--terminal --frames N")?;
    if !matches!(mode.as_str(), "--headless" | "--terminal")
        || args.next().as_deref() != Some("--frames")
    {
        return Err("usage: embedded_surface --headless|--terminal --frames N".into());
    }
    let frames: u64 = args
        .next()
        .ok_or("missing frame count")?
        .parse()
        .map_err(|_| "invalid frame count")?;
    if !(2..=600).contains(&frames) || args.next().is_some() {
        return Err("frame count must be 2..=600; no extra arguments".into());
    }
    let counts = if mode == "--terminal" {
        run(io::stdout().lock(), frames)?
    } else {
        // Exercise the same ANSI presenter/flush/commit path, without requiring
        // a terminal and without retaining an ever-growing output byte vector.
        run(io::sink(), frames)?
    };
    eprintln!(
        "live dashboard: commits={frames} produced={} coalesced={} cleanup=joined",
        counts.0, counts.1
    );
    Ok(())
}
