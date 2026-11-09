use super::*;

const MORE: EdgeAvailability = EdgeAvailability::MoreAvailable;
const DONE: EdgeAvailability = EdgeAvailability::Exhausted;

fn items(keys: Range<u32>) -> Vec<ResidentItem<u32>> {
    keys.map(ResidentItem::new).collect()
}

fn window(
    keys: Range<u32>,
    before: EdgeAvailability,
    after: EdgeAvailability,
) -> ResidentWindow<u32> {
    ResidentWindow::from_items(items(keys), before, after, PositionHints::unknown())
}

fn keys(window: &ResidentWindow<u32>) -> Vec<u32> {
    window.keys().copied().collect()
}

fn requested(demand: &SideDemand<u32>) -> PageRequest<u32> {
    match demand {
        SideDemand::Requested(request) => request.clone(),
        other => panic!("expected a request, got {other:?}"),
    }
}

/// Every item is `rows` tall.
fn uniform(rows: u64) -> impl Fn(&u32) -> u64 {
    move |_| rows
}

fn assert_consistent(window: &ResidentWindow<u32>) {
    let mut weight = 0;
    for (index, item) in window.items().enumerate() {
        assert_eq!(
            window.index_of(&item.key),
            Some(index),
            "index of {}",
            item.key
        );
        weight += item.weight;
    }
    assert_eq!(window.total_weight(), weight);
    assert_eq!(window.ordinals.len(), window.len());
}

/// Apply splices to a mirror vector, as a caller would to its list.
fn apply_splices(mirror: &mut Vec<u32>, splices: &[WindowSplice<u32>]) {
    for splice in splices {
        let removed: Vec<u32> = mirror
            .splice(splice.removed_range(), splice.inserted.iter().copied())
            .collect();
        assert_eq!(removed, splice.removed);
    }
}

// ---- construction and membership ----

#[test]
fn empty_window_reports_empty_and_blocks_page_demands() {
    let mut window = ResidentWindow::<u32>::new();
    assert!(window.is_empty());
    assert_eq!(window.first_key(), None);
    assert_eq!(
        window.boundary_status(WindowSide::Before),
        BoundaryStatus::MoreAvailable
    );
    let demands = window.poll_demands(EdgeRows::default(), &PrefetchPolicy::symmetric(10));
    assert_eq!(
        demands.before,
        SideDemand::Blocked(DemandBlock::EmptyWindow)
    );
    assert_eq!(demands.after, SideDemand::Blocked(DemandBlock::EmptyWindow));
    assert_eq!(window.in_flight(WindowSide::Before), None);
}

#[test]
fn from_items_skips_repeated_keys_and_indexes_in_order() {
    let window = ResidentWindow::from_items(
        [1, 2, 2, 3, 1].map(ResidentItem::new),
        MORE,
        MORE,
        PositionHints::unknown(),
    );
    assert_eq!(keys(&window), vec![1, 2, 3]);
    assert_consistent(&window);
}

#[test]
fn single_item_window_requests_both_sides() {
    let mut window = window(5..6, MORE, MORE);
    let demands = window.poll_demands(EdgeRows::default(), &PrefetchPolicy::symmetric(1));
    assert_eq!(requested(&demands.before).edge_key, 5);
    assert_eq!(requested(&demands.after).edge_key, 5);
    assert_eq!(demands.requests().count(), 2);
}

#[test]
fn remove_middle_and_edge_keys_keeps_indices_consistent() {
    let mut window = window(0..6, MORE, MORE);
    let splice = window.remove(&2).unwrap();
    assert_eq!(splice.at, 2);
    assert_eq!(splice.removed, vec![2]);
    assert_consistent(&window);
    window.remove(&0).unwrap();
    window.remove(&5).unwrap();
    assert_eq!(keys(&window), vec![1, 3, 4]);
    assert_consistent(&window);
    assert!(window.remove(&42).is_none());
    // Prepending after removals still indexes correctly.
    window.push_front(items(10..12));
    assert_eq!(keys(&window), vec![10, 11, 1, 3, 4]);
    assert_consistent(&window);
}

#[test]
fn set_weight_updates_total_and_revision() {
    let mut window = window(0..3, MORE, MORE);
    let revision = window.revision();
    assert!(window.set_weight(&1, 10));
    assert_eq!(window.total_weight(), 12);
    assert!(window.revision() > revision);
    assert!(!window.set_weight(&9, 1));
}

// ---- prefetch ----

