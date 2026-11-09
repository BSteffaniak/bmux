//! Resident windows over long, page-loaded keyed sequences.
//!
//! A [`ResidentWindow`] models the contiguous run of keyed items that is
//! currently loaded ("resident") out of a longer ordered sequence that the
//! caller loads in pages. It is pure, deterministic bookkeeping: it performs
//! no I/O, owns no timers, and never measures or paints. The caller executes
//! the page and window requests it reports, then feeds the results back.
//!
//! The window owns only generic mechanics:
//!
//! * resident keys in order, with an optional caller-defined weight per item
//!   (for example the number of underlying records an item represents);
//! * per-edge availability (more available or exhausted) and in-flight request
//!   tracking, from which a [`BoundaryStatus`] is derived;
//! * boundary demands driven by measured rows beyond the viewport edges and a
//!   caller-supplied [`PrefetchPolicy`], with at most one in-flight request per
//!   side and token fencing so stale or cancelled results are rejected;
//! * window replacement ([`JumpTarget`]) that supersedes in-flight pages;
//! * eviction plans that never remove the anchor, pinned keys, or the
//!   protected band around the viewport, returned as keyed
//!   [`WindowSplice`] operations the caller applies to its list and index;
//! * a scrollbar [`PositionEstimate`] over the whole sequence.
//!
//! Policy stays with the caller: which keys are pinned, when eviction may run,
//! how pages map to storage cursors, and how page targets are expressed.
//!
//! # Exact rows and estimates
//!
//! Every row count this module consumes for resident items (edge rows, item
//! rows for the protected band, resident rows for estimates) is exact,
//! measured by the caller's list. The only estimated quantity is
//! [`PositionEstimate`], which extrapolates rows for content that is not
//! resident at all from caller-supplied [`PositionHints`] and a rows-per-unit
//! density. It is scrollbar presentation geometry only and must never drive
//! navigation correctness.
//!
//! # Example
//!
//! ```
//! use bmux_tui_components::resident_window::{
//!     EdgeAvailability, EdgeRows, JumpTarget, PageResult, PositionHints, PrefetchPolicy,
//!     ReplaceResult, ResidentItem, ResidentWindow, SideDemand,
//! };
//!
//! let mut window = ResidentWindow::<u32>::new();
//! // Load the tail of the sequence first.
//! let jump = window.request_replace::<()>(JumpTarget::Tail);
//! window
//!     .apply_replace(
//!         jump.token,
//!         ReplaceResult {
//!             items: (90..100).map(ResidentItem::new).collect(),
//!             before: EdgeAvailability::MoreAvailable,
//!             after: EdgeAvailability::Exhausted,
//!             hints: PositionHints::known(90, 0),
//!         },
//!     )
//!     .unwrap();
//!
//! // The viewport is 5 measured rows away from the oldest resident row.
//! let policy = PrefetchPolicy::symmetric(20);
//! let demands = window.poll_demands(EdgeRows { before: 5, after: 0 }, &policy);
//! let SideDemand::Requested(request) = demands.before else {
//!     panic!("expected an older page request");
//! };
//! assert_eq!(request.edge_key, 90);
//!
//! // The caller loads the page and feeds it back.
//! let applied = window
//!     .apply_page(
//!         request.token,
//!         PageResult::new((80..90).map(ResidentItem::new), EdgeAvailability::MoreAvailable),
//!     )
//!     .unwrap();
//! assert_eq!(applied.splice.unwrap().at, 0);
//! assert_eq!(window.first_key(), Some(&80));
//! ```

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::ops::Range;

/// One end of a resident window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WindowSide {
    /// The end holding the earliest resident item (index 0).
    Before,
    /// The end holding the latest resident item.
    After,
}

impl WindowSide {
    /// Return the other side.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Before => Self::After,
            Self::After => Self::Before,
        }
    }
}

/// Whether more of the sequence exists beyond one window edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EdgeAvailability {
    /// More items exist beyond this edge (or it is not yet known that none do).
    #[default]
    MoreAvailable,
    /// The edge is the true start or end of the sequence.
    Exhausted,
}

/// Presentation status of one window edge, derived from availability and
/// in-flight requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoundaryStatus {
    /// More items exist and no request for them is in flight.
    MoreAvailable,
    /// A page or window request affecting this edge is in flight.
    Loading,
    /// The edge is the true start or end of the sequence.
    Exhausted,
}

/// Caller-visible identity of one page or window request.
///
/// Tokens are unique within one [`ResidentWindow`] and strictly increasing in
/// issue order. A result is accepted only while its token is the current
/// in-flight token for its slot; cancelled and superseded tokens are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestToken(u64);

impl RequestToken {
    /// Return the raw token value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One resident item: a stable key plus a caller-defined weight.
///
/// Weight is an opaque, caller-chosen unit (items, records, bytes) used by
/// eviction budgets, ceilings, protected-band caps, and position hints. Use
/// [`ResidentItem::new`] for a weight of one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResidentItem<K> {
    /// Stable item key.
    pub key: K,
    /// Caller-defined weight of the item.
    pub weight: u64,
}

impl<K> ResidentItem<K> {
    /// Create an item with weight one.
    #[must_use]
    pub const fn new(key: K) -> Self {
        Self { key, weight: 1 }
    }

    /// Create an item with an explicit weight.
    #[must_use]
    pub const fn weighted(key: K, weight: u64) -> Self {
        Self { key, weight }
    }
}

/// Measured resident rows beyond each viewport edge.
///
/// `before` is the number of resident rows above the viewport top and `after`
/// the number of resident rows below the viewport bottom, both taken from the
/// caller's exact list geometry. Overscroll space past the last item counts as
/// zero rows after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EdgeRows {
    /// Resident rows above the viewport top.
    pub before: u64,
    /// Resident rows below the viewport bottom.
    pub after: u64,
}

/// Resident weight ceiling that suppresses new page requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WeightCeiling {
    /// Maximum resident weight the window may reach.
    pub max_weight: u64,
    /// Weight reserved for each in-flight or newly issued page.
    pub page_reserve: u64,
}

/// What a newly issued request does to an in-flight request on the other side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ReversalPolicy {
    /// Both sides may have one request in flight at the same time.
    #[default]
    KeepInFlight,
    /// Issuing a request on one side cancels the other side's in-flight
    /// request when that side no longer needs rows. The cancelled token is
    /// reported as [`SideDemand::Cancelled`] and its result is rejected.
    CancelOpposite,
}

/// Prefetch distances and request limits for boundary demands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PrefetchPolicy {
    /// A side needs rows when fewer than this many resident rows remain above
    /// the viewport top. Zero disables prefetch on this side.
    pub before_rows: u64,
    /// A side needs rows when fewer than this many resident rows remain below
    /// the viewport bottom. Zero disables prefetch on this side.
    pub after_rows: u64,
    /// Optional resident weight ceiling.
    pub ceiling: Option<WeightCeiling>,
    /// Behavior for an in-flight request on the opposite side.
    pub reversal: ReversalPolicy,
}

