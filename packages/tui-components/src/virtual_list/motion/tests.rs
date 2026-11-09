use std::time::{Duration, Instant};

use bmux_tui::style::Easing;

use super::{MotionSample, ScrollMotion, ScrollTarget};
use crate::virtual_list::{
    AnchorOffset, AnchorPolicy, Edge, ListAnchor, ListGeometry, ListPosition, MeasuredListIndex,
    MeasuredListItem, NoSourceMap, Overscroll, PositionControl, ScrollInputSource, SourceMap,
    VirtualListAction, VirtualListState,
};

const SECOND: Duration = Duration::from_secs(1);

/// Index of `count` items keyed `0..count`, each `height` rows, no gap.
fn index(count: u64, height: u64) -> MeasuredListIndex<u64> {
    MeasuredListIndex::from_items(
        0,
        (0..count).map(|key| MeasuredListItem::new(key, 0, 10, 0, height)),
    )
    .expect("unique keys")
}

const fn geometry(viewport: u64) -> ListGeometry {
    ListGeometry::new(viewport)
}

const fn motion(
    from: ListPosition<u64>,
    target: ScrollTarget<u64>,
    easing: Easing,
    duration: Duration,
    started: Instant,
) -> ScrollMotion<u64> {
    ScrollMotion::new(from, target, easing, duration, started)
}

fn top_at(
    motion: &ScrollMotion<u64>,
    elapsed: Duration,
    index: &MeasuredListIndex<u64>,
    viewport: u64,
) -> MotionSample {
    motion
        .sample_elapsed(elapsed, index, geometry(viewport), &NoSourceMap)
        .expect("target resolves")
}

#[test]
fn linear_motion_interpolates_and_lands_exactly_on_the_target() {
    let index = index(100, 2);
    let start = Instant::now();
    let motion = motion(
        ListPosition::Top,
        ScrollTarget::Key(40, AnchorPolicy::ItemTopAtTop),
        Easing::Linear,
        SECOND,
        start,
    );
    assert_eq!(
        top_at(&motion, SECOND / 4, &index, 10),
        MotionSample {
            top_offset: 20,
            done: false
        }
    );
    assert_eq!(top_at(&motion, SECOND / 2, &index, 10).top_offset, 40);
    assert_eq!(
        top_at(&motion, SECOND, &index, 10),
        MotionSample {
            top_offset: 80,
            done: true
        }
    );
    assert_eq!(top_at(&motion, SECOND * 9, &index, 10).top_offset, 80);
    // Instant sampling agrees with elapsed sampling.
    let sampled = motion
        .sample(start + SECOND / 2, &index, geometry(10), &NoSourceMap)
        .expect("resolves");
    assert_eq!(sampled.top_offset, 40);
    let early = motion
        .sample(start, &index, geometry(10), &NoSourceMap)
        .expect("resolves");
    assert_eq!(early.top_offset, 0);
}

#[test]
fn zero_duration_is_complete_immediately() {
    let index = index(10, 3);
    let start = Instant::now();
    let motion = motion(
        ListPosition::Top,
        ScrollTarget::Bottom,
        Easing::EaseInOut,
        Duration::ZERO,
        start,
    );
    assert_eq!(
        top_at(&motion, Duration::ZERO, &index, 6),
        MotionSample {
            top_offset: 24,
            done: true
        }
    );
    assert!(motion.is_done(start));
    assert_eq!(motion.next_frame(start, SECOND / 60), None);
    assert_eq!(motion.deadline(), start);
    assert_eq!(
        motion.position_at(start, &index, geometry(6), &NoSourceMap),
        Some(ListPosition::FollowBottom)
    );
}

