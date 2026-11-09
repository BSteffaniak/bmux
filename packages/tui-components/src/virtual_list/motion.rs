//! Keyed scroll motion: an eased transition between two keyed positions.
//!
//! Both endpoints are keyed and re-resolved against the current index and
//! geometry at every sample, so content that moves during a motion never
//! strands it at stale rows. Time is always supplied by the caller: BMUX never
//! reads a wall clock, so a runtime's `RuntimeHandle::now` (or a deterministic
//! test clock) controls every sample.

use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use bmux_tui::style::Easing;

use super::position::anchor_row;
use super::{
    AnchorPolicy, ListAnchor, ListGeometry, ListPosition, MeasuredListIndex, SourceMap, anchor_for,
    position_at_offset, resolve_position, target_top_offset,
};

/// Progress quantization: eased progress is applied as an integer fraction of
/// this denominator, so samples are deterministic across platforms.
const PROGRESS_SCALE: u64 = 1 << 16;

/// Fallback horizon for durations whose deadline cannot be represented.
const HORIZON: Duration = Duration::from_secs(1 << 30);

/// Destination of a [`ScrollMotion`], kept keyed so it follows content.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScrollTarget<K, S = ()> {
    /// Place an item according to a policy (resolved relative to the motion's
    /// sampled start for [`AnchorPolicy::EnsureVisibleFrom`]).
    Key(K, AnchorPolicy),
    /// The first collection row; completes at [`ListPosition::Top`].
    Top,
    /// The content bottom; completes at [`ListPosition::FollowBottom`].
    Bottom,
    /// An anchor row on the viewport's first row. It may land in overscroll
    /// when the list permits it.
    Anchor(ListAnchor<K, S>),
}

impl<K: Clone + Ord, S: Clone> ScrollTarget<K, S> {
    /// Top offset of this target under `geometry`, clamped to
    /// `0..=max_overscroll_top`. `current_top` is the start used by
    /// [`AnchorPolicy::EnsureVisibleFrom`]. Returns `None` when a target key
    /// is not indexed.
    #[must_use]
    pub fn resolve_top(
        &self,
        index: &MeasuredListIndex<K>,
        current_top: u64,
        geometry: ListGeometry,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<u64> {
        let total = index.total_height();
        match self {
            Self::Key(key, policy) => target_top_offset(index, key, *policy, current_top, geometry),
            Self::Top => Some(0),
            Self::Bottom => Some(geometry.max_content_top(total)),
            Self::Anchor(anchor) => anchor_row(index, anchor, sources)
                .map(|row| row.min(geometry.max_overscroll_top(total))),
        }
    }

    /// Keyed position a completed motion toward this target commits.
    ///
    /// * `Key` commits [`anchor_for`] (the item at the target top);
    /// * `Top` commits [`ListPosition::Top`] and `Bottom`
    ///   [`ListPosition::FollowBottom`];
    /// * `Anchor` commits the anchor itself, as `Overscrolled` when it resolves
    ///   past `max_content_top`.
    ///
    /// Returns `None` when a target key is not indexed.
    #[must_use]
    pub fn final_position(
        &self,
        index: &MeasuredListIndex<K>,
        current_top: u64,
        geometry: ListGeometry,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<ListPosition<K, S>> {
        match self {
            Self::Key(key, policy) => {
                anchor_for(index, key, *policy, current_top, geometry, sources)
            }
            Self::Top => Some(ListPosition::Top),
            Self::Bottom => Some(ListPosition::FollowBottom),
            Self::Anchor(anchor) => {
                let top = self.resolve_top(index, current_top, geometry, sources)?;
                Some(if top > geometry.max_content_top(index.total_height()) {
                    ListPosition::Overscrolled(anchor.clone())
                } else {
                    ListPosition::Anchored(anchor.clone())
                })
            }
        }
    }
}

/// One sampled frame of a [`ScrollMotion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MotionSample {
    /// Collection row shown on the viewport's first row.
    pub top_offset: u64,
    /// Whether the motion has reached its target.
    pub done: bool,
}

/// An eased transition from a keyed start position to a keyed target.
///
/// `Instant` is [`std::time::Instant`] on native targets and
/// `web_time::Instant` on `wasm32`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollMotion<K, S = ()> {
    /// Keyed visual start position.
    pub from: ListPosition<K, S>,
    /// Keyed destination.
    pub target: ScrollTarget<K, S>,
    /// Timing curve.
    pub easing: Easing,
    /// Total duration; zero completes immediately (reduce-motion).
    pub duration: Duration,
    /// Caller-supplied start time.
    pub started: Instant,
}

impl<K: Clone + Ord, S: Clone> ScrollMotion<K, S> {
    /// Create a motion. `started` must come from the caller's time source.
    #[must_use]
    pub const fn new(
        from: ListPosition<K, S>,
        target: ScrollTarget<K, S>,
        easing: Easing,
        duration: Duration,
        started: Instant,
    ) -> Self {
        Self {
            from,
            target,
            easing,
            duration,
            started,
        }
    }

