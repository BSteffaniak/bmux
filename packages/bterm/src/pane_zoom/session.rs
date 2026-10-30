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
    receiver: mpsc::Receiver<Result<Option<ZoomPreferences>>>,
}

pub(crate) enum TargetRefresh<T> {
    Ready {
        target: T,
        revision: u64,
        scale: f64,
    },
    /// The UI and CLI commit callbacks have not converged yet; retain intent.
    Pending,
    Gone,
}

struct Active<T> {
    target: T,
    in_flight: Option<u64>,
    last_sent: Option<f64>,
    restore: Option<PaneViewRestoreToken>,
}

/// A native window owns one session. Its map is bounded by PaneZoomState.
/// Supplied target tokens stay bound throughout animation; focus changes never
/// retarget an existing transition. CLI must revalidate tokens at dequeue.
pub(crate) struct PaneZoomSession<T> {
    state: PaneZoomState,
    active: BTreeMap<PaneViewBinding, Active<T>>,
    store: Option<PreferenceStore>,
    remember: bool,
    policy: ZoomConfiguration,
    raster_cap: u16,
    pending: BTreeMap<PaneViewBinding, (Instant, PaneViewRestoreToken, f64)>,
    loading: Option<RestoreLoad<T>>,
    loaded: std::collections::BTreeSet<PaneViewBinding>,
    saving: Option<mpsc::Receiver<Result<()>>>,
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
            state: PaneZoomState::new(policy)?,
            active: BTreeMap::new(),
            store,
            pending: BTreeMap::new(),
            loading: None,
            loaded: Default::default(),
            saving: None,
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
                self.loading = Some(load);
                return Ok(());
            }
            TargetRefresh::Gone => return Ok(()),
        };
        let result = match load.receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => {
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
        self.state.bind_current(load.binding.clone(), scale)?;
        self.active.insert(
            load.binding,
            Active {
                in_flight: None,
                target,
                last_sent: Some(load.current_scale),
                restore: token,
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
        self.state.bind_current(binding.clone(), current_scale)?;
        self.active
            .entry(binding.clone())
            .and_modify(|active| active.target = target.clone())
            .or_insert(Active {
                in_flight: None,
                target,
                last_sent: Some(current_scale),
                restore,
            });
        self.state.apply(&binding, action, base_size, now)?;
        // Even a no-op Reset is explicit local intent and must supersede a late
        // saved override (e.g. resetting before asynchronous restore completes).
        self.schedule_save(&binding, now);
        Ok(())
    }

    fn schedule_save(&mut self, binding: &PaneViewBinding, now: Instant) {
        if self.remember
            && self.store.is_some()
            && let Some(token) = self
                .active
                .get(binding)
                .and_then(|active| active.restore.clone())
            && let Some(target) = self.state.target_scale(binding)
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
            self.active.remove(&binding);
            self.pending.remove(&binding);
        }
        self.poll_save(now);
        Ok(())
    }

    /// One request per target's committed revision; intervening actions coalesce
    /// into local intent. Refresh exact bindings rather than resolving focus.
    pub(crate) fn tick_bound(
        &mut self,
        now: Instant,
        mut refresh: impl FnMut(&T) -> TargetRefresh<T>,
        mut send: impl FnMut(&T, f64) -> Result<()>,
    ) -> Result<()> {
        let mut retired = Vec::new();
        for (binding, active) in &mut self.active {
            let (target, revision, committed_scale) = match refresh(&active.target) {
                TargetRefresh::Ready {
                    target,
                    revision,
                    scale,
                } => (target, revision, scale),
                TargetRefresh::Pending => continue,
                TargetRefresh::Gone => {
                    retired.push(binding.clone());
                    continue;
                }
            };
            active.target = target;
            if active.in_flight == Some(revision) {
                continue;
            }
            active.in_flight = None;
            let Some(scale) = self.state.sample(binding, now) else {
                continue;
            };
            // Compare in native milli-scale units, not sub-milli animation noise.
            if (scale * 1000.0).round() == (committed_scale * 1000.0).round() {
                active.last_sent = Some(scale);
                continue;
            }
            send(&active.target, scale)?;
            active.last_sent = Some(scale);
            active.in_flight = Some(revision);
        }
        for binding in retired {
            self.state.remove(&binding);
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
            let bindings: Vec<_> = self.active.keys().cloned().collect();
            for binding in bindings {
                self.schedule_save(&binding, now);
            }
        }
        Ok(())
    }

    pub(crate) fn reconfigure(&mut self, policy: ZoomConfiguration, now: Instant) -> Result<()> {
        let policy = bounded_policy(policy)?;
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
        self.active
            .values()
            .any(|active| active.in_flight.is_some())
            || self.state.needs_frame(now)
            || self.active.iter().any(|(binding, active)| {
                self.state.sample(binding, now).is_some_and(|scale| {
                    active
                        .last_sent
                        .is_none_or(|last| last.to_bits() != scale.to_bits())
                })
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
        }
        self.active.clear();
        self.loading = None;
        self.loaded.clear();
        // Invalidated bindings must not cause later writes based on stale evidence.
        self.pending.clear();
    }

    fn poll_save(&mut self, now: Instant) {
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
        if self.saving.is_some() {
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
                self.saving = Some(receiver);
            }
            Err(error) => eprintln!("bterm: cannot start pane zoom preference worker: {error}"),
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