impl PrefetchPolicy {
    /// Create a policy with the same prefetch distance on both sides.
    #[must_use]
    pub const fn symmetric(rows: u64) -> Self {
        Self {
            before_rows: rows,
            after_rows: rows,
            ceiling: None,
            reversal: ReversalPolicy::KeepInFlight,
        }
    }

    /// Return the policy with a resident weight ceiling.
    #[must_use]
    pub const fn ceiling(mut self, ceiling: WeightCeiling) -> Self {
        self.ceiling = Some(ceiling);
        self
    }

    /// Return the policy with a reversal behavior.
    #[must_use]
    pub const fn reversal(mut self, reversal: ReversalPolicy) -> Self {
        self.reversal = reversal;
        self
    }

    /// Return the prefetch distance for one side.
    #[must_use]
    pub const fn distance(&self, side: WindowSide) -> u64 {
        match side {
            WindowSide::Before => self.before_rows,
            WindowSide::After => self.after_rows,
        }
    }
}

/// Read-only boundary needs, ignoring in-flight requests and ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BoundarySignals {
    /// The before side has more items and is within its prefetch distance.
    pub need_before: bool,
    /// The after side has more items and is within its prefetch distance.
    pub need_after: bool,
}

/// A page request for the caller to execute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageRequest<K> {
    /// Token to pass back with the result.
    pub token: RequestToken,
    /// Side the page extends.
    pub side: WindowSide,
    /// The resident key at that edge when the request was issued. The page
    /// holds the items immediately beyond it.
    pub edge_key: K,
}

/// Why a side did not issue a request although it may need rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DemandBlock {
    /// The window has no resident items; load one with
    /// [`ResidentWindow::request_replace`].
    EmptyWindow,
    /// A window replacement is in flight.
    Replacing,
    /// The resident weight ceiling leaves no room for another page.
    Ceiling,
}

/// Boundary demand outcome for one side.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SideDemand<K> {
    /// The side does not need rows.
    Idle,
    /// A new request was issued and must be executed by the caller.
    Requested(PageRequest<K>),
    /// A request for this side is already in flight.
    InFlight(RequestToken),
    /// The side is exhausted.
    Exhausted,
    /// The side needs rows but no request could be issued.
    Blocked(DemandBlock),
    /// The side's in-flight request was cancelled under
    /// [`ReversalPolicy::CancelOpposite`]; its result will be rejected.
    Cancelled(RequestToken),
}

/// Boundary demand outcomes for both sides.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BoundaryDemands<K> {
    /// Before-side outcome.
    pub before: SideDemand<K>,
    /// After-side outcome.
    pub after: SideDemand<K>,
}

impl<K> BoundaryDemands<K> {
    /// Iterate over newly issued requests.
    pub fn requests(&self) -> impl Iterator<Item = &PageRequest<K>> {
        [&self.before, &self.after]
            .into_iter()
            .filter_map(|demand| match demand {
                SideDemand::Requested(request) => Some(request),
                _ => None,
            })
    }
}

/// A keyed splice the caller applies to its list, measurement index, and any
/// per-key caches.
///
/// Splices are returned in application order. `at` is an index into the
/// caller's resident sequence as it stands just before this splice is applied:
/// replace `at..at + removed.len()` with `inserted`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WindowSplice<K> {
    /// First affected index.
    pub at: usize,
    /// Keys removed, in order.
    pub removed: Vec<K>,
    /// Keys inserted at `at`, in order.
    pub inserted: Vec<K>,
}

impl<K> WindowSplice<K> {
    /// Return the index range replaced by this splice.
    #[must_use]
    pub const fn removed_range(&self) -> Range<usize> {
        self.at..self.at + self.removed.len()
    }
}

/// A loaded page for [`ResidentWindow::apply_page`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageResult<K> {
    /// Items in sequence order (earliest first), immediately beyond the edge.
    pub items: Vec<ResidentItem<K>>,
    /// Availability beyond the page.
    pub availability: EdgeAvailability,
    /// Weight known to remain beyond the page, when the source reports it.
    pub remaining_hint: Option<u64>,
}

impl<K> PageResult<K> {
    /// Create a page result without a remaining-weight hint.
    #[must_use]
    pub fn new(
        items: impl IntoIterator<Item = ResidentItem<K>>,
        availability: EdgeAvailability,
    ) -> Self {
        Self {
            items: items.into_iter().collect(),
            availability,
            remaining_hint: None,
        }
    }

    /// Return the result with a remaining-weight hint.
    #[must_use]
    pub const fn remaining_hint(mut self, remaining: u64) -> Self {
        self.remaining_hint = Some(remaining);
        self
    }
}

/// Outcome of an accepted page result.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageApplied<K> {
    /// Side extended.
    pub side: WindowSide,
    /// Splice to apply, or `None` when the page added no new keys.
    pub splice: Option<WindowSplice<K>>,
    /// Incoming keys skipped because they were already resident or repeated.
    pub skipped_duplicates: usize,
}

/// Target of a window replacement. `P` is a caller-defined position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JumpTarget<P> {
    /// Load a window around a caller-defined position.
    Around(P),
    /// Load the start of the sequence.
    Head,
    /// Load the end of the sequence.
    Tail,
}

/// A window replacement request for the caller to execute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReplaceRequest<P> {
    /// Token to pass back with the result.
    pub token: RequestToken,
    /// Requested target.
    pub target: JumpTarget<P>,
}

/// A loaded replacement window for [`ResidentWindow::apply_replace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceResult<K> {
    /// Items in sequence order.
    pub items: Vec<ResidentItem<K>>,
    /// Availability before the first item.
    pub before: EdgeAvailability,
    /// Availability after the last item.
    pub after: EdgeAvailability,
    /// Weight outside the new window, when known.
    pub hints: PositionHints,
}

/// Outcome of an accepted replacement.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReplaceApplied<K> {
    /// Splice replacing the whole previous window.
    pub splice: WindowSplice<K>,
    /// Incoming keys skipped because they repeated an earlier key.
    pub skipped_duplicates: usize,
}

/// Why a result was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StaleReason {
    /// The token is not the current in-flight token: it was cancelled,
    /// superseded by a replacement, already applied, or never issued.
    NotInFlight,
    /// The window edge changed after the page was requested (eviction,
    /// removal, or an unsolicited insertion at that edge).
    EdgeChanged,
}

/// A rejected page or replacement result. The window is unchanged and the
/// request slot is released when the token was in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StaleResult {
    /// Rejected token.
    pub token: RequestToken,
    /// Rejection reason.
    pub reason: StaleReason,
}