#[test]
fn prefetch_triggers_only_within_distance() {
    let mut window = window(10..20, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(8);
    let far = window.poll_demands(
        EdgeRows {
            before: 8,
            after: 30,
        },
        &policy,
    );
    assert_eq!(far.before, SideDemand::Idle);
    assert_eq!(far.after, SideDemand::Idle);

    let near = window.poll_demands(
        EdgeRows {
            before: 7,
            after: 30,
        },
        &policy,
    );
    let request = requested(&near.before);
    assert_eq!(request.side, WindowSide::Before);
    assert_eq!(request.edge_key, 10);
    assert_eq!(near.after, SideDemand::Idle);
    assert_eq!(
        window.boundary_status(WindowSide::Before),
        BoundaryStatus::Loading
    );
    assert_eq!(
        window.boundary_status(WindowSide::After),
        BoundaryStatus::MoreAvailable
    );
}

#[test]
fn zero_distance_disables_prefetch() {
    let mut window = window(0..3, MORE, MORE);
    let demands = window.poll_demands(EdgeRows::default(), &PrefetchPolicy::default());
    assert_eq!(demands.before, SideDemand::Idle);
    assert_eq!(demands.after, SideDemand::Idle);
}

#[test]
fn boundary_signals_are_read_only() {
    let window = window(0..3, MORE, DONE);
    let signals = window.boundary_signals(EdgeRows::default(), &PrefetchPolicy::symmetric(5));
    assert!(signals.need_before);
    assert!(!signals.need_after);
    assert_eq!(window.in_flight(WindowSide::Before), None);
}

#[test]
fn repeated_polls_never_duplicate_requests() {
    let mut window = window(0..5, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(10);
    let first = window.poll_demands(EdgeRows::default(), &policy);
    let before = requested(&first.before);
    let after = requested(&first.after);
    assert_ne!(before.token, after.token);
    for _ in 0..3 {
        let again = window.poll_demands(EdgeRows::default(), &policy);
        assert_eq!(again.before, SideDemand::InFlight(before.token));
        assert_eq!(again.after, SideDemand::InFlight(after.token));
        assert_eq!(again.requests().count(), 0);
    }
}

#[test]
fn exhausted_sides_never_request() {
    let mut window = window(0..5, DONE, DONE);
    let demands = window.poll_demands(EdgeRows::default(), &PrefetchPolicy::symmetric(100));
    assert_eq!(demands.before, SideDemand::Exhausted);
    assert_eq!(demands.after, SideDemand::Exhausted);
    assert_eq!(
        window.boundary_status(WindowSide::Before),
        BoundaryStatus::Exhausted
    );
}

#[test]
fn page_result_with_exhausted_edge_stops_further_requests() {
    let mut window = window(10..20, MORE, DONE);
    let policy = PrefetchPolicy::symmetric(10);
    let request = requested(&window.poll_demands(EdgeRows::default(), &policy).before);
    window
        .apply_page(request.token, PageResult::new(items(0..10), DONE))
        .unwrap();
    assert_eq!(window.availability(WindowSide::Before), DONE);
    assert_eq!(window.hints().before, Some(0));
    let demands = window.poll_demands(EdgeRows::default(), &policy);
    assert_eq!(demands.before, SideDemand::Exhausted);
}

#[test]
fn ceiling_blocks_requests_counting_in_flight_pages() {
    let mut window = window(0..10, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(10).ceiling(WeightCeiling {
        max_weight: 25,
        page_reserve: 10,
    });
    // 10 + 10 fits for the first page; the second needs 10 + 2 * 10 > 25.
    let demands = window.poll_demands(EdgeRows::default(), &policy);
    requested(&demands.before);
    assert_eq!(demands.after, SideDemand::Blocked(DemandBlock::Ceiling));
}

#[test]
fn ceiling_blocks_when_resident_weight_is_full() {
    let mut window = window(0..10, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(10).ceiling(WeightCeiling {
        max_weight: 15,
        page_reserve: 10,
    });
    let demands = window.poll_demands(EdgeRows::default(), &policy);
    assert_eq!(demands.before, SideDemand::Blocked(DemandBlock::Ceiling));
    assert_eq!(demands.after, SideDemand::Blocked(DemandBlock::Ceiling));
}

// ---- page results ----

#[test]
fn prepend_and_append_return_keyed_splices() {
    let mut window = window(10..13, MORE, MORE);
    let mut mirror = keys(&window);
    let policy = PrefetchPolicy::symmetric(1);
    let demands = window.poll_demands(EdgeRows::default(), &policy);
    let before = requested(&demands.before);
    let after = requested(&demands.after);

    let applied = window
        .apply_page(after.token, PageResult::new(items(13..15), MORE))
        .unwrap();
    assert_eq!(applied.side, WindowSide::After);
    let splice = applied.splice.unwrap();
    assert_eq!(splice.at, 3);
    apply_splices(&mut mirror, &[splice]);

    let applied = window
        .apply_page(before.token, PageResult::new(items(7..10), MORE))
        .unwrap();
    let splice = applied.splice.unwrap();
    assert_eq!((splice.at, splice.inserted.clone()), (0, vec![7, 8, 9]));
    apply_splices(&mut mirror, &[splice]);

    assert_eq!(mirror, keys(&window));
    assert_eq!(keys(&window), (7..15).collect::<Vec<_>>());
    assert_eq!(window.last_extension(), Some(WindowSide::Before));
    assert_consistent(&window);
}

#[test]
fn page_overlap_skips_resident_keys() {
    let mut window = window(10..15, MORE, MORE);
    let request = requested(
        &window
            .poll_demands(EdgeRows::default(), &PrefetchPolicy::symmetric(1))
            .before,
    );
    let applied = window
        .apply_page(
            request.token,
            PageResult::new([8, 9, 10, 11, 9].map(ResidentItem::new), MORE),
        )
        .unwrap();
    assert_eq!(applied.skipped_duplicates, 3);
    assert_eq!(applied.splice.unwrap().inserted, vec![8, 9]);
    assert_consistent(&window);
}

#[test]
fn empty_page_releases_slot_without_splice() {
    let mut window = window(0..3, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(1);
    let request = requested(&window.poll_demands(EdgeRows::default(), &policy).after);
    let applied = window
        .apply_page(request.token, PageResult::new(Vec::new(), MORE))
        .unwrap();
    assert!(applied.splice.is_none());
    assert_eq!(window.in_flight(WindowSide::After), None);
    // The side may request again (zero-row pages are the caller's concern).
    let again = window.poll_demands(EdgeRows::default(), &policy);
    assert!(requested(&again.after).token > request.token);
}

#[test]
fn page_hints_track_remaining_weight() {
    let mut window =
        ResidentWindow::from_items(items(50..60), MORE, MORE, PositionHints::known(50, 40));
    let policy = PrefetchPolicy::symmetric(1);
    let demands = window.poll_demands(EdgeRows::default(), &policy);
    let before = requested(&demands.before);
    let after = requested(&demands.after);
    window
        .apply_page(before.token, PageResult::new(items(40..50), MORE))
        .unwrap();
    assert_eq!(window.hints().before, Some(40));
    window
        .apply_page(
            after.token,
            PageResult::new(items(60..65), MORE).remaining_hint(7),
        )
        .unwrap();
    assert_eq!(window.hints().after, Some(7));
}

// ---- token fencing ----

#[test]
fn stale_tokens_are_rejected() {
    let mut window = window(10..20, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(1);
    let request = requested(&window.poll_demands(EdgeRows::default(), &policy).before);

    // Never issued.
    let bogus = RequestToken(999);
    let error = window
        .apply_page(bogus, PageResult::new(items(0..1), MORE))
        .unwrap_err();
    assert_eq!(error.reason, StaleReason::NotInFlight);

    // Applied once, rejected the second time.
    window
        .apply_page(request.token, PageResult::new(items(5..10), MORE))
        .unwrap();
    let error = window
        .apply_page(request.token, PageResult::new(items(0..5), MORE))
        .unwrap_err();
    assert_eq!(error.reason, StaleReason::NotInFlight);
    assert_eq!(keys(&window), (5..20).collect::<Vec<_>>());
    assert_eq!(
        error.to_string(),
        format!("request {} is not in flight", request.token.get())
    );
}

#[test]
fn cancelled_request_result_is_ignored() {
    let mut window = window(10..20, MORE, MORE);
    let request = requested(
        &window
            .poll_demands(EdgeRows::default(), &PrefetchPolicy::symmetric(1))
            .before,
    );
    assert_eq!(window.cancel(WindowSide::Before), Some(request.token));
    assert_eq!(window.cancel(WindowSide::Before), None);
    let revision = window.revision();
    let error = window
        .apply_page(request.token, PageResult::new(items(0..10), MORE))
        .unwrap_err();
    assert_eq!(error.reason, StaleReason::NotInFlight);
    assert_eq!(window.revision(), revision);
}

#[test]
fn edge_change_rejects_page_and_frees_slot() {
    let mut window = window(10..20, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(1);
    let request = requested(&window.poll_demands(EdgeRows::default(), &policy).after);
    window.push_back([ResidentItem::new(20)]);
    let error = window
        .apply_page(request.token, PageResult::new(items(21..25), MORE))
        .unwrap_err();
    assert_eq!(error.reason, StaleReason::EdgeChanged);
    assert_eq!(window.in_flight(WindowSide::After), None);
    let again = requested(&window.poll_demands(EdgeRows::default(), &policy).after);
    assert_eq!(again.edge_key, 20);
}

#[test]
fn reversal_cancels_opposite_request_when_configured() {
    let mut window = window(0..10, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(5).reversal(ReversalPolicy::CancelOpposite);
    let after = requested(
        &window
            .poll_demands(
                EdgeRows {
                    before: 50,
                    after: 0,
                },
                &policy,
            )
            .after,
    );
    // The reader reverses: the before side now needs rows, the after side not.
    let demands = window.poll_demands(
        EdgeRows {
            before: 0,
            after: 50,
        },
        &policy,
    );
    let before = requested(&demands.before);
    assert_eq!(demands.after, SideDemand::Cancelled(after.token));
    assert!(before.token > after.token);
    let error = window
        .apply_page(after.token, PageResult::new(items(10..12), MORE))
        .unwrap_err();
    assert_eq!(error.reason, StaleReason::NotInFlight);
    window
        .apply_page(
            before.token,
            PageResult::new([ResidentItem::new(100)], MORE),
        )
        .unwrap();
}

#[test]
fn reversal_keeps_opposite_request_that_still_needs_rows() {
    let mut window = window(0..2, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(5).reversal(ReversalPolicy::CancelOpposite);
    let after = requested(
        &window
            .poll_demands(
                EdgeRows {
                    before: 50,
                    after: 0,
                },
                &policy,
            )
            .after,
    );
    let demands = window.poll_demands(
        EdgeRows {
            before: 0,
            after: 1,
        },
        &policy,
    );
    requested(&demands.before);
    assert_eq!(demands.after, SideDemand::InFlight(after.token));
}

#[test]
fn default_reversal_keeps_both_requests() {
    let mut window = window(0..10, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(5);
    let after = requested(
        &window
            .poll_demands(
                EdgeRows {
                    before: 50,
                    after: 0,
                },
                &policy,
            )
            .after,
    );
    let demands = window.poll_demands(
        EdgeRows {
            before: 0,
            after: 50,
        },
        &policy,
    );
    requested(&demands.before);
    assert_eq!(demands.after, SideDemand::InFlight(after.token));
}

// ---- replacement ----

#[test]
fn replace_fences_pages_and_older_replacements() {
    let mut window = window(10..20, MORE, MORE);
    let policy = PrefetchPolicy::symmetric(1);
    let page = requested(&window.poll_demands(EdgeRows::default(), &policy).before);

    let first = window.request_replace(JumpTarget::Around(500_u64));
    assert_eq!(window.in_flight(WindowSide::Before), None);
    assert_eq!(
        window.boundary_status(WindowSide::After),
        BoundaryStatus::Loading
    );
    let demands = window.poll_demands(EdgeRows::default(), &policy);
    assert_eq!(demands.before, SideDemand::Blocked(DemandBlock::Replacing));
    let second = window.request_replace::<u64>(JumpTarget::Head);
    assert!(second.token > first.token);

    let result = |range: Range<u32>| ReplaceResult {
        items: items(range),
        before: DONE,
        after: MORE,
        hints: PositionHints::known(0, 1000),
    };
    assert_eq!(
        window
            .apply_page(page.token, PageResult::new(items(0..10), MORE))
            .unwrap_err()
            .reason,
        StaleReason::NotInFlight
    );
    assert_eq!(
        window
            .apply_replace(first.token, result(490..510))
            .unwrap_err()
            .reason,
        StaleReason::NotInFlight
    );
    let mut mirror = keys(&window);
    let applied = window.apply_replace(second.token, result(0..8)).unwrap();
    assert_eq!(applied.splice.removed, (10..20).collect::<Vec<_>>());
    apply_splices(&mut mirror, &[applied.splice]);
    assert_eq!(mirror, (0..8).collect::<Vec<_>>());
    assert_eq!(keys(&window), mirror);
    assert_eq!(window.availability(WindowSide::Before), DONE);
    assert_eq!(window.replace_in_flight(), None);
    assert_eq!(window.last_extension(), None);
    assert_consistent(&window);
    // A replayed replacement result is rejected.
    assert!(window.apply_replace(second.token, result(0..3)).is_err());
}

#[test]
fn cancelled_replace_is_rejected_and_unblocks_pages() {
    let mut window = window(0..5, MORE, MORE);
    let request = window.request_replace::<()>(JumpTarget::Tail);
    assert_eq!(window.cancel_replace(), Some(request.token));
    let result = ReplaceResult {
        items: items(0..1),
        before: MORE,
        after: DONE,
        hints: PositionHints::unknown(),
    };
    assert!(window.apply_replace(request.token, result).is_err());
    let demands = window.poll_demands(EdgeRows::default(), &PrefetchPolicy::symmetric(1));
    requested(&demands.before);
}

#[test]
fn replace_with_empty_result_and_repeated_keys() {
    let mut window = window(0..5, MORE, MORE);
    let request = window.request_replace::<()>(JumpTarget::Tail);
    let applied = window
        .apply_replace(
            request.token,
            ReplaceResult {
                items: [7, 7, 8].map(ResidentItem::new).to_vec(),
                before: MORE,
                after: DONE,
                hints: PositionHints::unknown(),
            },
        )
        .unwrap();
    assert_eq!(applied.skipped_duplicates, 1);
    assert_eq!(keys(&window), vec![7, 8]);
    assert_eq!(window.hints().after, Some(0));
    assert_consistent(&window);
}

// ---- eviction ----

fn budget(trigger: u64, target: u64, sides: EvictionSides) -> EvictionBudget {
    EvictionBudget {
        trigger_weight: trigger,
        target_weight: target,
        sides,
    }
}

/// `view` is `(anchor, top_from_anchor, height)`.
fn plan(
    window: &ResidentWindow<u32>,
    (anchor, top_from_anchor, height): (u32, i64, u64),
    band: ProtectedBand,
    budget: EvictionBudget,
    pinned: &[u32],
    rows: &dyn ItemRows<u32>,
) -> EvictionPlan {
    window
        .plan_eviction(&EvictionInput {
            placement: ViewportPlacement {
                anchor: &anchor,
                top_from_anchor,
                height,
            },
            band,
            budget,
            pinned,
            rows,
        })
        .unwrap()
}

#[test]
fn no_eviction_at_or_below_trigger() {
    let window = window(0..10, MORE, MORE);
    let result = plan(
        &window,
        (5, 0, 1),
        ProtectedBand::symmetric(0, 1),
        budget(
            10,
            2,
            EvictionSides::Both {
                first: WindowSide::Before,
            },
        ),
        &[],
        &uniform(1),
    );
    assert!(result.is_empty());
    assert_eq!(result.removed_weight, 0);
}

#[test]
fn eviction_trims_opposite_of_last_extension_and_never_the_band() {
    let mut window = window(10..30, MORE, MORE);
    let request = requested(
        &window
            .poll_demands(EdgeRows::default(), &PrefetchPolicy::symmetric(1))
            .after,
    );
    window
        .apply_page(request.token, PageResult::new(items(30..40), MORE))
        .unwrap();
    // 30 items of 2 rows; viewport shows items 20..23 (6 rows) with 4 rows of
    // band on each side => items 18..25 protected.
    let band = ProtectedBand::symmetric(4, 100);
    let result = plan(
        &window,
        (20, 0, 6),
        band,
        budget(20, 10, EvictionSides::default()),
        &[],
        &uniform(2),
    );
    assert_eq!(result.protected, 8..15);
    assert!(result.after.is_empty(), "after side was just extended");
    assert_eq!(
        result.before,
        0..8,
        "cannot reach target without entering the band"
    );
    assert_eq!(result.removed_weight, 8);

    let mut mirror = keys(&window);
    let splices = window.apply_eviction(&result).unwrap();
    apply_splices(&mut mirror, &splices);
    assert_eq!(mirror, keys(&window));
    assert_eq!(window.first_key(), Some(&18));
    assert!(window.contains(&20));
    assert_consistent(&window);
}

#[test]
fn eviction_stops_once_target_reached() {
    let window = window(0..20, MORE, MORE);
    let result = plan(
        &window,
        (15, 0, 1),
        ProtectedBand::symmetric(0, 1),
        budget(10, 15, EvictionSides::Only(WindowSide::Before)),
        &[],
        &uniform(1),
    );
    assert_eq!(result.before, 0..5);
    assert_eq!(result.removed_weight, 5);
}

#[test]
fn eviction_both_sides_trims_second_side_when_needed() {
    let window = window(0..20, MORE, MORE);
    let result = plan(
        &window,
        (2, 0, 1),
        ProtectedBand::symmetric(0, 1),
        budget(
            5,
            5,
            EvictionSides::Both {
                first: WindowSide::Before,
            },
        ),
        &[],
        &uniform(1),
    );
    assert_eq!(result.protected, 2..3);
    assert_eq!(result.before, 0..2);
    assert_eq!(result.after, 7..20);
    assert_eq!(result.removed_weight, 15);
    assert_eq!(window.total_weight() - result.removed_weight, 5);
}

#[test]
fn band_cap_limits_protection_but_anchor_always_survives() {
    let window = window(0..20, MORE, MORE);
    let result = plan(
        &window,
        (10, 0, 5),
        ProtectedBand::symmetric(100, 0),
        budget(
            0,
            0,
            EvictionSides::Both {
                first: WindowSide::After,
            },
        ),
        &[],
        &uniform(1),
    );
    assert_eq!(result.protected, 10..11);
    assert_eq!(result.before, 0..10);
    assert_eq!(result.after, 11..20);

    let capped = plan(
        &window,
        (10, 0, 5),
        ProtectedBand::symmetric(100, 3),
        budget(
            0,
            0,
            EvictionSides::Both {
                first: WindowSide::After,
            },
        ),
        &[],
        &uniform(1),
    );
    // Nearest first: the viewport items below the anchor have distance zero.
    assert_eq!(capped.protected, 10..13);
}

#[test]
fn band_weight_cap_limits_protection() {
    let mut window = window(0..10, MORE, MORE);
    window.set_weight(&6, 50);
    let band = ProtectedBand::symmetric(100, 100).max_weight(10);
    // Item 6 (weight 50) is visible, so it is the nearest candidate, and it
    // exceeds the cap: protection stops there.
    let result = plan(
        &window,
        (5, 0, 2),
        band,
        budget(
            0,
            0,
            EvictionSides::Both {
                first: WindowSide::Before,
            },
        ),
        &[],
        &uniform(1),
    );
    assert_eq!(result.protected, 5..6);
}

#[test]
fn pinned_keys_extend_the_protected_range() {
    let window = window(0..20, MORE, MORE);
    let result = plan(
        &window,
        (10, 0, 1),
        ProtectedBand::symmetric(0, 1),
        budget(
            0,
            0,
            EvictionSides::Both {
                first: WindowSide::Before,
            },
        ),
        &[3, 99, 16],
        &uniform(1),
    );
    assert_eq!(result.protected, 3..17);
    assert_eq!(result.before, 0..3);
    assert_eq!(result.after, 17..20);
}

#[test]
fn item_taller_than_viewport_is_protected_with_neighbors() {
    let window = window(0..10, MORE, MORE);
    let rows = |key: &u32| if *key == 5 { 100 } else { 1 };
    // Viewport sits 40 rows into the 100-row anchor; band of 2 rows on each
    // side lies entirely inside the anchor.
    let result = plan(
        &window,
        (5, 40, 10),
        ProtectedBand::symmetric(2, 100),
        budget(
            0,
            0,
            EvictionSides::Both {
                first: WindowSide::Before,
            },
        ),
        &[],
        &rows,
    );
    assert_eq!(result.protected, 5..6);
    // Viewport at the anchor top: two rows above protect items 3 and 4.
    let result = plan(
        &window,
        (5, 0, 10),
        ProtectedBand::symmetric(2, 100),
        budget(
            0,
            0,
            EvictionSides::Both {
                first: WindowSide::Before,
            },
        ),
        &[],
        &rows,
    );
    assert_eq!(result.protected, 3..6);
}

#[test]
fn negative_viewport_offset_protects_items_above_anchor() {
    let window = window(0..10, MORE, MORE);
    // Viewport starts 3 rows above anchor 5 (items 2..5 visible above it).
    let result = plan(
        &window,
        (5, -3, 4),
        ProtectedBand::symmetric(0, 100),
        budget(
            0,
            0,
            EvictionSides::Both {
                first: WindowSide::Before,
            },
        ),
        &[],
        &uniform(1),
    );
    assert_eq!(result.protected, 2..6);
}

#[test]
fn zero_height_items_inside_band_are_protected_outside_are_not() {
    let window = window(0..10, MORE, MORE);
    let rows = |key: &u32| u64::from(*key != 4 && *key != 7);
    // Viewport is item 5 only; band of zero rows. Item 4 (zero rows) sits at
    // the viewport top edge; item 7 sits below item 6, outside the viewport.
    let result = plan(
        &window,
        (5, 0, 1),
        ProtectedBand::symmetric(0, 100),
        budget(
            0,
            0,
            EvictionSides::Both {
                first: WindowSide::Before,
            },
        ),
        &[],
        &rows,
    );
    assert_eq!(result.protected, 4..6);
}

#[test]
fn eviction_respects_window_starts() {
    struct Groups;
    impl ItemRows<u32> for Groups {
        fn rows(&self, _key: &u32) -> u64 {
            1
        }
        fn is_window_start(&self, key: &u32) -> bool {
            key.is_multiple_of(4)
        }
    }
    let window = window(0..20, MORE, MORE);
    // Need 7: the before cut must leave the window starting at a multiple of
    // 4 no later than the anchor; cutting at 4 removes too little, so 8.
    let result = plan(
        &window,
        (10, 0, 1),
        ProtectedBand::symmetric(0, 1),
        budget(
            0,
            13,
            EvictionSides::Both {
                first: WindowSide::Before,
            },
        ),
        &[],
        &Groups,
    );
    assert_eq!(result.before, 0..8);
    assert!(result.after.is_empty(), "target already reached");
    // After side: the removed range must start at a multiple of 4 after the
    // anchor; 16..20 removes too little, so 12..20.
    let result = plan(
        &window,
        (10, 0, 1),
        ProtectedBand::symmetric(0, 1),
        budget(0, 13, EvictionSides::Only(WindowSide::After)),
        &[],
        &Groups,
    );
    assert_eq!(result.after, 12..20);
    // Nothing is removable when no window start lies outside the protected range.
    let result = plan(
        &window,
        (2, 0, 1),
        ProtectedBand::symmetric(0, 1),
        budget(0, 0, EvictionSides::Only(WindowSide::Before)),
        &[],
        &Groups,
    );
    assert!(result.is_empty());
}

#[test]
fn eviction_marks_side_available_cancels_its_page_and_grows_hint() {
    let mut window =
        ResidentWindow::from_items(items(0..20), DONE, MORE, PositionHints::known(0, 30));
    let policy = PrefetchPolicy::symmetric(1);
    let page = requested(&window.poll_demands(EdgeRows::default(), &policy).after);
    let result = plan(
        &window,
        (5, 0, 1),
        ProtectedBand::symmetric(0, 1),
        budget(
            0,
            0,
            EvictionSides::Both {
                first: WindowSide::After,
            },
        ),
        &[],
        &uniform(1),
    );
    window.apply_eviction(&result).unwrap();
    assert_eq!(keys(&window), vec![5]);
    assert_eq!(window.availability(WindowSide::Before), MORE);
    assert_eq!(window.hints(), PositionHints::known(5, 44));
    assert_eq!(window.in_flight(WindowSide::After), None);
    assert!(
        window
            .apply_page(page.token, PageResult::new(items(20..25), MORE))
            .is_err()
    );
}

#[test]
fn stale_eviction_plan_and_missing_anchor_are_rejected() {
    let mut window = window(0..10, MORE, MORE);
    let result = plan(
        &window,
        (5, 0, 1),
        ProtectedBand::symmetric(0, 1),
        budget(
            0,
            0,
            EvictionSides::Both {
                first: WindowSide::Before,
            },
        ),
        &[],
        &uniform(1),
    );
    window.push_back([ResidentItem::new(10)]);
    assert_eq!(
        window.apply_eviction(&result),
        Err(EvictionError::StalePlan)
    );
    assert_eq!(window.len(), 11);

    let missing = window.plan_eviction(&EvictionInput {
        placement: ViewportPlacement {
            anchor: &77,
            top_from_anchor: 0,
            height: 1,
        },
        band: ProtectedBand::symmetric(0, 1),
        budget: budget(0, 0, EvictionSides::default()),
        pinned: &[],
        rows: &uniform(1),
    });
    assert_eq!(missing, Err(EvictionError::AnchorNotResident));
}

#[test]
fn empty_plan_applies_without_changing_revision() {
    let mut window = window(0..3, MORE, MORE);
    let result = plan(
        &window,
        (1, 0, 1),
        ProtectedBand::symmetric(0, 1),
        budget(100, 0, EvictionSides::default()),
        &[],
        &uniform(1),
    );
    let revision = window.revision();
    assert_eq!(window.apply_eviction(&result).unwrap(), Vec::new());
    assert_eq!(window.revision(), revision);
}

// ---- estimates ----

#[test]
fn estimate_uses_hints_density_and_ignores_exhausted_sides() {
    let window =
        ResidentWindow::from_items(items(0..10), MORE, DONE, PositionHints::known(100, 50));
    let estimate = window.position_estimate(30, 2.5);
    assert_eq!(
        estimate,
        PositionEstimate {
            before: 250,
            resident: 30,
            after: 0
        }
    );
    let unknown = ResidentWindow::from_items(items(0..10), MORE, MORE, PositionHints::unknown());
    assert_eq!(unknown.position_estimate(30, 2.0).total(), 30);
    assert_eq!(window.position_estimate(30, f64::NAN).before, 0);
    assert_eq!(window.position_estimate(30, -1.0).before, 0);
}

#[test]
fn fraction_is_monotonic_and_bounded() {
    let estimate = PositionEstimate {
        before: 400,
        resident: 100,
        after: 300,
    };
    let mut previous = 0.0;
    for offset in 0..=150 {
        let fraction = estimate.fraction(offset, 20);
        assert!((0.0..=1.0).contains(&fraction));
        assert!(fraction >= previous);
        previous = fraction;
    }
    assert!(estimate.fraction(0, 1000).abs() < f64::EPSILON);
    let tail = PositionEstimate {
        before: 100,
        resident: 50,
        after: 0,
    };
    assert!((tail.fraction(30, 20) - 1.0).abs() < f64::EPSILON);
    assert!((tail.fraction(u64::MAX, 20) - 1.0).abs() < f64::EPSILON);
}

#[test]
fn fraction_targets_invert_the_estimate() {
    let estimate = PositionEstimate {
        before: 400,
        resident: 100,
        after: 300,
    };
    let viewport = 20;
    assert_eq!(
        estimate.target_for_fraction(0.0, viewport),
        FractionTarget::Before {
            rows_before_window: 400
        }
    );
    assert_eq!(
        estimate.target_for_fraction(1.0, viewport),
        FractionTarget::After {
            rows_after_window: 300
        }
    );
    let inside = estimate.fraction(40, viewport);
    assert_eq!(
        estimate.target_for_fraction(inside, viewport),
        FractionTarget::Resident { offset: 40 }
    );
    assert_eq!(
        estimate.target_for_fraction(f64::NAN, viewport),
        FractionTarget::Before {
            rows_before_window: 400
        }
    );
    // Fully resident content always lands inside the window.
    let resident = PositionEstimate {
        before: 0,
        resident: 50,
        after: 0,
    };
    assert_eq!(
        resident.target_for_fraction(1.0, 20),
        FractionTarget::Resident { offset: 30 }
    );
    assert_eq!(
        resident.target_for_fraction(0.5, 100),
        FractionTarget::Resident { offset: 0 }
    );
}

#[test]
fn density_smoothing_converges_and_ignores_zero_weight() {
    let mut density = RowDensity::default().smoothing(0.5);
    assert!((density.value() - 1.0).abs() < f64::EPSILON);
    density.observe(30, 10);
    assert!((density.value() - 2.0).abs() < 1e-9);
    density.observe(5, 0);
    assert!((density.value() - 2.0).abs() < 1e-9);
    for _ in 0..64 {
        density.observe(30, 10);
    }
    assert!((density.value() - 3.0).abs() < 1e-9);
    let frozen = RowDensity::new(4.0).smoothing(-3.0);
    let mut frozen_copy = frozen;
    frozen_copy.observe(100, 1);
    assert!((frozen_copy.value() - 4.0).abs() < f64::EPSILON);
}

#[cfg(feature = "scrollbar")]
#[test]
fn scrollbar_state_scales_huge_estimates() {
    let estimate = PositionEstimate {
        before: 10_000_000,
        resident: 1_000,
        after: 10_000_000,
    };
    let state = estimate.scrollbar_state(0, 50);
    assert!(state.content_len > 0);
    assert!(state.viewport_len >= 1);
    assert!(state.offset <= state.max_offset());
    // The thumb sits at the estimate's own fraction, scaled by the shared
    // scroll-view mapping.
    let expected = f64::from(state.max_offset()) * estimate.fraction(0, 50);
    assert!((f64::from(state.offset) - expected).abs() <= 1.0);
    assert_eq!(
        state,
        crate::scrollbar::scrollbar_state(estimate.total(), 50, estimate.position(0))
    );
    let small = PositionEstimate {
        before: 10,
        resident: 20,
        after: 10,
    }
    .scrollbar_state(5, 10);
    assert_eq!(
        (small.content_len, small.viewport_len, small.offset),
        (40, 10, 15)
    );
}

// ---- randomized model check ----

/// Deterministic linear congruential generator for reproducible sequences.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }
}

#[test]
fn randomized_operations_keep_window_and_mirror_in_sync() {
    let mut rng = Lcg(7);
    let mut window = ResidentWindow::from_items(
        items(1000..1010),
        MORE,
        MORE,
        PositionHints::known(1000, 1000),
    );
    let mut mirror = keys(&window);
    let mut next_front = 999_u32;
    let mut next_back = 1010_u32;
    let policy = PrefetchPolicy::symmetric(3);
    let mut pending: Vec<PageRequest<u32>> = Vec::new();
    for _ in 0..2000 {
        match rng.next(6) {
            0 => {
                let rows = EdgeRows {
                    before: rng.next(5),
                    after: rng.next(5),
                };
                let demands = window.poll_demands(rows, &policy);
                pending.extend(demands.requests().cloned());
            }
            1 if !pending.is_empty() => {
                let index = usize::try_from(rng.next(pending.len() as u64)).unwrap();
                let request = pending.swap_remove(index);
                let count = u32::try_from(rng.next(4)).unwrap();
                let page: Vec<ResidentItem<u32>> = match request.side {
                    WindowSide::Before => {
                        let start = next_front + 1 - count;
                        next_front -= count;
                        items(start..start + count)
                    }
                    WindowSide::After => {
                        let start = next_back;
                        next_back += count;
                        items(start..start + count)
                    }
                };
                if let Ok(applied) = window.apply_page(request.token, PageResult::new(page, MORE)) {
                    apply_splices(&mut mirror, applied.splice.as_slice());
                }
            }
            2 if window.len() > 2 => {
                let index = usize::try_from(rng.next(window.len() as u64)).unwrap();
                let key = *window.key_at(index).unwrap();
                let splice = window.remove(&key).unwrap();
                apply_splices(&mut mirror, &[splice]);
            }
            3 if !window.is_empty() => {
                let index = usize::try_from(rng.next(window.len() as u64)).unwrap();
                let anchor = *window.key_at(index).unwrap();
                let result = plan(
                    &window,
                    (anchor, 0, rng.next(4)),
                    ProtectedBand::symmetric(rng.next(4), 8),
                    budget(
                        12,
                        8,
                        EvictionSides::Both {
                            first: WindowSide::Before,
                        },
                    ),
                    &[],
                    &uniform(1),
                );
                assert!(result.protected.contains(&index));
                let splices = window.apply_eviction(&result).unwrap();
                apply_splices(&mut mirror, &splices);
                assert!(window.contains(&anchor));
            }
            4 => {
                let splice = window.push_back([ResidentItem::new(next_back)]);
                next_back += 1;
                apply_splices(&mut mirror, splice.as_slice());
            }
            _ => {
                if let Some(token) = window.cancel(WindowSide::Before) {
                    pending.retain(|request| request.token != token);
                }
            }
        }
        assert_eq!(mirror, keys(&window));
        assert_consistent(&window);
    }
}
