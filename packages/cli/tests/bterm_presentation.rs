//! End-to-end retained preparation → direct output acceptance regressions.
//!
//! No browser frame, ANSI parser, terminal writer, display server, or alternate
//! hit map participates. The output callback receives the canonical owned scene.
use std::sync::Arc;

use bmux_frontend_runtime::presentation::{
    DeliveryError, DeliveryRejection, PreparedDelivery, PresentationAuthority, PresentationIdentity,
};
use bmux_tui::{
    damage::Damage,
    geometry::{Point, Rect},
    hit::HitRegion,
    presentation::PresentationState,
    style::Style,
};
use uuid::Uuid;

const AREA: Rect = Rect::new(0, 0, 16, 6);

fn identity(revision: u64) -> PresentationIdentity {
    PresentationIdentity {
        epoch: Uuid::from_u128(10),
        attached_id: Uuid::from_u128(20),
        viewport_generation: 1,
        revision,
    }
}

fn authority() -> PresentationAuthority {
    let id = identity(1);
    PresentationAuthority::new(id.epoch, id.attached_id, id.viewport_generation, AREA)
}

fn delivery(revision: u64, label: &str, hit: Rect) -> PreparedDelivery {
    let prepared = PresentationState::new(AREA)
        .prepare(Damage::Full, |paint| {
            paint.set_cell(1, 1, label, Style::default());
            paint.set_cursor_local(hit.x, hit.y, true);
            paint.push_hit(HitRegion::new(label, hit).focusable(true));
        })
        .expect("full retained scene");
    PreparedDelivery {
        identity: identity(revision),
        prepared: Arc::new(prepared),
    }
}

#[test]
fn failed_output_never_exposes_new_hit_geometry_and_retry_uses_identical_scene() {
    let mut authority = authority();
    let first = delivery(1, "a", Rect::new(1, 1, 2, 2));
    authority.stage(&first).unwrap();
    authority
        .present(first.clone(), |_| Ok::<_, ()>(()))
        .unwrap();
    let next = delivery(2, "b", Rect::new(8, 3, 2, 2));
    authority.stage(&next).unwrap();
    let result = authority.present(next.clone(), |output| {
        assert!(Arc::ptr_eq(&output.prepared, &next.prepared));
        assert!(output.prepared.hits().hit_test(Point::new(8, 3)).is_some());
        Err::<(), _>("surface lost")
    });
    assert_eq!(result, Err(DeliveryError::Output("surface lost")));
    let committed = authority.committed().unwrap();
    assert!(Arc::ptr_eq(&committed.prepared, &first.prepared));
    assert_eq!(
        committed.prepared.cursor().unwrap().position,
        Point::new(1, 1)
    );
    assert_eq!(committed.prepared.focus().active().unwrap().as_str(), "a");
    assert!(
        committed
            .prepared
            .hits()
            .hit_test(Point::new(1, 1))
            .is_some()
    );
    assert!(
        committed
            .prepared
            .hits()
            .hit_test(Point::new(8, 3))
            .is_none()
    );
    authority
        .present(next.clone(), |_| Ok::<_, ()>(()))
        .unwrap();
    let committed = authority.committed().unwrap();
    assert!(Arc::ptr_eq(&committed.prepared, &next.prepared));
    assert_eq!(
        committed.prepared.cursor().unwrap().position,
        Point::new(8, 3)
    );
    assert_eq!(committed.prepared.focus().active().unwrap().as_str(), "b");
    assert!(
        committed
            .prepared
            .hits()
            .hit_test(Point::new(1, 1))
            .is_none()
    );
    assert!(
        committed
            .prepared
            .hits()
            .hit_test(Point::new(8, 3))
            .is_some()
    );
}

#[test]
fn same_identity_cannot_substitute_different_pixels_or_interaction_metadata() {
    let mut authority = authority();
    let staged = delivery(1, "a", Rect::new(1, 1, 2, 2));
    let replacement = delivery(1, "b", Rect::new(8, 3, 2, 2));
    assert_eq!(staged.identity, replacement.identity);
    authority.stage(&staged).unwrap();
    assert_eq!(
        authority.present(replacement, |_| -> Result<(), ()> {
            panic!("unstaged scene must never reach the backend")
        }),
        Err(DeliveryError::Rejected(DeliveryRejection::Superseded))
    );
    assert!(authority.committed().is_none());
    authority.present(staged, |_| Ok::<_, ()>(())).unwrap();
}