    /// Sample the motion at `now`. See [`Self::sample_elapsed`].
    #[must_use]
    pub fn sample(
        &self,
        now: Instant,
        index: &MeasuredListIndex<K>,
        geometry: ListGeometry,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<MotionSample> {
        self.sample_elapsed(self.elapsed(now), index, geometry, sources)
    }

    /// Sample the motion `elapsed` after its start.
    ///
    /// Both endpoints are re-resolved against `index` and `geometry`. The top
    /// offset is `from + (target - from) * progress`, with the eased progress
    /// quantized to 1/65536 and applied in integer arithmetic, so it moves
    /// monotonically toward the target. At or after the duration the sample is
    /// exactly the target and `done` is true. A start anchor whose key is no
    /// longer indexed starts at the target. Returns `None` when a target key is
    /// not indexed.
    #[must_use]
    pub fn sample_elapsed(
        &self,
        elapsed: Duration,
        index: &MeasuredListIndex<K>,
        geometry: ListGeometry,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<MotionSample> {
        let (from, to) = self.endpoints(index, geometry, sources)?;
        if elapsed >= self.duration {
            return Some(MotionSample {
                top_offset: to,
                done: true,
            });
        }
        let progress = quantize(self.easing.progress(elapsed, self.duration));
        Some(MotionSample {
            top_offset: interpolate(from, to, progress),
            done: false,
        })
    }

    /// The visual position at `now` as a keyed position: the sampled top as an
    /// anchor ([`position_at_offset`]) before completion, and
    /// [`ScrollTarget::final_position`] once done. Use it to commit a frame or
    /// to materialize an interrupted motion where it currently is. Returns
    /// `None` when a target key is not indexed.
    #[must_use]
    pub fn position_at(
        &self,
        now: Instant,
        index: &MeasuredListIndex<K>,
        geometry: ListGeometry,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<ListPosition<K, S>> {
        let sample = self.sample(now, index, geometry, sources)?;
        if sample.done {
            let (from, _) = self.endpoints(index, geometry, sources)?;
            self.target.final_position(index, from, geometry, sources)
        } else {
            Some(position_at_offset(
                index,
                sample.top_offset,
                geometry,
                sources,
            ))
        }
    }

    /// Whether the motion has reached its deadline at `now`.
    #[must_use]
    pub fn is_done(&self, now: Instant) -> bool {
        self.elapsed(now) >= self.duration
    }

    /// Time at which the motion completes. Durations whose deadline cannot be
    /// represented saturate to a far-future horizon.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.started
            .checked_add(self.duration)
            .or_else(|| self.started.checked_add(HORIZON))
            .unwrap_or(self.started)
    }

    /// Instant at which the next frame should be sampled after `now`: one
    /// `frame_interval` later, but never after the deadline. Returns `None`
    /// once the motion is done. A zero `frame_interval` schedules only the
    /// deadline.
    #[must_use]
    pub fn next_frame(&self, now: Instant, frame_interval: Duration) -> Option<Instant> {
        if self.is_done(now) {
            return None;
        }
        let deadline = self.deadline();
        if frame_interval.is_zero() {
            return Some(deadline);
        }
        Some(
            now.checked_add(frame_interval)
                .map_or(deadline, |next| next.min(deadline)),
        )
    }

    fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }

    /// Current start and target top offsets.
    fn endpoints(
        &self,
        index: &MeasuredListIndex<K>,
        geometry: ListGeometry,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<(u64, u64)> {
        let from = resolve_position(index, &self.from, geometry, sources)
            .map(|resolved| resolved.top_offset);
        // A vanished start anchor starts at the target (resolved from row 0
        // for start-relative policies).
        let to = self
            .target
            .resolve_top(index, from.unwrap_or(0), geometry, sources)?;
        Some((from.unwrap_or(to), to))
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "progress is clamped to 0..=1, so the scaled value lies in 0..=PROGRESS_SCALE"
)]
fn quantize(progress: f64) -> u64 {
    let progress = if progress.is_nan() {
        0.0
    } else {
        progress.clamp(0.0, 1.0)
    };
    ((progress * PROGRESS_SCALE as f64).round() as u64).min(PROGRESS_SCALE)
}

/// `from + (to - from) * progress / PROGRESS_SCALE`, rounded toward `from`.
fn interpolate(from: u64, to: u64, progress: u64) -> u64 {
    let scale = |distance: u64| {
        u64::try_from(u128::from(distance) * u128::from(progress) / u128::from(PROGRESS_SCALE))
            .unwrap_or(distance)
    };
    if to >= from {
        from + scale(to - from)
    } else {
        from - scale(from - to)
    }
}

#[cfg(test)]
mod tests;