impl fmt::Display for StaleResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            StaleReason::NotInFlight => {
                write!(f, "request {} is not in flight", self.token.0)
            }
            StaleReason::EdgeChanged => write!(
                f,
                "window edge changed since request {} was issued",
                self.token.0
            ),
        }
    }
}

impl std::error::Error for StaleResult {}

/// Weight outside the resident window, in the caller's weight unit.
///
/// Hints are presentation inputs for [`PositionEstimate`] only. Unknown hints
/// contribute zero estimated rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PositionHints {
    /// Weight before the first resident item.
    pub before: Option<u64>,
    /// Weight after the last resident item.
    pub after: Option<u64>,
}

impl PositionHints {
    /// Hints with both sides unknown.
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            before: None,
            after: None,
        }
    }

    /// Hints with both sides known.
    #[must_use]
    pub const fn known(before: u64, after: u64) -> Self {
        Self {
            before: Some(before),
            after: Some(after),
        }
    }

    /// Derive hints from a sequence total, the weight before the window, and
    /// the resident weight.
    #[must_use]
    pub const fn from_total(total: u64, before: u64, resident: u64) -> Self {
        Self::known(
            before,
            total.saturating_sub(before).saturating_sub(resident),
        )
    }

    const fn get(self, side: WindowSide) -> Option<u64> {
        match side {
            WindowSide::Before => self.before,
            WindowSide::After => self.after,
        }
    }

    const fn set(&mut self, side: WindowSide, value: Option<u64>) {
        match side {
            WindowSide::Before => self.before = value,
            WindowSide::After => self.after = value,
        }
    }
}

/// Exact per-item measurements used by eviction planning.
///
/// Implemented for any `Fn(&K) -> u64` returning item rows, with every key a
/// valid window start.
pub trait ItemRows<K> {
    /// Exact rows the item occupies, including any trailing inter-item gap.
    fn rows(&self, key: &K) -> u64;

    /// Whether a window may begin at this key. Eviction cuts only where the
    /// remaining window starts at a window start (before side) or where the
    /// removed range starts at one (after side).
    fn is_window_start(&self, key: &K) -> bool {
        let _ = key;
        true
    }
}

impl<K, F> ItemRows<K> for F
where
    F: Fn(&K) -> u64,
{
    fn rows(&self, key: &K) -> u64 {
        self(key)
    }
}

/// Viewport placement relative to a resident anchor item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ViewportPlacement<'a, K> {
    /// Resident anchor key. It is never evicted.
    pub anchor: &'a K,
    /// Viewport top in rows relative to the anchor item's top (negative when
    /// the viewport top lies above the anchor).
    pub top_from_anchor: i64,
    /// Viewport height in rows.
    pub height: u64,
}

/// Rows around the viewport that eviction must keep resident.
///
/// The band covers `[viewport top - before_rows, viewport bottom + after_rows)`.
/// Items between the anchor and the band and items intersecting the band are
/// protected, taken outward from the viewport nearest first until a cap is
/// reached. The anchor is always protected regardless of caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtectedBand {
    /// Rows above the viewport top.
    pub before_rows: u64,
    /// Rows below the viewport bottom.
    pub after_rows: u64,
    /// Maximum number of protected items.
    pub max_items: usize,
    /// Optional maximum total protected weight.
    pub max_weight: Option<u64>,
}

impl ProtectedBand {
    /// Create a band with the same row extent on both sides.
    #[must_use]
    pub const fn symmetric(rows: u64, max_items: usize) -> Self {
        Self {
            before_rows: rows,
            after_rows: rows,
            max_items,
            max_weight: None,
        }
    }

    /// Return the band with a protected-weight cap.
    #[must_use]
    pub const fn max_weight(mut self, max_weight: u64) -> Self {
        self.max_weight = Some(max_weight);
        self
    }
}

/// Which sides eviction may trim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EvictionSides {
    /// Trim only the side opposite the most recent page extension (the before
    /// side when nothing has been extended yet).
    #[default]
    OppositeOfLastExtension,
    /// Trim only this side.
    Only(WindowSide),
    /// Trim `first`, then the other side if still over target.
    Both {
        /// Side trimmed first.
        first: WindowSide,
    },
}

/// When and how far eviction trims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvictionBudget {
    /// Eviction runs only while total resident weight exceeds this.
    pub trigger_weight: u64,
    /// Eviction trims toward this total resident weight.
    pub target_weight: u64,
    /// Sides eviction may trim.
    pub sides: EvictionSides,
}

/// Inputs for [`ResidentWindow::plan_eviction`].
#[derive(Debug, Clone, Copy)]
pub struct EvictionInput<'a, K, R: ?Sized> {
    /// Viewport placement around a resident anchor.
    pub placement: ViewportPlacement<'a, K>,
    /// Protected band.
    pub band: ProtectedBand,
    /// Eviction budget.
    pub budget: EvictionBudget,
    /// Additional keys that must stay resident (for example motion targets or
    /// selection endpoints). Keys that are not resident are ignored.
    pub pinned: &'a [K],
    /// Exact item measurements.
    pub rows: &'a R,
}

/// Why an eviction plan could not be computed or applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvictionError {
    /// The placement anchor is not resident.
    AnchorNotResident,
    /// The window changed after the plan was computed.
    StalePlan,
}

impl fmt::Display for EvictionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnchorNotResident => f.write_str("eviction anchor is not resident"),
            Self::StalePlan => f.write_str("window changed after the eviction plan was computed"),
        }
    }
}

impl std::error::Error for EvictionError {}

/// Index ranges an eviction would remove.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EvictionPlan {
    revision: u64,
    /// Range removed from the before side (`0..n`, possibly empty).
    pub before: Range<usize>,
    /// Range removed from the after side (`n..len`, possibly empty).
    pub after: Range<usize>,
    /// Protected index range the plan never touches.
    pub protected: Range<usize>,
    /// Total weight removed.
    pub removed_weight: u64,
}

impl EvictionPlan {
    /// Whether the plan removes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.before.is_empty() && self.after.is_empty()
    }
}

/// Exponentially smoothed rows-per-weight density for position estimates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowDensity {
    value: f64,
    smoothing: f64,
}

