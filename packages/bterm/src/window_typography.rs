//! Window-owned typography policy and bounded, debounced local preference IO.
//! Target state is not committed presentation or interaction authority.
use std::time::{Duration, Instant};

use anyhow::Result;
use bterm_native_platform::input::InputEvent;
use bterm_native_renderer::text::RendererTypography;

use crate::{
    configuration::NativeConfiguration,
    typography::TypographyConfiguration,
    zoom::{ScopedZoomAction, ZoomController, ZoomInputMatcher, ZoomScope},
    zoom_preferences::{PreferenceStore, ZoomPreferences},
};

pub(crate) struct WindowTypography {
    pub(crate) settings: TypographyConfiguration,
    controller: ZoomController,
    matcher: ZoomInputMatcher,
    store: Option<PreferenceStore>,
    remember: bool,
    pending: Option<(Instant, f64)>,
    saving: Option<(std::sync::mpsc::Receiver<Result<()>>, f64)>,
    retries: u8,
}

impl WindowTypography {
    pub(crate) fn new(native: NativeConfiguration, store: Option<PreferenceStore>) -> Result<Self> {
        native.typography.validate()?;
        let remember = native.zoom.remember;
        let (store, remembered) = match store {
            Some(store) if remember => match store.load() {
                Ok(saved) => (Some(store), saved.map(|saved| saved.target_scale)),
                Err(error) => {
                    eprintln!("bterm: cannot restore zoom preferences (saving disabled): {error}");
                    (None, None)
                }
            },
            Some(store) => (Some(store), None),
            None => (None, None),
        };
        Ok(Self {
            settings: native.typography,
            matcher: ZoomInputMatcher::new(&native.zoom)?,
            controller: ZoomController::new(native.zoom, remembered)?,
            store,
            remember,
            pending: None,
            saving: None,
            retries: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn input(&mut self, input: &InputEvent, now: Instant) -> Result<bool> {
        Ok(self.scoped_input(input, now)?.is_some())
    }

    /// Apply window actions here; return pane actions unchanged to the local-view
    /// controller. Matching (including held-key ownership) happens exactly once.
    pub(crate) fn scoped_input(
        &mut self,
        input: &InputEvent,
        now: Instant,
    ) -> Result<Option<ScopedZoomAction>> {
        let Some(action) = self.matcher.input(input) else {
            return Ok(None);
        };
        if action.scope == ZoomScope::Pane {
            return Ok(Some(action));
        }
        if self
            .controller
            .apply(action.action, f64::from(self.settings.size), now)?
            && self.remember
            && self.store.is_some()
        {
            self.retries = 0;
            self.pending = Some((
                now + Duration::from_millis(250),
                self.controller.target_scale(),
            ));
        }
        Ok(Some(action))
    }

    pub(crate) fn reconfigure(
        &mut self,
        native: NativeConfiguration,
        now: Instant,
        dpi_scale: f32,
        validate: impl FnOnce(&RendererTypography) -> Result<()>,
    ) -> Result<()> {
        native.typography.validate()?;
        native.zoom.validate()?;
        let old_controller = self.controller.clone();
        let old_settings = self.settings.clone();
        self.controller.reconfigure(native.zoom.clone(), now)?;
        self.settings = native.typography;
        // No renderer or input state is published until the full resolved
        // candidate has passed the renderer's transactional validation.
        if let Err(error) = validate(&self.resolved(now, dpi_scale)) {
            self.controller = old_controller;
            self.settings = old_settings;
            return Err(error);
        }
        self.matcher.reconfigure(&native.zoom)?;
        self.remember = native.zoom.remember;
        if !self.remember {
            self.pending = None;
        } else if self.store.is_some() {
            self.pending = Some((
                now + Duration::from_millis(250),
                self.controller.target_scale(),
            ));
        }
        Ok(())
    }

    pub(crate) fn reset_input(&mut self) {
        self.matcher.reset();
    }

    pub(crate) fn resolved(&self, now: Instant, dpi_scale: f32) -> RendererTypography {
        // Logical and physical raster sizes are independently bounded. DPI is
        // not user zoom; preserve the requested target when a display caps it.
        let lower = 1.0_f64.max(1.0 / f64::from(dpi_scale));
        let upper = 256.0_f64.min(256.0 / f64::from(dpi_scale)).min(
            if self.settings.letter_spacing > 0.0 {
                f64::from(self.settings.size) * 128.0 / f64::from(self.settings.letter_spacing)
            } else {
                f64::INFINITY
            },
        );
        RendererTypography {
            font_size: (f64::from(self.settings.size) * self.controller.sample(now))
                .clamp(lower, upper) as f32,
            family: self.settings.family.clone(),
            fallback: self.settings.fallback.clone(),
            line_height: self.settings.line_height,
            letter_spacing: self.settings.letter_spacing
                * ((f64::from(self.settings.size) * self.controller.sample(now))
                    .clamp(lower, upper)
                    / f64::from(self.settings.size)) as f32,
        }
    }

    pub(crate) fn animating(&self, now: Instant) -> bool {
        self.controller.needs_frame(now)
    }

    fn record_save(&mut self, result: Result<()>, target: f64, now: Instant) {
        if let Err(error) = result {
            eprintln!("bterm: cannot save zoom preference: {error}");
            // Never let an old failed save overwrite a newer window target.
            if self.remember && self.pending.is_none() && self.retries < 3 {
                self.retries += 1;
                self.pending = Some((
                    now + Duration::from_millis(100 * u64::from(self.retries)),
                    target,
                ));
            }
        } else {
            self.retries = 0;
        }
    }

    fn start_save(&mut self, now: Instant) {
        if self.saving.is_some() {
            return;
        }
        if let (Some(store), Some((_, target_scale))) = (self.store.clone(), self.pending.take()) {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            match std::thread::Builder::new()
                .name("bterm-zoom-preference".into())
                .spawn(move || {
                    let _ = sender.send(store.save(ZoomPreferences { target_scale }));
                }) {
                Ok(_) => self.saving = Some((receiver, target_scale)),
                Err(error) => self.record_save(Err(error.into()), target_scale, now),
            }
        }
    }

    pub(crate) fn poll_save(&mut self, now: Instant) {
        if let Some((receiver, target)) = self.saving.take() {
            match receiver.try_recv() {
                Ok(result) => self.record_save(result, target, now),
                Err(std::sync::mpsc::TryRecvError::Empty) => self.saving = Some((receiver, target)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.record_save(
                    Err(anyhow::anyhow!("zoom preference worker disconnected")),
                    target,
                    now,
                ),
            }
        }
        if self.pending.is_some_and(|(due, _)| now >= due) {
            self.start_save(now);
        }
    }

    pub(crate) fn save_pending(&self) -> bool {
        self.pending.is_some() || self.saving.is_some()
    }

    pub(crate) fn flush(&mut self) {
        // Filesystem IO is never performed on the event thread. A stalled disk
        // must not make closing a window an unbounded join of a blocking task.
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            if self.saving.is_none() {
                self.start_save(Instant::now());
            }
            let Some((receiver, target)) = self.saving.take() else {
                break;
            };
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(result) => self.record_save(result, target, Instant::now()),
                Err(_) => {
                    eprintln!(
                        "bterm: zoom preference flush exceeded 200ms; latest target may not be retained"
                    );
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zoom::{ZoomAction, ZoomConfiguration};

    fn native() -> NativeConfiguration {
        let mut native = NativeConfiguration::default();
        native.typography.size = 16.0;
        native.typography.letter_spacing = 0.5;
        native.zoom.animation.enabled = false;
        native
    }

    #[test]
    fn zoom_scales_spacing_and_dpi_independently() {
        let mut window = WindowTypography::new(native(), None).unwrap();
        let now = Instant::now();
        window
            .controller
            .apply(ZoomAction::SetScale(2.0), 16.0, now)
            .unwrap();
        let first = window.resolved(now, 1.0);
        let retina = window.resolved(now, 2.0);
        assert_eq!(first.font_size, 32.0);
        assert_eq!(first.letter_spacing, 1.0);
        assert_eq!(first, retina);
    }

    #[test]
    fn extreme_requests_preserve_target_but_cap_logical_and_physical_size() {
        let mut settings = native();
        settings.zoom.max_scale = 1000.0;
        let mut window = WindowTypography::new(settings, None).unwrap();
        let now = Instant::now();
        window
            .controller
            .apply(ZoomAction::SetScale(1000.0), 16.0, now)
            .unwrap();
        assert_eq!(window.resolved(now, 1.0).font_size, 256.0);
        assert_eq!(window.resolved(now, 2.0).font_size, 128.0);
        assert_eq!(window.controller.target_scale(), 1000.0);
        window
            .controller
            .apply(ZoomAction::SetScale(0.5), 16.0, now)
            .unwrap();
        assert_eq!(window.resolved(now, 2.0).font_size, 8.0);
    }

    #[test]
    fn pane_scoped_input_is_consumed_without_changing_window_scale() {
        use crate::zoom::{ScopedZoomAction, ZoomScope};
        use bterm_native_platform::input::{InputEvent, Modifiers, ScrollDelta};
        let mut policy = native();
        policy.zoom.wheel.scope = ZoomScope::Pane;
        let mut window = WindowTypography::new(policy, None).unwrap();
        let now = Instant::now();
        let before = window.resolved(now, 1.0);
        let event = InputEvent::Scroll {
            delta: ScrollDelta::Lines { x: 0.0, y: 1.0 },
            modifiers: Modifiers {
                super_key: cfg!(target_os = "macos"),
                control: !cfg!(target_os = "macos"),
                ..Modifiers::default()
            },
        };
        assert_eq!(
            window.scoped_input(&event, now).unwrap(),
            Some(ScopedZoomAction {
                scope: ZoomScope::Pane,
                action: ZoomAction::Steps(0.1),
            })
        );
        assert_eq!(window.resolved(now, 1.0), before);
        assert!(!window.save_pending());
    }

    #[test]
    fn failed_native_reload_keeps_old_typography_and_target() {
        let mut window = WindowTypography::new(native(), None).unwrap();
        let now = Instant::now();
        window
            .controller
            .apply(ZoomAction::SetScale(1.5), 16.0, now)
            .unwrap();
        let before = window.resolved(now, 2.0);
        let mut candidate = native();
        candidate.typography.size = 22.0;
        assert!(
            window
                .reconfigure(candidate, now, 2.0, |_| {
                    anyhow::bail!("injected raster preparation failure")
                })
                .is_err()
        );
        assert_eq!(window.resolved(now, 2.0), before);
        assert_eq!(window.controller.target_scale(), 1.5);
    }

    #[test]
    fn native_reload_preserves_zoom_and_updates_base_and_spacing() {
        let mut window = WindowTypography::new(native(), None).unwrap();
        let now = Instant::now();
        window
            .controller
            .apply(ZoomAction::SetScale(1.5), 16.0, now)
            .unwrap();
        let mut candidate = native();
        candidate.typography.size = 20.0;
        window.reconfigure(candidate, now, 2.0, |_| Ok(())).unwrap();
        assert_eq!(window.resolved(now, 2.0).font_size, 30.0);
        assert_eq!(window.resolved(now, 2.0).letter_spacing, 0.75);
    }

    #[test]
    fn failed_save_retries_are_bounded_and_do_not_replace_newer_targets() {
        let mut window = WindowTypography::new(native(), None).unwrap();
        let now = Instant::now();
        for _ in 0..3 {
            window.record_save(Err(anyhow::anyhow!("contended lock")), 1.5, now);
            assert_eq!(window.pending.take().unwrap().1, 1.5);
        }
        window.record_save(Err(anyhow::anyhow!("contended lock")), 1.5, now);
        assert!(window.pending.is_none());
        window.pending = Some((now, 2.0));
        window.record_save(Err(anyhow::anyhow!("old failed save")), 1.5, now);
        assert_eq!(window.pending.unwrap().1, 2.0);
        window.record_save(Ok(()), 1.5, now);
        assert_eq!(window.retries, 0);
    }

    #[test]
    fn pending_save_flush_has_a_deadline() {
        let mut window = WindowTypography::new(native(), None).unwrap();
        let (_sender, receiver) = std::sync::mpsc::sync_channel(1);
        window.saving = Some((receiver, 1.5));
        let now = Instant::now();
        window.flush();
        assert!(now.elapsed() < Duration::from_secs(2));
        assert!(window.saving.is_none());
    }

    #[test]
    fn saved_window_target_restores_new_windows_without_mutating_existing_ones() {
        use crate::configuration::{ConfigOptions, SourcePolicy};
        use crate::runtime_paths::BtermPaths;
        let root = tempfile::tempdir().unwrap();
        let configuration = ConfigOptions {
            policy: SourcePolicy::Explicit,
            ..Default::default()
        }
        .capture_selected_with_environment(
            BtermPaths::from_root(root.path().to_path_buf()).unwrap(),
            root.path().to_path_buf(),
            std::collections::BTreeMap::new(),
        )
        .unwrap()
        .resolve()
        .unwrap();
        let store = PreferenceStore::for_configuration(&configuration, "test-view").unwrap();
        let mut first = WindowTypography::new(native(), Some(store.clone())).unwrap();
        let second = WindowTypography::new(native(), Some(store.clone())).unwrap();
        let now = Instant::now();
        assert!(
            first
                .input(
                    &InputEvent::Key(bterm_native_platform::input::KeyEvent {
                        physical: bterm_native_platform::input::PhysicalKey::Code("Equal".into()),
                        logical: bterm_native_platform::input::LogicalKey::Character("=".into()),
                        phase: bterm_native_platform::input::KeyPhase::Press,
                        modifiers: bterm_native_platform::input::Modifiers {
                            super_key: cfg!(target_os = "macos"),
                            control: !cfg!(target_os = "macos"),
                            ..Default::default()
                        },
                        text: None,
                    }),
                    now
                )
                .unwrap()
        );
        assert_eq!(first.controller.target_scale(), 1.1);
        assert_eq!(second.controller.target_scale(), 1.0);
        first.flush();
        assert_eq!(store.load().unwrap().unwrap().target_scale, 1.1);
        let restored = WindowTypography::new(native(), Some(store)).unwrap();
        assert_eq!(restored.controller.target_scale(), 1.1);
        assert_eq!(second.controller.target_scale(), 1.0);
    }

    #[test]
    fn disabled_remember_never_opens_a_store() {
        let mut settings = native();
        settings.zoom = ZoomConfiguration {
            remember: false,
            ..Default::default()
        };
        let window = WindowTypography::new(settings, None).unwrap();
        assert!(!window.save_pending());
        assert!(window.store.is_none());
    }
}
