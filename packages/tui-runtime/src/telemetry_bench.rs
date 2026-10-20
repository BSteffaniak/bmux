//! Same-binary telemetry cost harness; never part of the production runtime.
use std::hint::black_box;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::mailbox::{ReliableSender, observe_receive, stats_snapshot, with_stats};
use crate::stats::{PresentationSample, QueueSource, RuntimeStats, TerminalDispatchRange, micros};

// Exact pre-telemetry snapshot fields/types, retained only for baseline copy cost.
#[derive(Clone, Copy, Default)]
struct BaselineStats {
    reliable_depth: usize,
    reliable_high_water: usize,
    terminal_depth: usize,
    terminal_high_water: usize,
    latest_depth: usize,
    latest_high_water: usize,
    reliable_rejected: u64,
    terminal_rejected: u64,
    latest_rejected: u64,
    latest_replaced: u64,
    reliable_processed: u64,
    terminal_processed: u64,
    latest_processed: u64,
    subscription_depth: usize,
    subscription_high_water: usize,
    subscription_rejected: u64,
    subscriptions_started: u64,
    subscriptions_cancelled: u64,
    subscriptions_completed: u64,
    timers_delivered: u64,
    scheduler_budget_exhausted: u64,
    redraw_requests: u64,
    redraw_coalesced: u64,
    frames_presented: u64,
    full_repaints: u64,
    presented_changed_cells: u64,
    presentation_delay_us: u64,
    presentation_time_us: u64,
    updates_completed: u64,
    update_time_us: u64,
    commands_started: u64,
    commands_queued: u64,
    commands_rejected: u64,
    commands_cancelled: u64,
    stale_command_completions: u64,
}

impl BaselineStats {
    // Keep the entire baseline snapshot observable without suppressing dead-code
    // warnings or benchmarking a smaller, optimized-away surrogate.
    fn checksum(self) -> u64 {
        let Self {
            reliable_depth,
            reliable_high_water,
            terminal_depth,
            terminal_high_water,
            latest_depth,
            latest_high_water,
            reliable_rejected,
            terminal_rejected,
            latest_rejected,
            latest_replaced,
            reliable_processed,
            terminal_processed,
            latest_processed,
            subscription_depth,
            subscription_high_water,
            subscription_rejected,
            subscriptions_started,
            subscriptions_cancelled,
            subscriptions_completed,
            timers_delivered,
            scheduler_budget_exhausted,
            redraw_requests,
            redraw_coalesced,
            frames_presented,
            full_repaints,
            presented_changed_cells,
            presentation_delay_us,
            presentation_time_us,
            updates_completed,
            update_time_us,
            commands_started,
            commands_queued,
            commands_rejected,
            commands_cancelled,
            stale_command_completions,
        } = black_box(self);
        [
            reliable_depth,
            reliable_high_water,
            terminal_depth,
            terminal_high_water,
            latest_depth,
            latest_high_water,
            subscription_depth,
            subscription_high_water,
        ]
        .into_iter()
        .fold(0_u64, |sum, n| sum.wrapping_add(u64::try_from(n).unwrap()))
        .wrapping_add(
            [
                reliable_rejected,
                terminal_rejected,
                latest_rejected,
                latest_replaced,
                reliable_processed,
                terminal_processed,
                latest_processed,
                subscription_rejected,
                subscriptions_started,
                subscriptions_cancelled,
                subscriptions_completed,
                timers_delivered,
                scheduler_budget_exhausted,
                redraw_requests,
                redraw_coalesced,
                frames_presented,
                full_repaints,
                presented_changed_cells,
                presentation_delay_us,
                presentation_time_us,
                updates_completed,
                update_time_us,
                commands_started,
                commands_queued,
                commands_rejected,
                commands_cancelled,
                stale_command_completions,
            ]
            .into_iter()
            .fold(0_u64, u64::wrapping_add),
        )
    }
}

fn baseline(count: u64, snapshot_every: u64) -> (Duration, u64) {
    let stats = Mutex::new(BaselineStats::default());
    let (tx, mut rx) = mpsc::channel(64);
    let started = Instant::now();
    let mut checksum = 0_u64;
    for value in 0..count {
        tx.try_send(black_box(value)).unwrap();
        {
            let mut stats = stats.lock().unwrap();
            stats.reliable_depth = tx.max_capacity() - tx.capacity();
            stats.reliable_high_water = stats.reliable_high_water.max(stats.reliable_depth);
        }
        let value = rx.try_recv().unwrap();
        stats.lock().unwrap().reliable_depth = rx.len();
        stats.lock().unwrap().reliable_processed += 1;
        let update_started = Instant::now();
        checksum = checksum.wrapping_add(black_box(value));
        let elapsed = update_started.elapsed();
        {
            let mut stats = stats.lock().unwrap();
            stats.updates_completed += 1;
            stats.update_time_us = stats.update_time_us.saturating_add(micros(elapsed));
        }
        if value % 64 == 63 {
            let scheduled = Instant::now();
            let present_started = Instant::now();
            black_box(checksum);
            let presented = Instant::now();
            let mut stats = stats.lock().unwrap();
            stats.frames_presented += 1;
            stats.presentation_time_us = stats
                .presentation_time_us
                .saturating_add(micros(presented.duration_since(present_started)));
            stats.presentation_delay_us = stats
                .presentation_delay_us
                .saturating_add(micros(presented.duration_since(scheduled)));
        }
        if value % snapshot_every == 0 {
            black_box(*stats.lock().unwrap());
        }
    }
    let elapsed = started.elapsed();
    black_box((*stats.lock().unwrap()).checksum());
    (elapsed, checksum)
}

