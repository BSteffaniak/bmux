//! Bterm-owned animation and optional local preference IO. Targets are opaque
//! CLI-issued tokens; this module never resolves focus, hover, or source geometry.
use std::{
    collections::BTreeMap,
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::Result;

use super::{PaneViewBinding, PaneViewRestoreToken, PaneZoomState};
use crate::{
    zoom::{ZoomAction, ZoomConfiguration},
    zoom_preferences::{PreferenceStore, ZoomPreferences},
};

#[cfg(test)]
mod tests;

const MAX_WAIT: Duration = Duration::from_secs(2);
const MAX_RETRIES: u8 = 8;

pub(crate) struct PaneZoomRequest<T> {
    pub binding: PaneViewBinding,
    pub target: T,
    pub current_scale: f64,
    pub restore: Option<PaneViewRestoreToken>,
    pub action: ZoomAction,
    pub base_size: f64,
}

struct RestoreLoad<T> {
    binding: PaneViewBinding,
    target: T,
    token: PaneViewRestoreToken,
    current_scale: f64,
    started: Instant,
    receiver: mpsc::Receiver<Result<Option<ZoomPreferences>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestStatus {
    Pending,
    /// Reducer accepted intent; complete output still needs verification.
    Applied,
    Rejected,
}

pub(crate) type RequestReceipt = Box<dyn Fn() -> RequestStatus>;

pub(crate) enum TargetRefresh<T> {
    /// Caller verifies exact source and committed output identity before Ready.
    Ready {
        target: T,
        scale: f64,
    },
    /// The UI and CLI commit callbacks have not converged yet; retain intent.
    Pending,
    Gone,
}

struct Active<T> {
    target: T,
    in_flight: Option<RequestReceipt>,
    last_sent: Option<f64>,
    restore: Option<PaneViewRestoreToken>,
    waiting_since: Option<Instant>,
    retries: u8,
    stopped_reported: bool,
}

impl<T> Active<T> {
    fn runnable(&self, now: Instant) -> bool {
        self.retries < MAX_RETRIES
            && self
                .waiting_since
                .is_none_or(|since| now.saturating_duration_since(since) < MAX_WAIT)
    }
}

/// A native window owns one session. Its map is bounded by PaneZoomState.
/// Supplied target tokens stay bound throughout animation; focus changes never
/// retarget an existing transition. CLI must revalidate tokens at dequeue.
pub(crate) struct PaneZoomSession<T> {
    state: PaneZoomState,
    // Logical requested intent is never reconfigured with a raster/DPI cap.
    requested: PaneZoomState,
    active: BTreeMap<PaneViewBinding, Active<T>>,
    store: Option<PreferenceStore>,
    remember: bool,
    policy: ZoomConfiguration,
    raster_cap: u16,
    pending: BTreeMap<PaneViewBinding, (Instant, PaneViewRestoreToken, f64)>,
    loading: Option<RestoreLoad<T>>,
    loaded: std::collections::BTreeSet<PaneViewBinding>,
    saving: Option<mpsc::Receiver<Result<()>>>,
    saving_started: Option<Instant>,
    save_spawn_failures: u8,
}

fn bounded_policy(mut policy: ZoomConfiguration) -> Result<ZoomConfiguration> {
    policy.validate()?;
    // Native attach's public scale contract is 125..=8000 milli-scale.
    policy.min_scale = policy.min_scale.clamp(0.125, 8.0);
    policy.max_scale = policy.max_scale.clamp(policy.min_scale, 8.0);
    Ok(policy)
}

impl<T: Clone> PaneZoomSession<T> {
    pub(crate) fn new(policy: ZoomConfiguration, store: Option<PreferenceStore>) -> Result<Self> {
        let policy = bounded_policy(policy)?;
        Ok(Self {
            remember: policy.remember,
            policy: policy.clone(),
            raster_cap: 8000,
            requested: PaneZoomState::new(policy.clone())?,
            state: PaneZoomState::new(policy)?,
            active: BTreeMap::new(),
            store,
            pending: BTreeMap::new(),
            loading: None,
            loaded: Default::default(),
            saving: None,
            saving_started: None,
            save_spawn_failures: 0,
        })
    }

    /// Observe a committed pane before restoring; IO never blocks native output.
    pub(crate) fn observe(
        &mut self,
        binding: PaneViewBinding,
        target: T,
        current_scale: f64,
        token: PaneViewRestoreToken,
    ) -> Result<()> {
        if !self.remember
            || self.loading.is_some()
            || self.loaded.contains(&binding)
            || self.active.contains_key(&binding)
            || self.loaded.len() >= 256
        {
            return Ok(());
        }
        let Some(store) = self.store.clone() else {
            return Ok(());
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        let read_token = token.clone();
        std::thread::Builder::new()
            .name("bterm-pane-zoom-restore".into())
            .spawn(move || {
                let _ = sender.send(store.load_pane(&read_token));
            })?;
        self.loading = Some(RestoreLoad {
            binding,
            target,
            token,
            current_scale,
            started: Instant::now(),
            receiver,
        });
        Ok(())
    }

    /// Fresh committed CLI evidence must verify the original pane/source after
    /// IO. A user label or persisted value cannot satisfy this check.
    pub(crate) fn poll_restore(
        &mut self,
        mut validate: impl FnMut(&T) -> TargetRefresh<T>,
    ) -> Result<()> {
        let Some(load) = self.loading.take() else {
            return Ok(());
        };
        let target = match validate(&load.target) {
            TargetRefresh::Ready { target, .. } => target,
            TargetRefresh::Pending => {
                if load.started.elapsed() >= MAX_WAIT {
                    self.loaded.insert(load.binding);
                    eprintln!("bterm: pane zoom restore validation timed out");
                    return Ok(());
                }
                self.loading = Some(load);
                return Ok(());
            }
            TargetRefresh::Gone => return Ok(()),
        };
        let result = match load.receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => {
                if load.started.elapsed() >= MAX_WAIT {
                    self.loaded.insert(load.binding);
                    eprintln!("bterm: pane zoom restore IO timed out");
                    return Ok(());
                }
                self.loading = Some(load);
                return Ok(());
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!("pane zoom restore worker disconnected"))
            }
        };
        self.loaded.insert(load.binding.clone());
        if self.active.contains_key(&load.binding) || !self.remember {
            return Ok(());
        }
        let (scale, token) = match result {
            Ok(saved) => (
                saved.map_or(load.current_scale, |saved| saved.target_scale),
                Some(load.token),
            ),
            Err(error) => {
                eprintln!(
                    "bterm: cannot restore pane zoom preferences: {error}; pane saving disabled"
                );
                (load.current_scale, None)
            }
        };
        self.requested.bind_current(load.binding.clone(), scale)?;
        self.state.bind_current(load.binding.clone(), scale)?;
        self.active.insert(
            load.binding,
            Active {
                in_flight: None,
                target,
                last_sent: Some(load.current_scale),
                restore: token,
                waiting_since: None,
                retries: 0,
                stopped_reported: false,
            },
        );
        Ok(())
    }

    pub(crate) fn action(&mut self, request: PaneZoomRequest<T>, now: Instant) -> Result<()> {
        let PaneZoomRequest {
            binding,
            target,
            current_scale,
            restore,
            action,
            base_size,
        } = request;
        // Consumed releases must never create views or restart animation.
        if action == ZoomAction::Consume {
            return Ok(());
        }
        self.requested
            .bind_current(binding.clone(), current_scale)?;
        self.state.bind_current(binding.clone(), current_scale)?;
        self.active
            .entry(binding.clone())
            .and_modify(|active| active.target = target.clone())
            .or_insert(Active {
                in_flight: None,
                target,
                last_sent: Some(current_scale),
                restore,
                waiting_since: None,
                retries: 0,
                stopped_reported: false,
            });
        let changed = self.requested.apply(&binding, action, base_size, now)?;
        if changed && let Some(active) = self.active.get_mut(&binding) {
            active.waiting_since = None;
            active.retries = 0;
            active.stopped_reported = false;
        }
        if let Some(scale) = self.requested.target_scale(&binding) {
            self.state
                .apply(&binding, ZoomAction::SetScale(scale), base_size, now)?;
        }
        // Even a no-op Reset is explicit local intent and must supersede a late
        // saved override (e.g. resetting before asynchronous restore completes).
        self.schedule_save(&binding, now);
        Ok(())
    }

    fn schedule_save(&mut self, binding: &PaneViewBinding, now: Instant) {
        if self.remember
            && self.save_spawn_failures < MAX_RETRIES
            && self.store.is_some()
            && let Some(token) = self
                .active
                .get(binding)
                .and_then(|active| active.restore.clone())
            && let Some(target) = self.requested.target_scale(binding)
        {
            self.pending.insert(
                binding.clone(),
                (now + Duration::from_millis(250), token, target),
            );
        }
    }

    /// Returns samples needing publication. Queue admission is not presentation
    /// acceptance: failed admission leaves samples retryable. Invalidated targets
    /// must be removed by the caller, never rebound to the currently focused pane.
    #[cfg(test)]
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        mut send: impl FnMut(&T, f64) -> Result<bool>,
    ) -> Result<()> {
        let mut retired = Vec::new();
        for (binding, active) in &mut self.active {
            let Some(scale) = self.state.sample(binding, now) else {
                continue;
            };
            if active
                .last_sent
                .is_some_and(|last| last.to_bits() == scale.to_bits())
            {
                continue;
            }
            if send(&active.target, scale)? {
                active.last_sent = Some(scale);
            } else {
                retired.push(binding.clone());
            }
        }
        for binding in retired {
            self.state.remove(&binding);
            self.requested.remove(&binding);
            self.active.remove(&binding);
            self.pending.remove(&binding);
        }
        self.poll_save(now);
        Ok(())
    }

    /// Exact request receipts, not arbitrary output changes, release in-flight
    /// work. Retry exhaustion retains intent but stops polling until a changed
    /// action or policy/cap change. Never resubmit an unacknowledged request.
    pub(crate) fn tick_bound(
        &mut self,
        now: Instant,
        mut refresh: impl FnMut(&T) -> TargetRefresh<T>,
        mut send: impl FnMut(&T, f64) -> Result<RequestReceipt>,
    ) -> Result<()> {
        let mut retired = Vec::new();
        for (binding, active) in &mut self.active {
            if !active.runnable(now) && !active.stopped_reported {
                eprintln!(
                    "bterm: pane zoom stopped waiting for source/request/output; requested intent retained, change zoom or policy to retry"
                );
                active.stopped_reported = true;
            }
            let (target, committed_scale) = match refresh(&active.target) {
                TargetRefresh::Ready { target, scale } => (target, scale),
                TargetRefresh::Pending => {
                    active.waiting_since.get_or_insert(now);
                    // A queued preference has no fresh source validation. Do
                    // not admit it to IO while the binding is contested.
                    if active.runnable(now) {
                        if let Some((due, _, _)) = self.pending.get_mut(binding) {
                            *due = now + Duration::from_millis(250);
                        }
                    } else {
                        self.pending.remove(binding);
                    }
                    continue;
                }
                TargetRefresh::Gone => {
                    retired.push(binding.clone());
                    continue;
                }
            };
            active.target = target;
            if !active.runnable(now) {
                continue;
            }
            if let Some(receipt) = &active.in_flight {
                match receipt() {
                    RequestStatus::Pending => {
                        active.waiting_since.get_or_insert(now);
                        continue;
                    }
                    RequestStatus::Applied => {
                        // Reducer acceptance is not output completion. Ignore
                        // unrelated output and newer coalesced local intent.
                        if active.last_sent.is_none_or(|sent| {
                            (sent * 1000.0).round() != (committed_scale * 1000.0).round()
                        }) {
                            active.waiting_since.get_or_insert(now);
                            continue;
                        }
                        active.retries = 0;
                        active.stopped_reported = false;
                    }
                    RequestStatus::Rejected => {
                        active.retries += 1;
                        if !active.runnable(now) {
                            continue;
                        }
                    }
                }
                active.in_flight = None;
            }
            active.waiting_since = None;
            let Some(scale) = self.state.sample(binding, now) else {
                continue;
            };
            // Compare in native milli-scale units, not sub-milli animation noise.
            if (scale * 1000.0).round() == (committed_scale * 1000.0).round() {
                active.last_sent = Some(scale);
                continue;
            }
            let receipt = match send(&active.target, scale) {
                Ok(receipt) => receipt,
                Err(error) => {
                    active.retries += 1;
                    if active.retries == MAX_RETRIES {
                        eprintln!(
                            "bterm: pane zoom queue retry limit reached; retaining requested intent"
                        );
                    }
                    return Err(error);
                }
            };
            active.last_sent = Some(scale);
            active.in_flight = Some(receipt);
            active.waiting_since = Some(now);
        }
        for binding in retired {
            self.state.remove(&binding);
            self.requested.remove(&binding);
            self.active.remove(&binding);
            self.pending.remove(&binding);
            self.loaded.remove(&binding);
        }
        self.poll_save(now);
        Ok(())
    }

    pub(crate) fn set_raster_cap(&mut self, cap: u16, now: Instant) -> Result<()> {
        let cap = cap.clamp(125, 8000);
        if self.raster_cap != cap {
            let mut policy = self.policy.clone();
            policy.max_scale = policy.max_scale.min(f64::from(cap) / 1000.0);
            policy.min_scale = policy.min_scale.min(policy.max_scale);
            self.state.reconfigure(policy, now)?;
            self.raster_cap = cap;
            for binding in self.active.keys() {
                if let Some(scale) = self.requested.target_scale(binding) {
                    self.state
                        .apply(binding, ZoomAction::SetScale(scale), 1.0, now)?;
                }
            }
            for active in self.active.values_mut() {
                active.waiting_since = None;
                active.retries = 0;
                active.stopped_reported = false;
            }
        }
        Ok(())
    }

    pub(crate) fn reconfigure(&mut self, policy: ZoomConfiguration, now: Instant) -> Result<()> {
        let policy = bounded_policy(policy)?;
        self.requested.reconfigure(policy.clone(), now)?;
        self.policy = policy.clone();
        let cap = self.raster_cap;
        self.raster_cap = 0;
        self.set_raster_cap(cap, now)?;
        self.remember = policy.remember;
        if !self.remember {
            self.pending.clear();
        } else {
            let bindings: Vec<_> = self.active.keys().cloned().collect();
            for binding in bindings {
                self.schedule_save(&binding, now);
            }
        }
        Ok(())
    }

    /// Last requested pane targets that remain publishable, including a final
    /// sample rejected by queue admission after its animation elapsed.
    pub(crate) fn needs_tick(&self, now: Instant) -> bool {
        self.active.iter().any(|(binding, active)| {
            active.runnable(now)
                && (self.state.view_needs_frame(binding, now)
                    || active.in_flight.is_some()
                    || active.waiting_since.is_some()
                    || self.state.sample(binding, now).is_some_and(|scale| {
                        active
                            .last_sent
                            .is_none_or(|last| (last * 1000.0).round() != (scale * 1000.0).round())
                    }))
        })
    }

    #[cfg(test)]
    pub(crate) fn animating(&self, now: Instant) -> bool {
        self.state.needs_frame(now)
    }
    pub(crate) fn save_pending(&self) -> bool {
        self.loading.is_some() || !self.pending.is_empty() || self.saving.is_some()
    }

    pub(crate) fn clear(&mut self) {
        for binding in self.active.keys() {
            self.state.remove(binding);
            self.requested.remove(binding);
        }
        self.active.clear();
        self.loading = None;
        self.loaded.clear();
        // Cancel IO admission, not an already admitted immutable preference
        // write. Such a write only addresses its original verified durable key;
        // it cannot mutate a new source, live target, or another view's state.
        self.pending.clear();
    }

    fn poll_save(&mut self, now: Instant) {
        if let Some(started) = self.saving_started
            && now.saturating_duration_since(started) >= MAX_WAIT
            && self.saving.is_some()
        {
            // Do not spawn a competing writer if an OS operation is stalled.
            // Dropping the receiver does not cancel its immutable write.
            self.saving = None;
            self.saving_started = None;
            self.save_spawn_failures = MAX_RETRIES;
            self.pending.clear();
            eprintln!("bterm: pane zoom save timed out; further saving disabled");
        }
        if let Some(receiver) = self.saving.take() {
            match receiver.try_recv() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("bterm: cannot save pane zoom preferences: {error}"),
                Err(mpsc::TryRecvError::Empty) => self.saving = Some(receiver),
                Err(mpsc::TryRecvError::Disconnected) => {
                    eprintln!("bterm: pane zoom preference worker disconnected")
                }
            }
        }
        if self.saving.is_some() || self.save_spawn_failures >= MAX_RETRIES {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let due: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, (due, _, _))| now >= *due)
            .map(|(binding, (_, token, scale))| (binding.clone(), token.clone(), *scale))
            .collect();
        if due.is_empty() {
            return;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let values: Vec<_> = due
            .iter()
            .map(|(_, token, scale)| (token.clone(), *scale))
            .collect();
        match std::thread::Builder::new()
            .name("bterm-pane-zoom-preference".into())
            .spawn(move || {
                let result = values.into_iter().try_for_each(|(token, target_scale)| {
                    store.save_pane(&token, ZoomPreferences { target_scale })
                });
                let _ = sender.send(result);
            }) {
            Ok(_) => {
                for (binding, _, _) in due {
                    self.pending.remove(&binding);
                }
                self.saving_started = Some(now);
                self.saving = Some(receiver);
            }
            Err(error) => {
                self.save_spawn_failures += 1;
                if self.save_spawn_failures >= MAX_RETRIES {
                    self.pending.clear();
                }
                eprintln!("bterm: cannot start pane zoom preference worker: {error}");
            }
        }
    }

    pub(crate) fn flush(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(200);
        for (due, _, _) in self.pending.values_mut() {
            *due = Instant::now();
        }
        loop {
            self.poll_save(Instant::now());
            let Some(receiver) = self.saving.take() else {
                break;
            };
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("bterm: cannot flush pane zoom preferences: {error}"),
                Err(_) => {
                    eprintln!("bterm: pane zoom flush exceeded 200ms; target may not be retained");
                    break;
                }
            }
            if self.pending.is_empty() || Instant::now() >= deadline {
                break;
            }
        }
    }
}