impl Default for RowDensity {
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl RowDensity {
    /// Default smoothing factor applied to each new observation.
    pub const DEFAULT_SMOOTHING: f64 = 0.25;

    /// Create a density with an initial rows-per-weight value. Non-finite or
    /// negative values are treated as zero.
    #[must_use]
    pub const fn new(initial: f64) -> Self {
        Self {
            value: sanitize_density(initial),
            smoothing: Self::DEFAULT_SMOOTHING,
        }
    }

    /// Return the density with a smoothing factor in `0.0..=1.0` (clamped;
    /// `1.0` uses only the latest observation, `0.0` never changes).
    #[must_use]
    pub const fn smoothing(mut self, smoothing: f64) -> Self {
        self.smoothing = if smoothing.is_nan() {
            Self::DEFAULT_SMOOTHING
        } else {
            smoothing.clamp(0.0, 1.0)
        };
        self
    }

    /// Fold in an observation of `rows` exact rows for `weight` resident
    /// weight. Zero weight is ignored.
    #[allow(
        clippy::cast_precision_loss,
        reason = "density is presentation-only; precision loss above 2^53 rows is irrelevant"
    )]
    pub fn observe(&mut self, rows: u64, weight: u64) {
        if weight == 0 {
            return;
        }
        let sample = rows as f64 / weight as f64;
        self.value = sanitize_density(self.smoothing.mul_add(sample - self.value, self.value));
    }

    /// Current rows-per-weight value.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.value
    }
}

const fn sanitize_density(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

/// Estimated scrollbar geometry over the whole sequence, in rows.
///
/// `resident` is exact (measured by the caller's list); `before` and `after`
/// are extrapolated for content that is not resident. This is a presentation
/// hint for scrollbars and fraction jumps only, not an estimated item height,
/// and must never drive navigation correctness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PositionEstimate {
    /// Estimated rows before the resident window.
    pub before: u64,
    /// Exact resident rows.
    pub resident: u64,
    /// Estimated rows after the resident window.
    pub after: u64,
}

/// Where a scrollbar fraction lands relative to the resident window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FractionTarget {
    /// Before the window, this many estimated rows before its first row.
    Before {
        /// Estimated rows between the target and the window start.
        rows_before_window: u64,
    },
    /// Inside the window at this exact resident row offset.
    Resident {
        /// Resident row offset of the viewport top.
        offset: u64,
    },
    /// After the window, this many estimated rows after its last row.
    After {
        /// Estimated rows between the window end and the target.
        rows_after_window: u64,
    },
}

