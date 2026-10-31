//! Native-host reconciliation of positioned output after a source/layout transition.
//! Geometry is derived only from displayed fragments. It never authorizes a resize:
//! the retained target and existing CLI reducer/service still validate every request.
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use bmux_cli::attach::{
    NativePaneZoomRequestReceipt, NativePaneZoomRequestStatus, NativePaneZoomTarget,
};
use bmux_frontend_runtime::presentation::PresentationIdentity;
use bmux_tui::{grid_fragment::GridContentLayout, presentation::PreparedPresentation};
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(2);
const MAX_ATTEMPTS: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Geometry {
    layout: GridContentLayout,
    viewport: [u16; 4],
    scale: u16,
}
struct Sample {
    geometry: Geometry,
    size: [u16; 2],
    point: [f64; 2],
}
struct Attempt {
    execution: Uuid,
    geometry: Geometry,
    count: u8,
    receipt: Option<NativePaneZoomRequestReceipt>,
    started: Instant,
    settled: bool,
}

#[derive(Default)]
pub(crate) struct PositionedZoomSizing {
    identity: Option<PresentationIdentity>,
    samples: BTreeMap<Uuid, Sample>,
    attempts: BTreeMap<Uuid, Attempt>,
}
impl PositionedZoomSizing {
    pub(crate) fn observe(
        &mut self,
        identity: PresentationIdentity,
        prepared: &PreparedPresentation,
    ) {
        if self.identity.is_some_and(|old| {
            old.epoch != identity.epoch || old.attached_id != identity.attached_id
        }) {
            self.attempts.clear();
        }
        self.identity = Some(identity);
        self.samples.clear();
        for fragment in prepared.grid_fragments().iter().take(4096) {
            let Some(id) = fragment
                .id()
                .strip_prefix("pane-zoom:")
                .and_then(|id| Uuid::parse_str(id).ok())
            else {
                continue;
            };
            let Some(viewport) = fragment.viewport() else {
                continue;
            };
            let Some(clip) = fragment.visible_clips().first() else {
                continue;
            };
            let scale = fragment.transform().scale_millis();
            self.samples.insert(
                id,
                Sample {
                    geometry: Geometry {
                        layout: fragment.content_layout(),
                        viewport: [viewport.x, viewport.y, viewport.width, viewport.height],
                        scale,
                    },
                    size: [
                        fragment.buffer().area().width,
                        fragment.buffer().area().height,
                    ],
                    point: [f64::from(clip.x) + 0.5, f64::from(clip.y) + 0.5],
                },
            );
        }
        self.attempts.retain(|id, attempt| {
            self.samples
                .get(id)
                .is_some_and(|sample| sample.geometry == attempt.geometry)
        });
    }

