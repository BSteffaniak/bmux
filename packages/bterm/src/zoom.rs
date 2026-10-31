//! Scoped zoom policy. This module never commits presentation geometry.
//! The application must paint and hit-test using the same successfully presented sample.

use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use bterm_native_platform::input::{InputEvent, KeyPhase, LogicalKey, Modifiers, PhysicalKey};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZoomConfiguration {
    /// Multiplicative ratio per step, not an integer font-size increment.
    pub step: f64,
    pub min_scale: f64,
    pub max_scale: f64,
    pub remember: bool,
    pub animation: ZoomAnimationConfiguration,
    pub bindings: Vec<ZoomBinding>,
    pub wheel: ZoomWheelConfiguration,
}

impl Default for ZoomConfiguration {
    fn default() -> Self {
        Self {
            step: 1.1,
            min_scale: 0.5,
            max_scale: 3.0,
            remember: true,
            animation: ZoomAnimationConfiguration::default(),
            bindings: vec![
                ZoomBinding::primary("=", ZoomAction::In),
                ZoomBinding::primary("+", ZoomAction::In),
                ZoomBinding {
                    scope: ZoomScope::Window,
                    key: "+".into(),
                    modifiers: vec![ZoomModifier::Primary, ZoomModifier::Shift],
                    action: ZoomAction::In,
                },
                ZoomBinding::primary("-", ZoomAction::Out),
                ZoomBinding::primary("0", ZoomAction::Reset),
            ],
            wheel: ZoomWheelConfiguration::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZoomAnimationConfiguration {
    pub enabled: bool,
    /// Explicit accessibility override; no unreliable platform inference.
    pub reduced_motion: bool,
    pub duration_ms: u64,
    pub easing: ZoomEasing,
}

impl Default for ZoomAnimationConfiguration {
    fn default() -> Self {
        Self {
            enabled: true,
            reduced_motion: false,
            duration_ms: 120,
            easing: ZoomEasing::EaseOut,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoomEasing {
    Linear,
    EaseInOut,
    #[default]
    EaseOut,
}

impl ZoomEasing {
    fn sample(self, progress: f64) -> f64 {
        match self {
            Self::Linear => progress,
            Self::EaseInOut => progress * progress * (3.0 - 2.0 * progress),
            Self::EaseOut => 1.0 - (1.0 - progress).powi(3),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(
    rename_all = "snake_case",
    tag = "action",
    content = "value",
    deny_unknown_fields
)]
pub enum ZoomAction {
    In,
    Out,
    Reset,
    SetScale(f64),
    /// Exact desired logical font size; base size is supplied by the application.
    SetSize(f64),
    /// Fractional normalized wheel steps.
    Steps(f64),
    /// The matcher owns this event, but it must not change scale.
    Consume,
}

/// Routing intent only; targets must be resolved from committed state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoomScope {
    #[default]
    Window,
    Pane,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScopedZoomAction {
    pub scope: ZoomScope,
    pub action: ZoomAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoomModifier {
    Primary,
    Control,
    Alt,
    Shift,
    Super,
}

fn modifier_bits(modifiers: &[ZoomModifier]) -> u8 {
    modifiers.iter().fold(0, |bits, modifier| {
        bits | match modifier {
            ZoomModifier::Primary if cfg!(target_os = "macos") => 8,
            ZoomModifier::Primary | ZoomModifier::Control => 1,
            ZoomModifier::Alt => 2,
            ZoomModifier::Shift => 4,
            ZoomModifier::Super => 8,
        }
    })
}

fn matches_modifiers(expected: &[ZoomModifier], actual: Modifiers) -> bool {
    let actual = u8::from(actual.control)
        | (u8::from(actual.alt) << 1)
        | (u8::from(actual.shift) << 2)
        | (u8::from(actual.super_key) << 3);
    modifier_bits(expected) == actual
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ZoomBinding {
    /// Omitted scopes retain whole-window behavior.
    #[serde(default)]
    pub scope: ZoomScope,
    /// One logical character (not backend physical key codes).
    pub key: String,
    pub modifiers: Vec<ZoomModifier>,
    pub action: ZoomAction,
}

impl ZoomBinding {
    fn primary(key: &str, action: ZoomAction) -> Self {
        Self {
            scope: ZoomScope::Window,
            key: key.into(),
            modifiers: vec![ZoomModifier::Primary],
            action,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZoomWheelConfiguration {
    pub scope: ZoomScope,
    pub enabled: bool,
    pub modifiers: Vec<ZoomModifier>,
    /// Multiplicative zoom steps per normalized wheel line.
    pub sensitivity: f64,
}

impl Default for ZoomWheelConfiguration {
    fn default() -> Self {
        Self {
            scope: ZoomScope::Window,
            enabled: true,
            modifiers: vec![ZoomModifier::Primary],
            sensitivity: 0.1,
        }
    }
}

impl ZoomConfiguration {
    /// Validate finite bounds, timing, and unambiguous bounded bindings.
    ///
    /// # Errors
    /// Returns an error for invalid bounds, values, modifiers, or duplicate bindings.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.step.is_finite() && self.step > 1.0,
            "zoom.step must be finite and greater than one"
        );
        ensure!(
            self.min_scale.is_finite() && self.min_scale > 0.0 && self.min_scale <= 1.0,
            "zoom.min_scale must be positive, finite and <= 1"
        );
        ensure!(
            self.max_scale.is_finite() && self.max_scale >= 1.0,
            "zoom.max_scale must be finite and >= 1"
        );
        ensure!(
            self.animation.duration_ms <= 10_000,
            "zoom animation duration must not exceed 10000ms"
        );
        ensure!(
            self.bindings.len() <= 64,
            "at most 64 zoom bindings are supported"
        );
        ensure!(
            self.wheel.sensitivity.is_finite()
                && self.wheel.sensitivity > 0.0
                && self.wheel.sensitivity <= 100.0,
            "zoom wheel sensitivity must be in (0, 100]"
        );
        validate_modifiers(&self.wheel.modifiers)?;
        for (index, binding) in self.bindings.iter().enumerate() {
            ensure!(
                binding.key.chars().count() == 1,
                "zoom binding key must be one logical character"
            );
            validate_modifiers(&binding.modifiers)?;
            validate_action(binding.action)?;
            ensure!(
                !self.bindings[..index]
                    .iter()
                    .any(|prior| prior.key.eq_ignore_ascii_case(&binding.key)
                        && modifier_bits(&prior.modifiers) == modifier_bits(&binding.modifiers)),
                "duplicate zoom binding"
            );
        }
        Ok(())
    }
}

fn validate_modifiers(modifiers: &[ZoomModifier]) -> Result<()> {
    ensure!(
        modifiers.len() <= 4 && modifier_bits(modifiers).count_ones() as usize == modifiers.len(),
        "duplicate or excessive zoom modifiers"
    );
    Ok(())
}

fn validate_action(action: ZoomAction) -> Result<()> {
    match action {
        ZoomAction::SetScale(value) | ZoomAction::SetSize(value) => ensure!(
            value.is_finite() && value > 0.0,
            "zoom value must be positive and finite"
        ),
        ZoomAction::Steps(value) => ensure!(value.is_finite(), "zoom steps must be finite"),
        _ => (),
    }
    Ok(())
}

/// Bounded, window-local semantic binding state. Call before worker forwarding.
/// Never forward an event for which `input` returns `Some`, including Consume.
pub struct ZoomInputMatcher {
    configuration: ZoomConfiguration,
    held: Vec<(PhysicalKey, ScopedZoomAction)>,
}

impl ZoomInputMatcher {
    /// Build a window-local matcher from validated configuration.
    ///
    /// # Errors
    /// Returns an error if the configuration is invalid.
    pub fn new(configuration: &ZoomConfiguration) -> Result<Self> {
        configuration.validate()?;
        Ok(Self {
            configuration: configuration.clone(),
            held: Vec::new(),
        })
    }

    /// Replace binding policy while retaining ownership of held physical keys.
    /// Held keys keep ownership until release. Only step actions repeat; one-shot
    /// Reset/Set actions are consumed without reapplying to a newly focused view.
    ///
    /// # Errors
    /// Returns an error for invalid configuration, leaving policy and held keys unchanged.
    pub fn reconfigure(&mut self, configuration: &ZoomConfiguration) -> Result<()> {
        configuration.validate()?;
        self.configuration = configuration.clone();
        Ok(())
    }

    /// Release shortcut ownership on suspension or focus loss.
    /// Use `reconfigure` for policy reloads so held releases remain consumed.
    /// Focus-loss events perform the same cleanup automatically.
    pub fn reset(&mut self) {
        self.held.clear();
    }

    pub fn input(&mut self, event: &InputEvent) -> Option<ScopedZoomAction> {
        match event {
            InputEvent::Focus(false) => {
                self.reset();
                None
            }
            InputEvent::Key(key) => {
                if let Some(index) = self
                    .held
                    .iter()
                    .position(|(physical, _)| *physical == key.physical)
                {
                    let action = self.held[index].1;
                    if key.phase == KeyPhase::Release {
                        self.held.swap_remove(index);
                        return Some(ScopedZoomAction {
                            scope: action.scope,
                            action: ZoomAction::Consume,
                        });
                    }
                    return Some(ScopedZoomAction {
                        scope: action.scope,
                        action: if matches!(
                            action.action,
                            ZoomAction::In | ZoomAction::Out | ZoomAction::Steps(_)
                        ) {
                            action.action
                        } else {
                            ZoomAction::Consume
                        },
                    });
                }
                // A repeat without an owned press must not capture a worker-owned key.
                if key.phase != KeyPhase::Press || key.physical == PhysicalKey::Unidentified {
                    return None;
                }
                let LogicalKey::Character(text) = &key.logical else {
                    return None;
                };
                let binding = self.configuration.bindings.iter().find(|binding| {
                    binding.key.eq_ignore_ascii_case(text)
                        && matches_modifiers(&binding.modifiers, key.modifiers)
                })?;
                // Refuse new ownership rather than silently dropping a release identity.
                if self.held.len() >= 64 {
                    return None;
                }
                let action = ScopedZoomAction {
                    scope: binding.scope,
                    action: binding.action,
                };
                self.held.push((key.physical.clone(), action));
                Some(action)
            }
            InputEvent::Scroll { delta, modifiers }
                if self.configuration.wheel.enabled
                    && matches_modifiers(&self.configuration.wheel.modifiers, *modifiers) =>
            {
                let lines = bterm_native_platform::input::normalized_scroll_lines(*delta);
                Some(ScopedZoomAction {
                    scope: self.configuration.wheel.scope,
                    action: if lines == 0.0 {
                        ZoomAction::Consume
                    } else {
                        ZoomAction::Steps(lines * self.configuration.wheel.sensitivity)
                    },
                })
            }
            _ => None,
        }
    }
}

/// Time-explicit, allocation-free animation state. Target state is not output authority.
#[derive(Clone)]
pub struct ZoomController {
    configuration: ZoomConfiguration,
    from: f64,
    target: f64,
    started: Option<Instant>,
}

impl ZoomController {
    /// Construct scale state, optionally restoring a finite positive remembered scale.
    ///
    /// # Errors
    /// Returns an error if the configuration is invalid.
    pub fn new(configuration: ZoomConfiguration, remembered_scale: Option<f64>) -> Result<Self> {
        configuration.validate()?;
        let initial = if configuration.remember {
            remembered_scale
                .filter(|value| value.is_finite() && *value > 0.0)
                .unwrap_or(1.0)
        } else {
            1.0
        }
        .clamp(configuration.min_scale, configuration.max_scale);
        Ok(Self {
            configuration,
            from: initial,
            target: initial,
            started: None,
        })
    }

    /// Replace zoom policy without reloading remembered state or resetting local intent.
    /// Current and target scales are clamped to the new bounds. Any remaining
    /// transition restarts at `now` using the new duration and easing; disabling
    /// animation (including reduced motion or zero duration) settles immediately.
    ///
    /// # Errors
    /// Returns an error if the configuration is invalid, leaving all state unchanged.
    pub fn reconfigure(&mut self, configuration: ZoomConfiguration, now: Instant) -> Result<()> {
        configuration.validate()?;
        let current = self
            .sample(now)
            .clamp(configuration.min_scale, configuration.max_scale);
        let target = self
            .target
            .clamp(configuration.min_scale, configuration.max_scale);
        let animate = configuration.animation.enabled
            && !configuration.animation.reduced_motion
            && configuration.animation.duration_ms > 0
            && current.to_bits() != target.to_bits();
        self.configuration = configuration;
        self.from = current;
        self.target = target;
        self.started = animate.then_some(now);
        Ok(())
    }

    #[must_use]
    pub fn target_scale(&self) -> f64 {
        self.target
    }

    /// Returns whether target changed. `SetSize` uses an unscaled logical font size,
    /// not framebuffer pixels, monitor DPI, or an already-scaled size.
    ///
    /// # Errors
    /// Returns an error for non-finite action values, non-positive sizes/scales,
    /// or an invalid base size for `SetSize`. State is unchanged on error.
    pub fn apply(&mut self, action: ZoomAction, base_size: f64, now: Instant) -> Result<bool> {
        validate_action(action)?;
        let steps = match action {
            ZoomAction::In => Some(1.0),
            ZoomAction::Out => Some(-1.0),
            ZoomAction::Steps(value) => Some(value),
            _ => None,
        };
        let target = if let Some(steps) = steps {
            (self.target.ln() + steps * self.configuration.step.ln())
                .clamp(
                    self.configuration.min_scale.ln(),
                    self.configuration.max_scale.ln(),
                )
                .exp()
        } else {
            match action {
                ZoomAction::Reset => 1.0,
                ZoomAction::SetScale(value) => value,
                ZoomAction::SetSize(value) => {
                    ensure!(
                        base_size.is_finite() && base_size > 0.0,
                        "base logical font size must be positive and finite"
                    );
                    value / base_size
                }
                _ => return Ok(false),
            }
        }
        .clamp(self.configuration.min_scale, self.configuration.max_scale);
        // Exact positive-scale identity keeps unchanged targets from restarting animation.
        if target.to_bits() == self.target.to_bits() {
            return Ok(false);
        }
        self.from = self.sample(now);
        self.target = target;
        self.started = if self.configuration.animation.enabled
            && !self.configuration.animation.reduced_motion
            && self.configuration.animation.duration_ms > 0
        {
            Some(now)
        } else {
            None
        };
        Ok(true)
    }

    #[must_use]
    pub fn sample(&self, now: Instant) -> f64 {
        let Some(started) = self.started else {
            return self.target;
        };
        let elapsed = now.saturating_duration_since(started);
        let duration = Duration::from_millis(self.configuration.animation.duration_ms);
        if elapsed >= duration {
            return self.target;
        }
        let progress = self
            .configuration
            .animation
            .easing
            .sample(elapsed.as_secs_f64() / duration.as_secs_f64());
        (self.from.ln() + (self.target.ln() - self.from.ln()) * progress).exp()
    }

    #[must_use]
    pub fn needs_frame(&self, now: Instant) -> bool {
        self.started.is_some_and(|started| {
            now.saturating_duration_since(started)
                < Duration::from_millis(self.configuration.animation.duration_ms)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bterm_native_platform::input::{KeyEvent, ScrollDelta};

    fn key(phase: KeyPhase, modifiers: Modifiers) -> InputEvent {
        InputEvent::Key(KeyEvent {
            physical: PhysicalKey::Code("Equal".into()),
            logical: LogicalKey::Character("=".into()),
            phase,
            modifiers,
            text: None,
        })
    }

    fn assert_scale(actual: f64, expected: f64) {
        // These assertions check exact stored values/endpoints, not approximate interpolation.
        assert_eq!(actual.to_bits(), expected.to_bits());
    }

    fn primary() -> Modifiers {
        Modifiers {
            super_key: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Modifiers::default()
        }
    }

    #[test]
    fn matcher_reload_preserves_held_ownership_after_binding_removed() {
        let mut configuration = ZoomConfiguration::default();
        let mut matcher = ZoomInputMatcher::new(&configuration).unwrap();
        assert_eq!(
            matcher.input(&key(KeyPhase::Press, primary())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::In
            })
        );
        configuration.bindings.clear();
        matcher.reconfigure(&configuration).unwrap();
        assert_eq!(matcher.configuration, configuration);
        assert_eq!(
            matcher.input(&key(KeyPhase::Repeat, Modifiers::default())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::In
            })
        );
        assert_eq!(
            matcher.input(&key(KeyPhase::Release, Modifiers::default())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::Consume
            })
        );
        assert_eq!(matcher.input(&key(KeyPhase::Press, primary())), None);
    }

    #[test]
    fn invalid_matcher_reload_preserves_policy_and_held_keys() {
        let configuration = ZoomConfiguration::default();
        let mut matcher = ZoomInputMatcher::new(&configuration).unwrap();
        assert_eq!(
            matcher.input(&key(KeyPhase::Press, primary())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::In
            })
        );
        let invalid = ZoomConfiguration {
            step: 1.0,
            bindings: Vec::new(),
            ..configuration.clone()
        };
        assert!(matcher.reconfigure(&invalid).is_err());
        assert_eq!(matcher.configuration, configuration);
        assert_eq!(
            matcher.input(&key(KeyPhase::Repeat, Modifiers::default())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::In
            })
        );
        assert_eq!(
            matcher.input(&key(KeyPhase::Release, Modifiers::default())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::Consume
            })
        );
        assert_eq!(
            matcher.input(&key(KeyPhase::Press, primary())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::In
            })
        );
    }

    #[test]
    fn held_one_shot_actions_are_consumed_until_release() {
        let config = ZoomConfiguration {
            bindings: vec![ZoomBinding::primary("0", ZoomAction::Reset)],
            ..ZoomConfiguration::default()
        };
        let mut matcher = ZoomInputMatcher::new(&config).unwrap();
        let event = |phase| {
            InputEvent::Key(KeyEvent {
                phase,
                physical: PhysicalKey::Code("Digit0".into()),
                logical: LogicalKey::Character("0".into()),
                modifiers: primary(),
                text: None,
            })
        };
        assert_eq!(
            matcher.input(&event(KeyPhase::Press)).unwrap().action,
            ZoomAction::Reset
        );
        assert_eq!(
            matcher.input(&event(KeyPhase::Repeat)).unwrap().action,
            ZoomAction::Consume
        );
        assert_eq!(
            matcher.input(&event(KeyPhase::Release)).unwrap().action,
            ZoomAction::Consume
        );
        assert!(matcher.held.is_empty());
    }

    #[test]
    fn controller_clone_retains_independent_animation_snapshot() {
        let mut controller = ZoomController::new(ZoomConfiguration::default(), None).unwrap();
        let start = Instant::now();
        controller
            .apply(ZoomAction::SetScale(2.0), 14.0, start)
            .unwrap();
        let snapshot = controller.clone();
        let later = start + Duration::from_millis(60);
        assert_scale(snapshot.sample(later), controller.sample(later));
        controller.apply(ZoomAction::Reset, 14.0, later).unwrap();
        assert_scale(snapshot.target_scale(), 2.0);
        assert_scale(controller.target_scale(), 1.0);
        assert_scale(snapshot.sample(start + Duration::from_millis(120)), 2.0);
    }

    #[test]
    fn reconfigure_preserves_sample_and_target_with_new_timing() {
        let mut config = ZoomConfiguration::default();
        config.animation.easing = ZoomEasing::Linear;
        let mut controller = ZoomController::new(config.clone(), Some(1.25)).unwrap();
        let start = Instant::now();
        controller
            .apply(ZoomAction::SetScale(2.0), 14.0, start)
            .unwrap();
        let reload = start + Duration::from_millis(60);
        let current = controller.sample(reload);
        config.remember = false;
        config.step = 1.2;
        config.animation.duration_ms = 200;
        config.animation.easing = ZoomEasing::EaseInOut;
        controller.reconfigure(config.clone(), reload).unwrap();
        assert!((controller.sample(reload) - current).abs() < 1e-12);
        assert_scale(controller.target_scale(), 2.0);
        assert_eq!(controller.configuration, config);
        assert!(controller.needs_frame(reload + Duration::from_millis(199)));
        let quarter = controller.sample(reload + Duration::from_millis(50));
        let expected_quarter = (current.ln() + (2.0_f64.ln() - current.ln()) * 0.15625).exp();
        assert!((quarter - expected_quarter).abs() < 1e-12);
        let halfway = controller.sample(reload + Duration::from_millis(100));
        assert!((halfway - (current * 2.0).sqrt()).abs() < 1e-12);
        let end = reload + Duration::from_millis(200);
        assert_scale(controller.sample(end), 2.0);
        assert!(!controller.needs_frame(end));
    }

    #[test]
    fn invalid_reconfigure_preserves_configuration_and_animation() {
        let config = ZoomConfiguration::default();
        let mut controller = ZoomController::new(config.clone(), None).unwrap();
        let start = Instant::now();
        controller
            .apply(ZoomAction::SetScale(2.0), 14.0, start)
            .unwrap();
        let reload = start + Duration::from_millis(60);
        let current = controller.sample(reload);
        let future = controller.sample(reload + Duration::from_millis(30));
        let invalid = ZoomConfiguration {
            min_scale: f64::NAN,
            ..config.clone()
        };
        assert!(controller.reconfigure(invalid, reload).is_err());
        assert_eq!(controller.configuration, config);
        assert_scale(controller.target_scale(), 2.0);
        assert_scale(controller.sample(reload), current);
        assert_scale(
            controller.sample(reload + Duration::from_millis(30)),
            future,
        );
        assert!(controller.needs_frame(reload));
    }

    #[test]
    fn reconfigure_disabled_animation_reduced_motion_and_zero_duration_settle() {
        let start = Instant::now();
        for mode in 0..3 {
            let mut config = ZoomConfiguration::default();
            let mut controller = ZoomController::new(config.clone(), None).unwrap();
            controller
                .apply(ZoomAction::SetScale(2.0), 14.0, start)
                .unwrap();
            match mode {
                0 => config.animation.enabled = false,
                1 => config.animation.reduced_motion = true,
                _ => config.animation.duration_ms = 0,
            }
            let reload = start + Duration::from_millis(60);
            controller.reconfigure(config, reload).unwrap();
            assert_scale(controller.sample(reload), 2.0);
            assert_scale(controller.target_scale(), 2.0);
            assert!(!controller.needs_frame(reload));
        }
    }

    #[test]
    fn reconfigure_clamps_sample_and_target_to_new_bounds() {
        let mut config = ZoomConfiguration::default();
        config.animation.easing = ZoomEasing::Linear;
        let start = Instant::now();
        let mut controller = ZoomController::new(config.clone(), None).unwrap();
        controller
            .apply(ZoomAction::SetScale(3.0), 14.0, start)
            .unwrap();
        let reload = start + Duration::from_millis(1);
        let current = controller.sample(reload);
        config.max_scale = 1.5;
        controller.reconfigure(config.clone(), reload).unwrap();
        assert_scale(controller.target_scale(), 1.5);
        assert!((controller.sample(reload) - current).abs() < 1e-12);
        assert!(controller.needs_frame(reload));
        let end = reload + Duration::from_millis(120);
        assert_scale(controller.sample(end), 1.5);
        config.max_scale = 1.25;
        controller.reconfigure(config, end).unwrap();
        assert_scale(controller.sample(end), 1.25);
        assert!(!controller.needs_frame(end));

        let config = ZoomConfiguration {
            min_scale: 0.5,
            ..ZoomConfiguration::default()
        };
        let mut controller = ZoomController::new(config.clone(), Some(0.5)).unwrap();
        let new_config = ZoomConfiguration {
            min_scale: 0.75,
            ..config
        };
        controller.reconfigure(new_config, start).unwrap();
        assert_scale(controller.sample(start), 0.75);
        assert_scale(controller.target_scale(), 0.75);
        assert!(!controller.needs_frame(start));
    }

    #[test]
    fn log_interpolation_retargets_continuously_and_stops() {
        let mut config = ZoomConfiguration::default();
        config.animation.easing = ZoomEasing::Linear;
        let mut controller = ZoomController::new(config, None).unwrap();
        let now = Instant::now();
        controller
            .apply(ZoomAction::SetScale(2.0), 14.0, now)
            .unwrap();
        let midpoint = now + Duration::from_millis(60);
        assert!((controller.sample(midpoint) - 2.0_f64.sqrt()).abs() < 1e-12);
        controller.apply(ZoomAction::Out, 14.0, midpoint).unwrap();
        assert!((controller.sample(midpoint) - 2.0_f64.sqrt()).abs() < 1e-12);
        assert!(controller.needs_frame(midpoint));
        let end = midpoint + Duration::from_millis(120);
        assert_scale(controller.sample(end), controller.target_scale());
        assert!(!controller.needs_frame(end));
    }

    #[test]
    fn remembered_exact_size_bounds_and_reduced_motion() {
        let mut config = ZoomConfiguration::default();
        config.animation.reduced_motion = true;
        let mut controller = ZoomController::new(config, Some(1.25)).unwrap();
        let now = Instant::now();
        assert_scale(controller.sample(now), 1.25);
        controller
            .apply(ZoomAction::SetSize(17.5), 14.0, now)
            .unwrap();
        assert_scale(controller.target_scale(), 1.25);
        controller
            .apply(ZoomAction::SetScale(100.0), 14.0, now)
            .unwrap();
        assert_scale(controller.sample(now), 3.0);
        assert!(!controller.needs_frame(now));
        assert!(
            controller
                .apply(ZoomAction::SetSize(12.0), 0.0, now)
                .is_err()
        );
        assert!(
            controller
                .apply(ZoomAction::SetScale(f64::NAN), 14.0, now)
                .is_err()
        );
        controller.apply(ZoomAction::Reset, 14.0, now).unwrap();
        assert_scale(controller.sample(now), 1.0);
    }

    #[test]
    fn owned_lifecycle_survives_modifier_release_and_focus_clears() {
        let mut matcher = ZoomInputMatcher::new(&ZoomConfiguration::default()).unwrap();
        assert_eq!(
            matcher.input(&key(KeyPhase::Press, primary())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::In
            })
        );
        assert_eq!(
            matcher.input(&key(KeyPhase::Repeat, Modifiers::default())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::In
            })
        );
        assert_eq!(
            matcher.input(&key(KeyPhase::Release, Modifiers::default())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::Consume
            })
        );
        assert_eq!(matcher.input(&key(KeyPhase::Repeat, primary())), None);
        matcher.input(&key(KeyPhase::Press, primary()));
        matcher.input(&InputEvent::Focus(false));
        assert_eq!(matcher.input(&key(KeyPhase::Release, primary())), None);
    }

    #[test]
    fn wheel_is_fractional_and_unmodified_wheel_is_not_captured() {
        let mut matcher = ZoomInputMatcher::new(&ZoomConfiguration::default()).unwrap();
        assert_eq!(
            matcher.input(&InputEvent::Scroll {
                delta: ScrollDelta::Pixels { x: 0.0, y: 20.0 },
                modifiers: primary()
            }),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::Steps(0.05)
            })
        );
        assert_eq!(
            matcher.input(&InputEvent::Scroll {
                delta: ScrollDelta::Lines { x: 0.0, y: 1.0 },
                modifiers: Modifiers::default()
            }),
            None
        );
    }

    #[test]
    fn disabled_remembering_and_animation_are_immediate() {
        let mut config = ZoomConfiguration {
            remember: false,
            ..ZoomConfiguration::default()
        };
        config.animation.enabled = false;
        let mut controller = ZoomController::new(config, Some(2.0)).unwrap();
        let now = Instant::now();
        assert_scale(controller.sample(now), 1.0);
        controller
            .apply(ZoomAction::SetSize(15.5), 14.0, now)
            .unwrap();
        assert_scale(controller.sample(now), 15.5 / 14.0);
        controller
            .apply(ZoomAction::Steps(-f64::MAX), 14.0, now)
            .unwrap();
        assert_scale(controller.target_scale(), 0.5);
        assert!(!controller.needs_frame(now));
        assert!(!controller.apply(ZoomAction::Consume, 14.0, now).unwrap());
    }

    #[test]
    fn scoped_bindings_keep_original_scope_until_release() {
        let mut policy = ZoomConfiguration::default();
        policy.bindings[0].scope = ZoomScope::Pane;
        let mut matcher = ZoomInputMatcher::new(&policy).unwrap();
        let expected = ScopedZoomAction {
            scope: ZoomScope::Pane,
            action: ZoomAction::In,
        };
        assert_eq!(
            matcher.input(&key(KeyPhase::Press, primary())),
            Some(expected)
        );
        policy.bindings[0].scope = ZoomScope::Window;
        policy.bindings[0].action = ZoomAction::Out;
        matcher.reconfigure(&policy).unwrap();
        assert_eq!(
            matcher.input(&key(KeyPhase::Repeat, Modifiers::default())),
            Some(expected)
        );
        assert_eq!(
            matcher.input(&key(KeyPhase::Release, Modifiers::default())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Pane,
                action: ZoomAction::Consume
            })
        );
        assert_eq!(
            matcher.input(&key(KeyPhase::Press, primary())),
            Some(ScopedZoomAction {
                scope: ZoomScope::Window,
                action: ZoomAction::Out
            })
        );
    }

    #[test]
    fn legacy_scopes_default_to_window_and_cross_scope_collisions_are_rejected() {
        let binding: ZoomBinding =
            serde_json::from_str(r#"{"key":"=","modifiers":["primary"],"action":{"action":"in"}}"#)
                .unwrap();
        assert_eq!(binding.scope, ZoomScope::Window);
        let wheel: ZoomWheelConfiguration = serde_json::from_str("{}").unwrap();
        assert_eq!(wheel.scope, ZoomScope::Window);
        let mut policy = ZoomConfiguration::default();
        let mut duplicate = policy.bindings[0].clone();
        duplicate.scope = ZoomScope::Pane;
        policy.bindings.push(duplicate);
        assert!(policy.validate().is_err());
    }

    #[test]
    fn pane_wheel_uses_scope_even_when_only_consuming() {
        let mut policy = ZoomConfiguration::default();
        policy.wheel.scope = ZoomScope::Pane;
        let mut matcher = ZoomInputMatcher::new(&policy).unwrap();
        for (y, action) in [(20.0, ZoomAction::Steps(0.05)), (0.0, ZoomAction::Consume)] {
            assert_eq!(
                matcher.input(&InputEvent::Scroll {
                    delta: ScrollDelta::Pixels { x: 1.0, y },
                    modifiers: primary(),
                }),
                Some(ScopedZoomAction {
                    scope: ZoomScope::Pane,
                    action
                })
            );
        }
    }

    #[test]
    fn binding_action_wire_format_round_trips() {
        let binding = ZoomBinding {
            scope: ZoomScope::Pane,
            key: "1".into(),
            modifiers: vec![ZoomModifier::Primary],
            action: ZoomAction::SetScale(1.25),
        };
        let json = serde_json::to_value(&binding).unwrap();
        assert_eq!(json["action"]["action"], "set_scale");
        assert_eq!(json["action"]["value"], 1.25);
        let decoded: ZoomBinding = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.action, binding.action);
        assert_eq!(decoded.scope, ZoomScope::Pane);
    }

    #[test]
    fn configuration_equality_includes_nested_settings() {
        let config = ZoomConfiguration::default();
        assert_eq!(config, config.clone());
        let mut changed = config.clone();
        changed.animation.easing = ZoomEasing::Linear;
        assert_ne!(config, changed);
        let mut changed = config.clone();
        changed.wheel.sensitivity = 0.2;
        assert_ne!(config, changed);
        let mut changed = config.clone();
        changed.bindings[0].action = ZoomAction::SetScale(1.25);
        assert_ne!(config, changed);
    }

    #[test]
    fn configuration_is_strict_and_defaults_are_valid() {
        ZoomConfiguration::default().validate().unwrap();
        assert!(serde_json::from_str::<ZoomConfiguration>(r#"{"unknown":true}"#).is_err());
        assert!(
            serde_json::from_str::<ZoomConfiguration>(r#"{"animation":{"unknown":true}}"#).is_err()
        );
        let mut config = ZoomConfiguration {
            step: 1.0,
            ..ZoomConfiguration::default()
        };
        assert!(config.validate().is_err());
        config.step = 1.1;
        config.bindings.push(config.bindings[0].clone());
        assert!(config.validate().is_err());
    }
}