impl PositionEstimate {
    /// Total estimated rows.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.before
            .saturating_add(self.resident)
            .saturating_add(self.after)
    }

    /// Estimated top position of a viewport whose top lies at
    /// `resident_offset` rows into the resident window.
    #[must_use]
    pub const fn position(self, resident_offset: u64) -> u64 {
        let offset = if resident_offset < self.resident {
            resident_offset
        } else {
            self.resident
        };
        self.before.saturating_add(offset)
    }

    /// Scroll fraction in `0.0..=1.0` for a viewport of `viewport` rows whose
    /// top lies at `resident_offset`. Content no taller than the viewport
    /// yields `0.0`. Monotonic non-decreasing in `resident_offset`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "fractions are presentation-only scrollbar geometry"
    )]
    pub fn fraction(self, resident_offset: u64, viewport: u64) -> f64 {
        let max = self.total().saturating_sub(viewport);
        if max == 0 {
            return 0.0;
        }
        let position = self.position(resident_offset).min(max);
        (position as f64 / max as f64).clamp(0.0, 1.0)
    }

    /// Map a scroll fraction to a target relative to the resident window,
    /// inverting [`Self::fraction`]. Non-finite fractions map to `0.0`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "fraction is clamped to 0..=1, so the product lies in 0..=max"
    )]
    pub fn target_for_fraction(self, fraction: f64, viewport: u64) -> FractionTarget {
        let fraction = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let max = self.total().saturating_sub(viewport);
        let position = ((max as f64) * fraction).round().min(max as f64) as u64;
        if position < self.before {
            FractionTarget::Before {
                rows_before_window: self.before - position,
            }
        } else if position - self.before <= self.resident.saturating_sub(viewport)
            || self.after == 0
        {
            FractionTarget::Resident {
                offset: (position - self.before).min(self.resident.saturating_sub(viewport)),
            }
        } else {
            FractionTarget::After {
                rows_after_window: position
                    .saturating_add(viewport)
                    .saturating_sub(self.before.saturating_add(self.resident)),
            }
        }
    }

    /// Scrollbar state scaled into the scrollbar's `u16` cell domain.
    ///
    /// Uses the same logical-to-cell scaling as scroll views and exact virtual
    /// lists, so an estimate equal to the exact geometry draws the same thumb.
    #[cfg(feature = "scrollbar")]
    #[must_use]
    pub fn scrollbar_state(
        self,
        resident_offset: u64,
        viewport: u64,
    ) -> crate::scrollbar::ScrollbarState {
        let max_offset = self.total().saturating_sub(viewport);
        crate::scrollbar::scrollbar_state(
            self.total(),
            viewport,
            self.position(resident_offset).min(max_offset),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InFlightPage<K> {
    token: RequestToken,
    edge_key: K,
}

/// Resident window over a longer keyed sequence loaded in pages.
///
/// Keys are unique; insertions skip keys that are already resident. Index
/// lookups by key are `O(log n)`; edge insertions and removals are
/// `O(k log n)` for `k` affected keys; removing a key from the middle is
/// `O(n log n)`.
#[derive(Debug, Clone)]
pub struct ResidentWindow<K> {
    entries: VecDeque<ResidentItem<K>>,
    ordinals: BTreeMap<K, i64>,
    front_ordinal: i64,
    total_weight: u64,
    before: EdgeAvailability,
    after: EdgeAvailability,
    hints: PositionHints,
    in_flight_before: Option<InFlightPage<K>>,
    in_flight_after: Option<InFlightPage<K>>,
    in_flight_replace: Option<RequestToken>,
    next_token: u64,
    revision: u64,
    last_extension: Option<WindowSide>,
}

impl<K: Clone + Ord> Default for ResidentWindow<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Clone + Ord> ResidentWindow<K> {
    /// Create an empty window. Both edges report more available until a
    /// replacement result establishes the real bounds.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            ordinals: BTreeMap::new(),
            front_ordinal: 0,
            total_weight: 0,
            before: EdgeAvailability::MoreAvailable,
            after: EdgeAvailability::MoreAvailable,
            hints: PositionHints::unknown(),
            in_flight_before: None,
            in_flight_after: None,
            in_flight_replace: None,
            next_token: 1,
            revision: 0,
            last_extension: None,
        }
    }

    /// Create a window from already loaded items. Repeated keys keep their
    /// first occurrence.
    #[must_use]
    pub fn from_items(
        items: impl IntoIterator<Item = ResidentItem<K>>,
        before: EdgeAvailability,
        after: EdgeAvailability,
        hints: PositionHints,
    ) -> Self {
        let mut window = Self::new();
        window.push_back_unique(items);
        window.before = before;
        window.after = after;
        window.hints = hints;
        window
    }

    /// Number of resident items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no items are resident.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate resident keys in order.
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &K> + DoubleEndedIterator {
        self.entries.iter().map(|entry| &entry.key)
    }

    /// Iterate resident items in order.
    pub fn items(&self) -> impl ExactSizeIterator<Item = &ResidentItem<K>> + DoubleEndedIterator {
        self.entries.iter()
    }

    /// Key at `index`.
    #[must_use]
    pub fn key_at(&self, index: usize) -> Option<&K> {
        self.entries.get(index).map(|entry| &entry.key)
    }

    /// Resident index of `key`.
    #[must_use]
    pub fn index_of(&self, key: &K) -> Option<usize> {
        let ordinal = *self.ordinals.get(key)?;
        usize::try_from(ordinal - self.front_ordinal).ok()
    }

    /// Whether `key` is resident.
    #[must_use]
    pub fn contains(&self, key: &K) -> bool {
        self.ordinals.contains_key(key)
    }

    /// First resident key.
    #[must_use]
    pub fn first_key(&self) -> Option<&K> {
        self.entries.front().map(|entry| &entry.key)
    }

    /// Last resident key.
    #[must_use]
    pub fn last_key(&self) -> Option<&K> {
        self.entries.back().map(|entry| &entry.key)
    }

    /// Key at one edge.
    #[must_use]
    pub fn edge_key(&self, side: WindowSide) -> Option<&K> {
        match side {
            WindowSide::Before => self.first_key(),
            WindowSide::After => self.last_key(),
        }
    }

    /// Total resident weight.
    #[must_use]
    pub const fn total_weight(&self) -> u64 {
        self.total_weight
    }

    /// Membership revision, incremented by every change to resident keys or
    /// weights.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Side most recently extended by an accepted page.
    #[must_use]
    pub const fn last_extension(&self) -> Option<WindowSide> {
        self.last_extension
    }

    /// Availability beyond one edge.
    #[must_use]
    pub const fn availability(&self, side: WindowSide) -> EdgeAvailability {
        match side {
            WindowSide::Before => self.before,
            WindowSide::After => self.after,
        }
    }

    /// Set availability beyond one edge (for example when the caller learns
    /// the sequence grew).
    pub const fn set_availability(&mut self, side: WindowSide, availability: EdgeAvailability) {
        match side {
            WindowSide::Before => self.before = availability,
            WindowSide::After => self.after = availability,
        }
    }

    /// Presentation status of one edge. An edge is loading while a page for
    /// it or any window replacement is in flight.
    #[must_use]
    pub const fn boundary_status(&self, side: WindowSide) -> BoundaryStatus {
        if self.in_flight_replace.is_some() || self.in_flight_slot(side).is_some() {
            BoundaryStatus::Loading
        } else {
            match self.availability(side) {
                EdgeAvailability::MoreAvailable => BoundaryStatus::MoreAvailable,
                EdgeAvailability::Exhausted => BoundaryStatus::Exhausted,
            }
        }
    }

    /// Current position hints.
    #[must_use]
    pub const fn hints(&self) -> PositionHints {
        self.hints
    }

    /// Replace position hints.
    pub const fn set_hints(&mut self, hints: PositionHints) {
        self.hints = hints;
    }

    /// Token of the in-flight page for one side.
    #[must_use]
    pub fn in_flight(&self, side: WindowSide) -> Option<RequestToken> {
        self.in_flight_slot(side).map(|page| page.token)
    }

    /// Token of the in-flight window replacement.
    #[must_use]
    pub const fn replace_in_flight(&self) -> Option<RequestToken> {
        self.in_flight_replace
    }

    /// Read-only boundary needs: a side needs rows when it has more items and
    /// fewer than its prefetch distance of resident rows remain beyond the
    /// viewport edge. In-flight requests and ceilings are ignored.
    #[must_use]
    pub fn boundary_signals(&self, rows: EdgeRows, policy: &PrefetchPolicy) -> BoundarySignals {
        BoundarySignals {
            need_before: self.needs(WindowSide::Before, rows.before, policy),
            need_after: self.needs(WindowSide::After, rows.after, policy),
        }
    }

    /// Evaluate boundary demands and issue at most one page request per side.
    ///
    /// Requests are suppressed while a replacement is in flight, while the
    /// window is empty, while a request for the side is already in flight, and
    /// while the policy ceiling leaves no room. Under
    /// [`ReversalPolicy::CancelOpposite`] a newly issued request cancels the
    /// other side's in-flight request when that side no longer needs rows.
    pub fn poll_demands(&mut self, rows: EdgeRows, policy: &PrefetchPolicy) -> BoundaryDemands<K> {
        let signals = self.boundary_signals(rows, policy);
        let mut before = self.side_demand(WindowSide::Before, signals.need_before, policy);
        let mut after = self.side_demand(WindowSide::After, signals.need_after, policy);
        if policy.reversal == ReversalPolicy::CancelOpposite {
            if matches!(before, SideDemand::Requested(_))
                && !signals.need_after
                && let Some(token) = self.cancel(WindowSide::After)
            {
                after = SideDemand::Cancelled(token);
            }
            if matches!(after, SideDemand::Requested(_))
                && !signals.need_before
                && let Some(token) = self.cancel(WindowSide::Before)
            {
                before = SideDemand::Cancelled(token);
            }
        }
        BoundaryDemands { before, after }
    }

    /// Cancel the in-flight page for one side. Its result will be rejected.
    pub fn cancel(&mut self, side: WindowSide) -> Option<RequestToken> {
        self.in_flight_slot_mut(side).take().map(|page| page.token)
    }

    /// Cancel the in-flight window replacement. Its result will be rejected.
    pub const fn cancel_replace(&mut self) -> Option<RequestToken> {
        self.in_flight_replace.take()
    }

    /// Apply a page result.
    ///
    /// New keys are prepended (before side) or appended (after side) in
    /// sequence order; keys already resident or repeated within the page are
    /// skipped. The side's availability and hint are updated and its request
    /// slot is released.
    ///
    /// # Errors
    ///
    /// * [`StaleReason::NotInFlight`] if `token` is not the in-flight page
    ///   token for either side (cancelled, superseded by a replacement, already
    ///   applied, or never issued).
    /// * [`StaleReason::EdgeChanged`] if the edge key differs from the one the
    ///   request was issued against; the slot is released.
    pub fn apply_page(
        &mut self,
        token: RequestToken,
        result: PageResult<K>,
    ) -> Result<PageApplied<K>, StaleResult> {
        let side = [WindowSide::Before, WindowSide::After]
            .into_iter()
            .find(|side| self.in_flight(*side) == Some(token))
            .ok_or(StaleResult {
                token,
                reason: StaleReason::NotInFlight,
            })?;
        let Some(page) = self.in_flight_slot_mut(side).take() else {
            unreachable!("in-flight slot was just matched");
        };
        if self.edge_key(side) != Some(&page.edge_key) {
            return Err(StaleResult {
                token,
                reason: StaleReason::EdgeChanged,
            });
        }

        let incoming = result.items.len();
        let (splice, inserted_weight) = match side {
            WindowSide::Before => self.push_front_unique(result.items),
            WindowSide::After => self.push_back_unique(result.items),
        };
        let inserted = splice.as_ref().map_or(0, |splice| splice.inserted.len());
        self.set_availability(side, result.availability);
        let hint = match (result.availability, result.remaining_hint) {
            (EdgeAvailability::Exhausted, _) => Some(0),
            (EdgeAvailability::MoreAvailable, Some(remaining)) => Some(remaining),
            (EdgeAvailability::MoreAvailable, None) => self
                .hints
                .get(side)
                .map(|known| known.saturating_sub(inserted_weight)),
        };
        self.hints.set(side, hint);
        self.last_extension = Some(side);
        Ok(PageApplied {
            side,
            splice,
            skipped_duplicates: incoming - inserted,
        })
    }

    /// Request a window replacement. Supersedes any in-flight replacement and
    /// cancels in-flight pages on both sides.
    pub fn request_replace<P>(&mut self, target: JumpTarget<P>) -> ReplaceRequest<P> {
        self.in_flight_before = None;
        self.in_flight_after = None;
        let token = self.issue_token();
        self.in_flight_replace = Some(token);
        ReplaceRequest { token, target }
    }

    /// Apply a replacement result, replacing every resident key. Repeated keys
    /// keep their first occurrence.
    ///
    /// # Errors
    ///
    /// * [`StaleReason::NotInFlight`] if `token` is not the in-flight
    ///   replacement token (cancelled, superseded, already applied, or never
    ///   issued).
    pub fn apply_replace(
        &mut self,
        token: RequestToken,
        result: ReplaceResult<K>,
    ) -> Result<ReplaceApplied<K>, StaleResult> {
        if self.in_flight_replace != Some(token) {
            return Err(StaleResult {
                token,
                reason: StaleReason::NotInFlight,
            });
        }
        self.in_flight_replace = None;
        self.in_flight_before = None;
        self.in_flight_after = None;

        let removed: Vec<K> = self.entries.drain(..).map(|entry| entry.key).collect();
        self.ordinals.clear();
        self.front_ordinal = 0;
        self.total_weight = 0;
        self.revision += 1;
        let incoming = result.items.len();
        let (inserted, _) = self.push_back_unique(result.items);
        let inserted = inserted.map(|splice| splice.inserted).unwrap_or_default();
        self.before = result.before;
        self.after = result.after;
        self.hints = result.hints;
        if self.before == EdgeAvailability::Exhausted {
            self.hints.before = Some(0);
        }
        if self.after == EdgeAvailability::Exhausted {
            self.hints.after = Some(0);
        }
        self.last_extension = None;
        Ok(ReplaceApplied {
            skipped_duplicates: incoming - inserted.len(),
            splice: WindowSplice {
                at: 0,
                removed,
                inserted,
            },
        })
    }

    /// Prepend items that arrived outside page loading. Keys already resident
    /// are skipped. Availability and hints are unchanged.
    pub fn push_front(
        &mut self,
        items: impl IntoIterator<Item = ResidentItem<K>>,
    ) -> Option<WindowSplice<K>> {
        self.push_front_unique(items).0
    }

    /// Append items that arrived outside page loading (for example live items
    /// at the sequence end). Keys already resident are skipped. Availability
    /// and hints are unchanged.
    pub fn push_back(
        &mut self,
        items: impl IntoIterator<Item = ResidentItem<K>>,
    ) -> Option<WindowSplice<K>> {
        self.push_back_unique(items).0
    }

    /// Remove one resident key. Hints are unchanged because the item left the
    /// sequence rather than the window.
    pub fn remove(&mut self, key: &K) -> Option<WindowSplice<K>> {
        let index = self.index_of(key)?;
        let entry = self.entries.remove(index)?;
        self.ordinals.remove(&entry.key);
        self.total_weight -= entry.weight;
        if index == 0 {
            self.front_ordinal += 1;
        } else {
            for later in self.entries.range(index..) {
                if let Some(ordinal) = self.ordinals.get_mut(&later.key) {
                    *ordinal -= 1;
                }
            }
        }
        self.revision += 1;
        Some(WindowSplice {
            at: index,
            removed: vec![entry.key],
            inserted: Vec::new(),
        })
    }

    /// Change the weight of a resident key. Returns whether the key is
    /// resident.
    pub fn set_weight(&mut self, key: &K, weight: u64) -> bool {
        let Some(index) = self.index_of(key) else {
            return false;
        };
        let entry = &mut self.entries[index];
        self.total_weight = self.total_weight - entry.weight + weight;
        if entry.weight != weight {
            entry.weight = weight;
            self.revision += 1;
        }
        true
    }

    /// Plan an eviction without changing the window.
    ///
    /// Nothing is removed unless total weight exceeds
    /// [`EvictionBudget::trigger_weight`]. Removal then trims allowed sides
    /// toward [`EvictionBudget::target_weight`], cutting only at window starts
    /// (see [`ItemRows::is_window_start`]) and never touching the protected
    /// range: the anchor, every pinned resident key, everything between them,
    /// and the [`ProtectedBand`].
    ///
    /// # Errors
    ///
    /// * [`EvictionError::AnchorNotResident`] if the placement anchor is not
    ///   resident.
    pub fn plan_eviction<R>(
        &self,
        input: &EvictionInput<'_, K, R>,
    ) -> Result<EvictionPlan, EvictionError>
    where
        R: ItemRows<K> + ?Sized,
    {
        let anchor = self
            .index_of(input.placement.anchor)
            .ok_or(EvictionError::AnchorNotResident)?;
        let mut protected = self.protected_range(anchor, input);
        for key in input.pinned {
            if let Some(index) = self.index_of(key) {
                protected.start = protected.start.min(index);
                protected.end = protected.end.max(index + 1);
            }
        }

        let mut plan = EvictionPlan {
            revision: self.revision,
            before: 0..0,
            after: self.len()..self.len(),
            protected: protected.clone(),
            removed_weight: 0,
        };
        if self.total_weight <= input.budget.trigger_weight {
            return Ok(plan);
        }
        let mut need = self.total_weight.saturating_sub(input.budget.target_weight);
        let order: &[WindowSide] = match input.budget.sides {
            EvictionSides::OppositeOfLastExtension => match self.last_extension {
                Some(WindowSide::Before) => &[WindowSide::After],
                Some(WindowSide::After) | None => &[WindowSide::Before],
            },
            EvictionSides::Only(WindowSide::Before) => &[WindowSide::Before],
            EvictionSides::Only(WindowSide::After) => &[WindowSide::After],
            EvictionSides::Both {
                first: WindowSide::Before,
            } => &[WindowSide::Before, WindowSide::After],
            EvictionSides::Both {
                first: WindowSide::After,
            } => &[WindowSide::After, WindowSide::Before],
        };
        for side in order {
            if need == 0 {
                break;
            }
            let (range, weight) = match side {
                WindowSide::Before => self.front_cut(protected.start, need, input.rows),
                WindowSide::After => self.back_cut(protected.end, need, input.rows),
            };
            need = need.saturating_sub(weight);
            plan.removed_weight += weight;
            match side {
                WindowSide::Before => plan.before = range,
                WindowSide::After => plan.after = range,
            }
        }
        Ok(plan)
    }

    /// Apply an eviction plan computed against the current revision.
    ///
    /// Returns splices in application order (after side first, so indices of
    /// the before-side splice stay valid). Each trimmed side becomes
    /// [`EdgeAvailability::MoreAvailable`], its known hint grows by the removed
    /// weight, and its in-flight page is cancelled.
    ///
    /// # Errors
    ///
    /// * [`EvictionError::StalePlan`] if the window changed since the plan was
    ///   computed.
    pub fn apply_eviction(
        &mut self,
        plan: &EvictionPlan,
    ) -> Result<Vec<WindowSplice<K>>, EvictionError> {
        if plan.revision != self.revision
            || plan.before.start != 0
            || plan.after.end != self.len()
            || plan.before.end > plan.after.start
        {
            return Err(EvictionError::StalePlan);
        }
        let mut splices = Vec::new();
        if !plan.after.is_empty() {
            let removed = self.pop_back_n(plan.after.len());
            self.mark_trimmed(WindowSide::After, removed.1);
            splices.push(WindowSplice {
                at: plan.after.start,
                removed: removed.0,
                inserted: Vec::new(),
            });
        }
        if !plan.before.is_empty() {
            let removed = self.pop_front_n(plan.before.len());
            self.mark_trimmed(WindowSide::Before, removed.1);
            splices.push(WindowSplice {
                at: 0,
                removed: removed.0,
                inserted: Vec::new(),
            });
        }
        if !splices.is_empty() {
            self.revision += 1;
        }
        Ok(splices)
    }

    /// Position estimate for scrollbars.
    ///
    /// `resident_rows` is the exact measured height of the resident window.
    /// Rows outside it are `hint × rows_per_weight`, rounded; exhausted sides
    /// and unknown hints contribute zero. Non-finite or negative densities are
    /// treated as zero.
    #[must_use]
    pub fn position_estimate(&self, resident_rows: u64, rows_per_weight: f64) -> PositionEstimate {
        let density = sanitize_density(rows_per_weight);
        let extent = |side: WindowSide| -> u64 {
            if self.availability(side) == EdgeAvailability::Exhausted {
                return 0;
            }
            self.hints
                .get(side)
                .map_or(0, |weight| scale_rows(weight, density))
        };
        PositionEstimate {
            before: extent(WindowSide::Before),
            resident: resident_rows,
            after: extent(WindowSide::After),
        }
    }

    // ---- internals ----

    const fn in_flight_slot(&self, side: WindowSide) -> Option<&InFlightPage<K>> {
        match side {
            WindowSide::Before => self.in_flight_before.as_ref(),
            WindowSide::After => self.in_flight_after.as_ref(),
        }
    }

    const fn in_flight_slot_mut(&mut self, side: WindowSide) -> &mut Option<InFlightPage<K>> {
        match side {
            WindowSide::Before => &mut self.in_flight_before,
            WindowSide::After => &mut self.in_flight_after,
        }
    }

    const fn issue_token(&mut self) -> RequestToken {
        let token = RequestToken(self.next_token);
        self.next_token += 1;
        token
    }

    fn needs(&self, side: WindowSide, rows: u64, policy: &PrefetchPolicy) -> bool {
        self.availability(side) == EdgeAvailability::MoreAvailable && rows < policy.distance(side)
    }

    fn side_demand(
        &mut self,
        side: WindowSide,
        needs: bool,
        policy: &PrefetchPolicy,
    ) -> SideDemand<K> {
        if let Some(token) = self.in_flight(side) {
            return SideDemand::InFlight(token);
        }
        if self.availability(side) == EdgeAvailability::Exhausted {
            return SideDemand::Exhausted;
        }
        if !needs {
            return SideDemand::Idle;
        }
        if self.in_flight_replace.is_some() {
            return SideDemand::Blocked(DemandBlock::Replacing);
        }
        let Some(edge_key) = self.edge_key(side).cloned() else {
            return SideDemand::Blocked(DemandBlock::EmptyWindow);
        };
        if let Some(ceiling) = policy.ceiling {
            let pages = 1
                + u64::from(self.in_flight_before.is_some())
                + u64::from(self.in_flight_after.is_some());
            let projected = self
                .total_weight
                .saturating_add(ceiling.page_reserve.saturating_mul(pages));
            if projected > ceiling.max_weight {
                return SideDemand::Blocked(DemandBlock::Ceiling);
            }
        }
        let token = self.issue_token();
        *self.in_flight_slot_mut(side) = Some(InFlightPage {
            token,
            edge_key: edge_key.clone(),
        });
        SideDemand::Requested(PageRequest {
            token,
            side,
            edge_key,
        })
    }

    /// Filter incoming items to keys that are neither resident nor repeated.
    fn unique_items(
        &self,
        items: impl IntoIterator<Item = ResidentItem<K>>,
    ) -> Vec<ResidentItem<K>> {
        let mut seen = std::collections::BTreeSet::new();
        items
            .into_iter()
            .filter(|item| !self.ordinals.contains_key(&item.key) && seen.insert(item.key.clone()))
            .collect()
    }

    fn push_back_unique(
        &mut self,
        items: impl IntoIterator<Item = ResidentItem<K>>,
    ) -> (Option<WindowSplice<K>>, u64) {
        let items = self.unique_items(items);
        if items.is_empty() {
            return (None, 0);
        }
        let at = self.len();
        let mut weight = 0;
        let mut inserted = Vec::with_capacity(items.len());
        for item in items {
            let ordinal = self.front_ordinal + ordinal_len(self.entries.len());
            self.ordinals.insert(item.key.clone(), ordinal);
            weight += item.weight;
            inserted.push(item.key.clone());
            self.entries.push_back(item);
        }
        self.total_weight += weight;
        self.revision += 1;
        (
            Some(WindowSplice {
                at,
                removed: Vec::new(),
                inserted,
            }),
            weight,
        )
    }

    fn push_front_unique(
        &mut self,
        items: impl IntoIterator<Item = ResidentItem<K>>,
    ) -> (Option<WindowSplice<K>>, u64) {
        let items = self.unique_items(items);
        if items.is_empty() {
            return (None, 0);
        }
        let inserted: Vec<K> = items.iter().map(|item| item.key.clone()).collect();
        let mut weight = 0;
        for item in items.into_iter().rev() {
            self.front_ordinal -= 1;
            self.ordinals.insert(item.key.clone(), self.front_ordinal);
            weight += item.weight;
            self.entries.push_front(item);
        }
        self.total_weight += weight;
        self.revision += 1;
        (
            Some(WindowSplice {
                at: 0,
                removed: Vec::new(),
                inserted,
            }),
            weight,
        )
    }

    fn pop_front_n(&mut self, count: usize) -> (Vec<K>, u64) {
        let mut keys = Vec::with_capacity(count);
        let mut weight = 0;
        for entry in self.entries.drain(..count) {
            self.ordinals.remove(&entry.key);
            weight += entry.weight;
            keys.push(entry.key);
        }
        self.front_ordinal += ordinal_len(count);
        self.total_weight -= weight;
        (keys, weight)
    }

    fn pop_back_n(&mut self, count: usize) -> (Vec<K>, u64) {
        let start = self.len() - count;
        let mut keys = Vec::with_capacity(count);
        let mut weight = 0;
        for entry in self.entries.drain(start..) {
            self.ordinals.remove(&entry.key);
            weight += entry.weight;
            keys.push(entry.key);
        }
        self.total_weight -= weight;
        (keys, weight)
    }

    fn mark_trimmed(&mut self, side: WindowSide, removed_weight: u64) {
        self.set_availability(side, EdgeAvailability::MoreAvailable);
        let hint = self
            .hints
            .get(side)
            .map(|known| known.saturating_add(removed_weight));
        self.hints.set(side, hint);
        self.cancel(side);
    }

    /// Contiguous protected range around the anchor from the band and caps.
    fn protected_range<R>(&self, anchor: usize, input: &EvictionInput<'_, K, R>) -> Range<usize>
    where
        R: ItemRows<K> + ?Sized,
    {
        let rows = input.rows;
        let band = input.band;
        let top = i128::from(input.placement.top_from_anchor);
        let bottom = top + i128::from(input.placement.height);
        let band_start = top - i128::from(band.before_rows);
        let band_end = bottom + i128::from(band.after_rows);

        let mut start = anchor;
        let mut end = anchor + 1;
        let mut count = 1usize;
        let mut weight = self.entries[anchor].weight;
        // Edge of the next candidate relative to the anchor top: the bottom of
        // the item above and the top of the item below.
        let mut above_bottom: i128 = 0;
        let mut below_top: i128 = i128::from(rows.rows(&self.entries[anchor].key));

        loop {
            let above = start.checked_sub(1).and_then(|index| {
                let item_rows = i128::from(rows.rows(&self.entries[index].key));
                let item_top = above_bottom - item_rows;
                let beyond = if item_rows == 0 {
                    item_top < band_start
                } else {
                    above_bottom <= band_start
                };
                let distance = viewport_distance(item_top, above_bottom, top, bottom);
                (!beyond).then_some((index, item_top, distance))
            });
            let below = (end < self.len())
                .then(|| {
                    let item_rows = i128::from(rows.rows(&self.entries[end].key));
                    let beyond = below_top >= band_end;
                    let item_bottom = below_top + item_rows;
                    let distance = viewport_distance(below_top, item_bottom, top, bottom);
                    (!beyond).then_some((end, item_bottom, distance))
                })
                .flatten();
            let choice = match (above, below) {
                (None, None) => break,
                (Some(a), None) => (WindowSide::Before, a),
                (None, Some(b)) => (WindowSide::After, b),
                (Some(a), Some(b)) => {
                    if b.2 < a.2 {
                        (WindowSide::After, b)
                    } else {
                        (WindowSide::Before, a)
                    }
                }
            };
            let (side, (index, next_edge, _)) = choice;
            let item_weight = self.entries[index].weight;
            if count >= band.max_items
                || band
                    .max_weight
                    .is_some_and(|max| weight.saturating_add(item_weight) > max)
            {
                break;
            }
            count += 1;
            weight = weight.saturating_add(item_weight);
            match side {
                WindowSide::Before => {
                    start = index;
                    above_bottom = next_edge;
                }
                WindowSide::After => {
                    end = index + 1;
                    below_top = next_edge;
                }
            }
        }
        start..end
    }

    /// Before-side removal `0..cut` with `cut <= limit` at a window start.
    fn front_cut<R>(&self, limit: usize, need: u64, rows: &R) -> (Range<usize>, u64)
    where
        R: ItemRows<K> + ?Sized,
    {
        let mut best = (0..0, 0);
        let mut weight = 0;
        for cut in 1..=limit {
            weight += self.entries[cut - 1].weight;
            if rows.is_window_start(&self.entries[cut].key) {
                best = (0..cut, weight);
                if weight >= need {
                    break;
                }
            }
        }
        best
    }

    /// After-side removal `cut..len` with `cut >= limit` at a window start.
    fn back_cut<R>(&self, limit: usize, need: u64, rows: &R) -> (Range<usize>, u64)
    where
        R: ItemRows<K> + ?Sized,
    {
        let len = self.len();
        let mut best = (len..len, 0);
        let mut weight = 0;
        for cut in (limit.max(1)..len).rev() {
            weight += self.entries[cut].weight;
            if rows.is_window_start(&self.entries[cut].key) {
                best = (cut..len, weight);
                if weight >= need {
                    break;
                }
            }
        }
        best
    }
}

/// Protection priority of an item `[item_top, item_bottom)` relative to the
/// viewport `[top, bottom)`: zero for visible items (a zero-height item is
/// visible when `top <= item_top < bottom`), otherwise one plus the rows
/// separating it from the nearest viewport edge.
const fn viewport_distance(item_top: i128, item_bottom: i128, top: i128, bottom: i128) -> i128 {
    let visible = if item_top == item_bottom {
        top <= item_top && item_top < bottom
    } else {
        item_top < bottom && item_bottom > top
    };
    if visible {
        0
    } else if item_bottom <= top {
        top - item_bottom + 1
    } else {
        item_top - bottom + 1
    }
}

fn ordinal_len(len: usize) -> i64 {
    i64::try_from(len).expect("resident window length fits in i64")
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "estimate is presentation-only and saturates at u64::MAX"
)]
fn scale_rows(weight: u64, density: f64) -> u64 {
    let rows = (weight as f64 * density).round();
    if rows >= u64::MAX as f64 {
        u64::MAX
    } else {
        rows as u64
    }
}

#[cfg(test)]
mod tests;