    /// At most three dequeues per changed source/layout policy; no perpetual
    /// dimension enforcement against another attachment's last-writer sizing.
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        mut target_at: impl FnMut([f64; 2]) -> Option<NativePaneZoomTarget>,
        mut send: impl FnMut(NativePaneZoomTarget) -> anyhow::Result<NativePaneZoomRequestReceipt>,
    ) -> bool {
        let mut pending = false;
        for (id, sample) in &self.samples {
            let Some(target) = target_at(sample.point)
                .filter(|target| Some(target.identity) == self.identity && target.view_id == *id)
            else {
                continue;
            };
            let geometry = sample.geometry;
            if geometry.layout == GridContentLayout::Flow {
                self.attempts.remove(id);
                continue;
            }
            let attempt = self.attempts.entry(*id).or_insert_with(|| Attempt {
                execution: target.execution_id,
                geometry,
                count: 0,
                receipt: None,
                started: now,
                settled: false,
            });
            if attempt.execution != target.execution_id || attempt.geometry != geometry {
                *attempt = Attempt {
                    execution: target.execution_id,
                    geometry,
                    count: 0,
                    receipt: None,
                    started: now,
                    settled: false,
                };
            }
            if target.scale_millis != geometry.scale || attempt.settled {
                continue;
            }
            let expected = [
                (u32::from(geometry.viewport[2]) * 1000 / u32::from(geometry.scale)).clamp(1, 1000),
                (u32::from(geometry.viewport[3]) * 1000 / u32::from(geometry.scale)).clamp(1, 1000),
            ];
            if expected == sample.size.map(u32::from) {
                attempt.settled = true;
                continue;
            }
            if let Some(receipt) = &attempt.receipt {
                match receipt.status() {
                    NativePaneZoomRequestStatus::Applied
                        if now.saturating_duration_since(attempt.started) < WAIT =>
                    {
                        // Reducer admission is not output convergence. Wait for
                        // the exact dimensions above without replaying the resize.
                        pending = true;
                        continue;
                    }
                    NativePaneZoomRequestStatus::Applied => {
                        attempt.settled = true;
                        eprintln!(
                            "bterm: admitted pane sizing did not converge in displayed output"
                        );
                        continue;
                    }
                    NativePaneZoomRequestStatus::Pending
                        if now.saturating_duration_since(attempt.started) < WAIT =>
                    {
                        pending = true;
                        continue;
                    }
                    NativePaneZoomRequestStatus::Pending => {
                        attempt.settled = true;
                        eprintln!(
                            "bterm: positioned pane sizing timed out; no unacknowledged resize will be replayed"
                        );
                        continue;
                    }
                    NativePaneZoomRequestStatus::Rejected => {
                        attempt.receipt = None;
                    }
                }
            }
            if attempt.count >= MAX_ATTEMPTS {
                attempt.settled = true;
                eprintln!("bterm: positioned pane sizing rejected; change zoom or layout to retry");
                continue;
            }
            attempt.count += 1;
            attempt.started = now;
            match send(target) {
                Ok(receipt) => attempt.receipt = Some(receipt),
                Err(error) => eprintln!("bterm: positioned pane sizing deferred: {error}"),
            }
            pending = true;
        }
        pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_cli::attach::NativeAttachHost;
    use bmux_tui::{
        buffer::Buffer,
        damage::Damage,
        geometry::{Point, Rect},
        grid_fragment::{GridTransform, PreparedGridFragment},
        presentation::PresentationState,
    };
    use std::sync::Arc;

    fn target() -> NativePaneZoomTarget {
        NativePaneZoomTarget {
            identity: PresentationIdentity {
                epoch: Uuid::new_v4(),
                revision: 1,
                attached_id: Uuid::new_v4(),
                viewport_generation: 1,
            },
            view_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            source_revision: 1,
            scale_millis: 750,
            max_scale_millis: 8000,
        }
    }
    fn output(
        target: NativePaneZoomTarget,
        layout: GridContentLayout,
        size: [u16; 2],
    ) -> PreparedPresentation {
        let viewport = Rect::new(0, 0, 99, 24);
        let fragment = PreparedGridFragment::new(
            format!("pane-zoom:{}", target.view_id),
            Arc::new(Buffer::empty(Rect::new(0, 0, size[0], size[1]))),
            GridTransform::new(Point::new(0, 0), target.scale_millis).unwrap(),
            vec![viewport],
        )
        .unwrap()
        .with_viewport(viewport)
        .unwrap()
        .with_content_layout(layout);
        let mut prepared = PresentationState::new(viewport)
            .prepare(Damage::Full, |_| {})
            .unwrap();
        prepared
            .set_grid_fragments(
                vec![fragment],
                bmux_tui::presentation::PresentationLimits {
                    max_cells: 100_000,
                    max_entries: 4096,
                    max_bytes: 1_000_000,
                },
            )
            .unwrap();
        prepared
    }
    #[test]
    fn zoom_before_launch_transition_reentry_and_rejected_requests_are_bounded() {
        let mut sizing = PositionedZoomSizing::default();
        let target = target();
        let now = Instant::now();
        let mut sends = 0;
        let mut send = |got: NativePaneZoomTarget| {
            assert_eq!(got, target);
            sends += 1;
            anyhow::bail!("injected rejection")
        };
        sizing.observe(
            target.identity,
            &output(target, GridContentLayout::Flow, [132, 32]),
        );
        assert!(!sizing.tick(now, |_| Some(target), &mut send));
        sizing.observe(
            target.identity,
            &output(target, GridContentLayout::Positioned, [99, 24]),
        );
        for _ in 0..10 {
            sizing.tick(now, |_| Some(target), &mut send);
        }
        assert_eq!(sends, 3);
        sizing.observe(
            target.identity,
            &output(target, GridContentLayout::Flow, [132, 32]),
        );
        sizing.observe(
            target.identity,
            &output(target, GridContentLayout::Positioned, [99, 24]),
        );
        let mut retried = false;
        assert!(sizing.tick(
            now,
            |_| Some(target),
            |_| {
                retried = true;
                anyhow::bail!("reentry");
            }
        ));
        assert!(retried);
    }
    #[test]
    fn pending_timeout_does_not_replay_and_matching_output_stops_idle_work() {
        let mut sizing = PositionedZoomSizing::default();
        let target = target();
        let now = Instant::now();
        let (handle, _host) = NativeAttachHost::channel(99, 24, 1, |_| Ok(())).unwrap();
        sizing.observe(
            target.identity,
            &output(target, GridContentLayout::Positioned, [99, 24]),
        );
        assert!(sizing.tick(
            now,
            |_| Some(target),
            |target| handle.try_set_pane_zoom(target, target.scale_millis)
        ));
        assert!(sizing.tick(
            now + WAIT / 2,
            |_| Some(target),
            |_| panic!("pending replay")
        ));
        assert!(!sizing.tick(now + WAIT, |_| Some(target), |_| panic!("timed-out replay")));
        sizing.observe(
            target.identity,
            &output(target, GridContentLayout::Positioned, [132, 32]),
        );
        for _ in 0..20 {
            assert!(!sizing.tick(now, |_| Some(target), |_| panic!("idle resize")));
        }
    }
    #[test]
    fn only_exact_committed_target_can_trigger_sizing_and_success_is_not_enforced_forever() {
        let mut sizing = PositionedZoomSizing::default();
        let target = target();
        let now = Instant::now();
        sizing.observe(
            target.identity,
            &output(target, GridContentLayout::Positioned, [99, 24]),
        );
        let mut stale = target;
        stale.identity.revision += 1;
        assert!(!sizing.tick(now, |_| Some(stale), |_| panic!("stale output")));
        sizing.observe(
            target.identity,
            &output(target, GridContentLayout::Positioned, [132, 32]),
        );
        assert!(!sizing.tick(now, |_| Some(target), |_| panic!("already sized")));
        sizing.observe(
            target.identity,
            &output(target, GridContentLayout::Positioned, [99, 24]),
        );
        assert!(!sizing.tick(now, |_| Some(target), |_| panic!("fighting another view")));
        let mut changed = target;
        changed.scale_millis = 500;
        sizing.observe(
            changed.identity,
            &output(changed, GridContentLayout::Positioned, [99, 24]),
        );
        assert!(sizing.tick(
            now,
            |_| Some(changed),
            |_| anyhow::bail!("new geometry is eligible")
        ));
    }
}