#[test]
fn every_easing_moves_monotonically_toward_the_target_in_both_directions() {
    let index = index(1000, 3);
    let start = Instant::now();
    for easing in [
        Easing::Linear,
        Easing::EaseIn,
        Easing::EaseOut,
        Easing::EaseInOut,
    ] {
        for (from, target, downward) in [
            (ListPosition::Top, ScrollTarget::Bottom, true),
            (ListPosition::FollowBottom, ScrollTarget::Top, false),
        ] {
            let motion = motion(from, target, easing, SECOND, start);
            let mut previous = top_at(&motion, Duration::ZERO, &index, 20).top_offset;
            for step in 1..=240 {
                let sample = top_at(&motion, SECOND * step / 240, &index, 20);
                if downward {
                    assert!(sample.top_offset >= previous, "{easing:?} step {step}");
                } else {
                    assert!(sample.top_offset <= previous, "{easing:?} step {step}");
                }
                assert_eq!(sample.done, step == 240);
                previous = sample.top_offset;
            }
            assert_eq!(previous, if downward { 2980 } else { 0 });
        }
    }
}

#[test]
fn eased_progress_is_quantized_deterministically() {
    let index = index(1, 70_000);
    let motion = motion(
        ListPosition::Top,
        ScrollTarget::Anchor(ListAnchor::row(0, 65_536)),
        Easing::EaseIn,
        SECOND,
        Instant::now(),
    );
    // EaseIn at one half is exactly one quarter: 16384/65536 of 65536 rows.
    assert_eq!(top_at(&motion, SECOND / 2, &index, 10).top_offset, 16_384);
}

#[test]
fn keyed_endpoints_follow_content_inserted_during_motion() {
    let mut index = index(50, 2);
    let start = Instant::now();
    let motion = motion(
        ListPosition::Anchored(ListAnchor::top(10)),
        ScrollTarget::Key(30, AnchorPolicy::ItemTopAtTop),
        Easing::Linear,
        SECOND,
        start,
    );
    assert_eq!(top_at(&motion, SECOND / 2, &index, 10).top_offset, 40);
    // Prepending 5 items of 4 rows shifts both endpoints by 20 rows.
    index
        .prepend((100..105).map(|key| MeasuredListItem::new(key, 0, 10, 0, 4)))
        .expect("unique keys");
    assert_eq!(top_at(&motion, SECOND / 2, &index, 10).top_offset, 60);
    assert_eq!(top_at(&motion, SECOND, &index, 10).top_offset, 80);
    // Reflow between the endpoints (an item grows) retargets as well.
    assert!(index.update_height(&20, 12));
    assert_eq!(top_at(&motion, SECOND, &index, 10).top_offset, 90);
    let done = motion
        .position_at(start + SECOND, &index, geometry(10), &NoSourceMap)
        .expect("resolves");
    assert_eq!(done, ListPosition::Anchored(ListAnchor::top(30)));
}

#[test]
fn missing_start_starts_at_target_and_missing_target_stops_sampling() {
    let mut index = index(20, 1);
    let start = Instant::now();
    let motion = motion(
        ListPosition::Anchored(ListAnchor::top(3)),
        ScrollTarget::Key(8, AnchorPolicy::ItemTopAtTop),
        Easing::Linear,
        SECOND,
        start,
    );
    assert!(index.remove(&3).is_some());
    assert_eq!(
        top_at(&motion, SECOND / 2, &index, 4),
        MotionSample {
            top_offset: 7,
            done: false
        }
    );
    assert!(index.remove(&8).is_some());
    assert_eq!(
        motion.sample_elapsed(SECOND / 2, &index, geometry(4), &NoSourceMap),
        None
    );
    assert_eq!(
        motion.position_at(start + SECOND, &index, geometry(4), &NoSourceMap),
        None
    );
}