#[test]
fn reconnect_epoch_fences_visible_and_pending_output_even_when_revision_restarts() {
    let mut authority = authority();
    let visible = delivery(8, "a", Rect::new(1, 1, 2, 2));
    authority.stage(&visible).unwrap();
    authority
        .present(visible.clone(), |_| Ok::<_, ()>(()))
        .unwrap();
    let pending = delivery(9, "b", Rect::new(8, 3, 2, 2));
    authority.stage(&pending).unwrap();
    let new_epoch = Uuid::from_u128(99);
    authority.rebind(new_epoch, visible.identity.attached_id, 1, AREA);
    assert!(
        authority.committed().is_none(),
        "old pixels are not input authority"
    );
    assert_eq!(
        authority.present(pending, |_| -> Result<(), ()> {
            panic!("old epoch output")
        }),
        Err(DeliveryError::Rejected(DeliveryRejection::Identity))
    );
    let mut repaired = delivery(1, "c", Rect::new(5, 2, 2, 2));
    repaired.identity.epoch = new_epoch;
    authority.stage(&repaired).unwrap();
    authority
        .present(repaired.clone(), |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(authority.committed().unwrap().identity, repaired.identity);
    assert_eq!(authority.stage(&visible), Err(DeliveryRejection::Identity));
}

#[test]
fn viewport_geometry_mismatch_cannot_advance_output_or_input_authority() {
    let mut authority = authority();
    let mut wrong = delivery(1, "a", Rect::new(1, 1, 2, 2));
    wrong.prepared = Arc::new(
        PresentationState::new(Rect::new(0, 0, AREA.width + 1, AREA.height))
            .prepare(Damage::Full, |_| {})
            .unwrap(),
    );
    assert_eq!(authority.stage(&wrong), Err(DeliveryRejection::Viewport));
    assert_eq!(
        authority.present(wrong, |_| -> Result<(), ()> {
            panic!("wrong viewport output")
        }),
        Err(DeliveryError::Rejected(DeliveryRejection::Viewport))
    );
    assert!(authority.committed().is_none());
}

#[test]
fn direct_output_keeps_compositor_clipping_for_visible_and_offscreen_hits() {
    let mut authority = authority();
    let prepared = PresentationState::new(AREA)
        .prepare(Damage::Full, |paint| {
            paint.push_hit(HitRegion::new("partial", Rect::new(14, 4, 8, 8)));
            paint.push_hit(HitRegion::new("offscreen", Rect::new(20, 8, 2, 2)));
        })
        .unwrap();
    let candidate = PreparedDelivery {
        identity: identity(1),
        prepared: Arc::new(prepared),
    };
    authority.stage(&candidate).unwrap();
    authority
        .present(candidate.clone(), |output| {
            assert_eq!(output.prepared.hits().regions().len(), 1);
            assert_eq!(
                output.prepared.hits().regions()[0].area,
                Rect::new(14, 4, 2, 2)
            );
            Ok::<_, ()>(())
        })
        .unwrap();
    let committed = authority.committed().unwrap();
    assert!(Arc::ptr_eq(&candidate.prepared, &committed.prepared));
    assert!(
        committed
            .prepared
            .hits()
            .hit_test(Point::new(15, 5))
            .is_some()
    );
    assert!(
        committed
            .prepared
            .hits()
            .hit_test(Point::new(16, 5))
            .is_none()
    );
}

/// CPU-only baseline, deliberately ignored in ordinary correctness runs.
/// Measures actual retained preparation/direct acceptance, not GPU submission
/// or manual native UX. Emits samples without inventing hardware-independent SLOs.
#[test]
#[ignore = "run via perf/bterm-direct-presentation.sh for CPU baseline samples"]
fn bterm_direct_presentation_perf() {
    for (scenario, width, height, fail_every) in [
        ("small", 80_u16, 24_u16, 0_u64),
        ("large", 240, 80, 0),
        ("output-failure", 120, 40, 3),
    ] {
        let area = Rect::new(0, 0, width, height);
        let id = identity(1);
        let mut authority = PresentationAuthority::new(id.epoch, id.attached_id, 1, area);
        let state = PresentationState::new(area);
        let mut samples = Vec::new();
        let mut failures = 0;
        for revision in 1..=220_u64 {
            let start = std::time::Instant::now();
            let prepared = state
                .prepare(Damage::Full, |paint| {
                    for y in 0..height {
                        paint.set_cell(0, i64::from(y), "界", Style::default());
                        paint.push_hit(HitRegion::new(
                            format!("row-{y}"),
                            Rect::new(0, y, width, 1),
                        ));
                    }
                })
                .unwrap();
            let candidate = PreparedDelivery {
                identity: PresentationIdentity { revision, ..id },
                prepared: Arc::new(prepared),
            };
            authority.stage(&candidate).unwrap();
            let old = authority.committed().map(|output| output.identity);
            let fail = fail_every != 0 && revision % fail_every == 0;
            let result = authority.present(candidate, |_| if fail { Err(()) } else { Ok(()) });
            if fail {
                failures += 1;
                assert!(result.is_err());
                assert_eq!(authority.committed().map(|output| output.identity), old);
            } else {
                result.unwrap();
            }
            if revision > 20 {
                samples.push(start.elapsed().as_nanos());
            }
        }
        samples.sort_unstable();
        println!(
            "{}",
            serde_json::json!({
                "scenario": scenario, "measurement": "cpu_retained_prepare_direct_accept",
                "samples": samples.len(), "p50_ns": samples[samples.len() / 2],
                "p99_ns": samples[samples.len() * 99 / 100], "output_failures": failures,
                "native_window_verified": false,
            })
        );
    }
}