fn instrumented(count: u64, snapshot_every: u64, detailed: bool) -> (Duration, u64) {
    let stats = Arc::new(Mutex::new(RuntimeStats::new(detailed)));
    let (tx, mut rx) = mpsc::channel(64);
    let sender = ReliableSender::new(tx, Arc::clone(&stats), false, detailed);
    let started = Instant::now();
    let mut checksum = 0_u64;
    for value in 0..count {
        sender.try_send(black_box(value)).unwrap();
        let admitted = rx.try_recv().unwrap();
        observe_receive(&rx, &stats, false);
        with_stats(&stats, |stats| stats.reliable_processed += 1);
        let update_started = Instant::now();
        checksum = checksum.wrapping_add(black_box(admitted.value));
        let elapsed = update_started.elapsed();
        with_stats(&stats, |stats| {
            if let Some(at) = admitted.at {
                stats.record_queue_age(QueueSource::ApplicationFifo, at, update_started);
            }
            stats.record_update(elapsed, true);
        });
        if value % 64 == 63 {
            let turn_elapsed = update_started.elapsed();
            if turn_elapsed >= Duration::from_millis(4) {
                with_stats(&stats, |stats| {
                    stats.record_turn(turn_elapsed, Duration::from_millis(4));
                });
            }
            let scheduled = Instant::now();
            let present_started = Instant::now();
            black_box(checksum);
            let presented = Instant::now();
            with_stats(&stats, |stats| {
                let elapsed = presented.duration_since(present_started);
                if let Some(detailed) = &mut stats.detailed {
                    detailed.present_duration.record(elapsed);
                    detailed.last_presentation = Some(PresentationSample {
                        attempt: value / 64 + 1,
                        started_us: micros(present_started.duration_since(started)),
                        elapsed_us: micros(elapsed),
                        succeeded: true,
                        terminal_dispatches: TerminalDispatchRange::Empty,
                    });
                }
                stats.frames_presented += 1;
                stats.presentation_time_us = stats
                    .presentation_time_us
                    .saturating_add(micros(presented.duration_since(present_started)));
                stats.presentation_delay_us = stats
                    .presentation_delay_us
                    .saturating_add(micros(presented.duration_since(scheduled)));
            });
        }
        if value % snapshot_every == 0 {
            black_box(stats_snapshot(&stats));
        }
    }
    (started.elapsed(), checksum)
}

/// A pipeline-cost harness, not a full runtime replay: one application FIFO,
/// no-op update, no-op presentation every 64 messages, and controlled snapshots.
/// Includes added clocks, actual timed mailbox, histogram/sample locks and larger
/// snapshots. Excludes source rotation, terminal parsing, real paint/output,
/// subscriptions, commands, latest replacement, contention, and payload pressure.
#[test]
#[ignore = "manual release same-binary full FIFO pipeline telemetry comparison"]
fn release_pipeline_telemetry_overhead() {
    const COUNT: u64 = 500_000;
    for detailed in [false, true] {
        for snapshot_every in [1, 64, 4_096] {
            let _warmup = (
                baseline(1_000, snapshot_every),
                instrumented(1_000, snapshot_every, detailed),
            );
            let mut off = Vec::new();
            let mut on = Vec::new();
            for pair in 0..10 {
                let (a, b) = if pair % 2 == 0 {
                    (
                        baseline(COUNT, snapshot_every),
                        instrumented(COUNT, snapshot_every, detailed),
                    )
                } else {
                    let b = instrumented(COUNT, snapshot_every, detailed);
                    (baseline(COUNT, snapshot_every), b)
                };
                assert_eq!(a.1, b.1);
                off.push(a.0.as_nanos() / u128::from(COUNT));
                on.push(b.0.as_nanos() / u128::from(COUNT));
            }
            off.sort_unstable();
            on.sort_unstable();
            eprintln!(
                "pipeline telemetry detailed={detailed} release={} snapshot_every={snapshot_every} messages={COUNT} pairs=10 baseline_ns={off:?} instrumented_ns={on:?} upper_medians={}/{} snapshot_bytes={}/{}",
                !cfg!(debug_assertions),
                off[5],
                on[5],
                size_of::<BaselineStats>(),
                size_of::<RuntimeStats>()
            );
        }
    }
}