#[test]
fn ensure_visible_targets_resolve_relative_to_the_sampled_start() {
    let index = index(30, 2);
    let start = Instant::now();
    // Item 6 (rows 12..14) is fully visible from top 10 with 8 rows.
    let still = motion(
        ListPosition::Anchored(ListAnchor::top(5)),
        ScrollTarget::Key(6, AnchorPolicy::EnsureVisibleFrom(Edge::Bottom)),
        Easing::Linear,
        SECOND,
        start,
    );
    assert_eq!(top_at(&still, SECOND / 2, &index, 8).top_offset, 10);
    assert_eq!(top_at(&still, SECOND, &index, 8).top_offset, 10);
    // Item 20 (rows 40..42) is below; bottom-at-bottom lands at 34.
    let reveal = motion(
        ListPosition::Anchored(ListAnchor::top(5)),
        ScrollTarget::Key(20, AnchorPolicy::EnsureVisibleFrom(Edge::Bottom)),
        Easing::Linear,
        SECOND,
        start,
    );
    assert_eq!(top_at(&reveal, SECOND, &index, 8).top_offset, 34);
}

#[test]
fn targets_resolve_and_complete_at_keyed_positions() {
    let index = index(10, 4);
    let plain = geometry(8);
    let overscrolled = geometry(8).overscroll(Overscroll::ViewportMinus(1));
    assert_eq!(
        ScrollTarget::<u64>::Top.resolve_top(&index, 9, plain, &NoSourceMap),
        Some(0)
    );
    assert_eq!(
        ScrollTarget::<u64>::Bottom.resolve_top(&index, 0, overscrolled, &NoSourceMap),
        Some(32)
    );
    // An anchor on the final item lands in overscroll only when permitted.
    let anchor: ListAnchor<u64> = ListAnchor::row(9, 2);
    let target = ScrollTarget::Anchor(anchor.clone());
    assert_eq!(target.resolve_top(&index, 0, plain, &NoSourceMap), Some(32));
    assert_eq!(
        target.final_position(&index, 0, plain, &NoSourceMap),
        Some(ListPosition::Anchored(anchor.clone()))
    );
    assert_eq!(
        target.resolve_top(&index, 0, overscrolled, &NoSourceMap),
        Some(38)
    );
    assert_eq!(
        target.final_position(&index, 0, overscrolled, &NoSourceMap),
        Some(ListPosition::Overscrolled(anchor))
    );
    assert_eq!(
        ScrollTarget::<u64>::Key(99, AnchorPolicy::Center).resolve_top(
            &index,
            0,
            plain,
            &NoSourceMap
        ),
        None
    );
    assert_eq!(
        ScrollTarget::<u64>::Key(4, AnchorPolicy::ItemBottomAtBottom).final_position(
            &index,
            0,
            plain,
            &NoSourceMap
        ),
        Some(ListPosition::Anchored(ListAnchor::top(3)))
    );
}

#[test]
fn empty_and_single_item_indexes_resolve_without_stranding() {
    let empty = MeasuredListIndex::<u64>::new(0);
    let start = Instant::now();
    let to_bottom = motion(
        ListPosition::Top,
        ScrollTarget::Bottom,
        Easing::Linear,
        SECOND,
        start,
    );
    assert_eq!(top_at(&to_bottom, SECOND / 2, &empty, 5).top_offset, 0);
    assert_eq!(
        to_bottom.position_at(start + SECOND, &empty, geometry(5), &NoSourceMap),
        Some(ListPosition::FollowBottom)
    );
    // One item taller than the viewport.
    let tall = index(1, 50);
    assert_eq!(top_at(&to_bottom, SECOND, &tall, 5).top_offset, 45);
    assert_eq!(top_at(&to_bottom, SECOND / 2, &tall, 5).top_offset, 22);
}

#[test]
fn frames_are_scheduled_up_to_the_deadline() {
    let start = Instant::now();
    let motion = motion(
        ListPosition::Top,
        ScrollTarget::Bottom,
        Easing::Linear,
        Duration::from_millis(100),
        start,
    );
    let frame = Duration::from_millis(16);
    assert_eq!(motion.deadline(), start + Duration::from_millis(100));
    assert_eq!(motion.next_frame(start, frame), Some(start + frame));
    let late = start + Duration::from_millis(90);
    assert_eq!(motion.next_frame(late, frame), Some(motion.deadline()));
    assert_eq!(
        motion.next_frame(start, Duration::ZERO),
        Some(motion.deadline())
    );
    assert_eq!(motion.next_frame(motion.deadline(), frame), None);
    assert!(!motion.is_done(late));
    assert!(motion.is_done(motion.deadline()));
    // An unrepresentable deadline saturates instead of panicking.
    let forever = ScrollMotion::<u64>::new(
        ListPosition::Top,
        ScrollTarget::Bottom,
        Easing::Linear,
        Duration::MAX,
        start,
    );
    assert!(forever.deadline() > start);
    assert!(forever.next_frame(start, frame).is_some());
}

/// Uncontrolled state with `count` items of `height` rows laid out once at
/// `viewport` rows.
fn laid_out_state(count: u64, height: u64, viewport: u64) -> VirtualListState<u64> {
    let mut state = VirtualListState::new(0);
    state
        .splice_measured(
            0..0,
            (0..count).map(|key| MeasuredListItem::new(key, 0, 10, 0, height)),
        )
        .expect("unique keys");
    state.resolve_layout(viewport, &NoSourceMap);
    state
}

/// Advance the state's motion and lay out again, as a frame would.
fn advance(state: &mut VirtualListState<u64>, now: Instant) -> Option<Instant> {
    let next = state.advance_motion(now, Duration::from_millis(16), &NoSourceMap);
    let viewport = state.resolved().expect("laid out").viewport_height;
    state.resolve_layout(viewport, &NoSourceMap);
    next
}

#[test]
fn state_motion_commits_keyed_frames_and_completes_at_the_target() {
    let mut state = laid_out_state(40, 2, 10);
    let start = Instant::now();
    let duration = Duration::from_millis(100);
    state.start_motion(ScrollTarget::Bottom, Easing::Linear, duration, start);
    assert!(state.motion().is_some());
    assert_eq!(state.resolved().map(|r| r.top_offset), Some(0));
    let next = advance(&mut state, start + duration / 2);
    assert_eq!(next, Some(start + duration / 2 + Duration::from_millis(16)));
    assert_eq!(state.resolved().map(|r| r.top_offset), Some(35));
    assert_eq!(
        state.position(),
        ListPosition::Anchored(ListAnchor::row(17, 1))
    );
    assert_eq!(state.scroll.vertical_offset(), 35);
    // Content appended mid-motion moves the bottom target; completion lands
    // exactly on the new bottom and keeps following.
    state
        .splice_measured(40..40, [MeasuredListItem::new(40, 0, 10, 0, 6)])
        .expect("unique key");
    assert_eq!(advance(&mut state, start + duration), None);
    assert!(state.motion().is_none());
    assert_eq!(state.position(), ListPosition::FollowBottom);
    assert_eq!(state.resolved().map(|r| r.top_offset), Some(76));
}

#[test]
fn zero_duration_state_motion_applies_immediately() {
    let mut state = laid_out_state(20, 3, 6);
    let now = Instant::now();
    state.start_motion(
        ScrollTarget::Key(10, AnchorPolicy::ItemTopAtTop),
        Easing::EaseOut,
        Duration::ZERO,
        now,
    );
    assert!(state.motion().is_none());
    assert_eq!(
        state.position(),
        ListPosition::Anchored(ListAnchor::top(10))
    );
    assert_eq!(state.resolve_layout(6, &NoSourceMap).top_offset, 30);
}

#[test]
fn manual_input_and_explicit_positions_cancel_motion_where_it_is() {
    let mut state = laid_out_state(40, 2, 10);
    let start = Instant::now();
    let duration = Duration::from_millis(100);
    state.start_motion(ScrollTarget::Bottom, Easing::Linear, duration, start);
    advance(&mut state, start + duration / 2);
    assert!(state.apply_action(&VirtualListAction::ScrollRequested {
        rows: -5,
        source: ScrollInputSource::Wheel,
    }));
    assert!(state.motion().is_none());
    assert_eq!(state.resolved().map(|r| r.top_offset), Some(30));
    // Activation never cancels.
    state.start_motion(ScrollTarget::Top, Easing::Linear, duration, start);
    assert!(!state.apply_action(&VirtualListAction::Activated(3)));
    assert!(state.motion().is_some());
    state.set_position(ListPosition::Anchored(ListAnchor::top(4)));
    assert!(state.motion().is_none());
    // Legacy offset writes interrupt as well.
    state.start_motion(ScrollTarget::Top, Easing::Linear, duration, start);
    state.scroll.set_vertical_offset(12);
    assert_eq!(state.advance_motion(start, duration, &NoSourceMap), None);
    assert_eq!(state.position(), ListPosition::Anchored(ListAnchor::top(6)));
    assert!(state.motion().is_none());
    state.start_motion(ScrollTarget::Top, Easing::Linear, duration, start);
    assert!(state.cancel_motion());
    assert!(!state.cancel_motion());
}

#[test]
fn restarting_continues_from_the_committed_frame() {
    let mut state = laid_out_state(40, 2, 10);
    let start = Instant::now();
    let duration = Duration::from_millis(100);
    state.start_motion(ScrollTarget::Bottom, Easing::Linear, duration, start);
    advance(&mut state, start + duration / 2);
    let midway = start + duration / 2;
    state.start_motion(ScrollTarget::Top, Easing::Linear, duration, midway);
    let motion = state.motion().expect("running");
    assert_eq!(motion.from, ListPosition::Anchored(ListAnchor::row(17, 1)));
    advance(&mut state, midway + duration / 2);
    // Halfway from 35 to 0, rounded toward the start.
    assert_eq!(state.resolved().map(|r| r.top_offset), Some(18));
}

#[test]
fn controlled_state_ignores_motion_helpers_and_vanished_targets_cancel() {
    let mut controlled = VirtualListState::<u64>::with_control(0, PositionControl::Controlled);
    let now = Instant::now();
    controlled.start_motion(ScrollTarget::Bottom, Easing::Linear, SECOND, now);
    assert!(controlled.motion().is_none());
    assert_eq!(controlled.advance_motion(now, SECOND, &NoSourceMap), None);

    let mut state = laid_out_state(10, 1, 4);
    state.start_motion(
        ScrollTarget::Key(7, AnchorPolicy::ItemTopAtTop),
        Easing::Linear,
        SECOND,
        now,
    );
    state.splice_measured(7..8, []).expect("in bounds");
    assert_eq!(
        state.advance_motion(now + SECOND / 2, SECOND, &NoSourceMap),
        None
    );
    assert!(state.motion().is_none());
}

#[test]
fn motion_resolves_source_anchors_through_the_source_map() {
    struct Lines;
    impl SourceMap<u64, u32> for Lines {
        fn row_for_source(&self, _key: &u64, position: &u32) -> Option<u64> {
            Some(u64::from(*position) * 2)
        }
        fn source_for_row(&self, _key: &u64, row: u64) -> Option<(u32, u64)> {
            u32::try_from(row / 2)
                .ok()
                .map(|line| (line, u64::from(line) * 2))
        }
    }
    let index = index(4, 20);
    let motion = ScrollMotion::<u64, u32>::new(
        ListPosition::Top,
        ScrollTarget::Anchor(ListAnchor::new(
            2,
            AnchorOffset::Source {
                position: 3,
                row_delta: 1,
            },
        )),
        Easing::Linear,
        SECOND,
        Instant::now(),
    );
    let sample = motion
        .sample_elapsed(SECOND, &index, geometry(10), &Lines)
        .expect("resolves");
    assert_eq!(sample.top_offset, 47);
}
