//! Headless acceptance through public Bterm and renderer APIs. No native window,
//! companion, ambient configuration, installed fonts, or sleeps are required.
//! These contracts do not substitute for native paint/hit-test acceptance.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bterm::configuration::{ConfigOptions, ResolvedConfiguration, SourcePolicy};
use bterm::runtime_paths::BtermPaths;
use bterm::typography::TypographyConfiguration;
use bterm::zoom::{
    ScopedZoomAction, ZoomAction, ZoomConfiguration, ZoomController, ZoomEasing, ZoomInputMatcher,
    ZoomScope,
};
use bterm::zoom_preferences::{PreferenceStore, ZoomPreferences};
use bterm_native_platform::input::{
    InputEvent, KeyEvent, KeyPhase, LogicalKey, Modifiers, PhysicalKey, ScrollDelta,
};
use bterm_native_renderer::text::{TextRasterizer, TextStyle};

fn window_action(action: ZoomAction) -> Option<ScopedZoomAction> {
    Some(ScopedZoomAction {
        scope: ZoomScope::Window,
        action,
    })
}

fn primary() -> Modifiers {
    Modifiers {
        super_key: cfg!(target_os = "macos"),
        control: !cfg!(target_os = "macos"),
        ..Modifiers::default()
    }
}

fn key(text: &str, phase: KeyPhase, modifiers: Modifiers) -> InputEvent {
    InputEvent::Key(KeyEvent {
        physical: PhysicalKey::Code("zoom-test-position".into()),
        logical: LogicalKey::Character(text.into()),
        phase,
        modifiers,
        text: None,
    })
}

fn resolve(document: &str) -> ResolvedConfiguration {
    let root = tempfile::tempdir().unwrap();
    let options = ConfigOptions {
        policy: SourcePolicy::Explicit,
        ..ConfigOptions::default()
    };
    let mut inputs = options
        .capture_selected_with_environment(
            BtermPaths::from_root(root.path().to_owned()).unwrap(),
            root.path().to_owned(),
            BTreeMap::new(),
        )
        .unwrap();
    inputs.bterm = bmux_config::ConfigFamily {
        documents: vec![bmux_config::ConfigDocument {
            source: bmux_config::ConfigSource {
                path: root.path().join("selected.toml"),
                required: true,
            },
            value: Some(toml::from_str(document).unwrap()),
            local: false,
            inline_origin: None,
        }],
    };
    inputs.resolve().unwrap()
}

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-10, "{actual} != {expected}");
}

#[test]
fn native_configuration_preserves_fractional_typography_without_changing_companion_identity() {
    let baseline = resolve("");
    let selected = resolve(
        r#"
[bterm.typography]
size = 15.25
family = "Example Mono"
fallback = ["Example Symbols", "Example Emoji"]
line_height = 1.125
letter_spacing = 0.25
[bterm.zoom]
step = 1.125
min_scale = 0.75
max_scale = 2.5
remember = false
[bterm.zoom.animation]
enabled = false
"#,
    );
    assert_eq!(selected.native.typography.size, 15.25);
    assert_eq!(
        selected.native.typography.family.as_deref(),
        Some("Example Mono")
    );
    assert_eq!(
        selected.native.typography.fallback,
        ["Example Symbols", "Example Emoji"]
    );
    assert_eq!(selected.native.typography.line_height, 1.125);
    assert_eq!(selected.native.typography.letter_spacing, 0.25);
    assert!(!selected.native.zoom.remember);
    assert!(!selected.native.zoom.animation.enabled);
    assert_eq!(
        baseline.runtime_configuration_id,
        selected.runtime_configuration_id
    );
    assert_eq!(baseline.bmux_toml, selected.bmux_toml);
    assert_ne!(baseline.generation, selected.generation);
}

#[test]
fn default_zoom_is_animated_remembered_and_whole_window() {
    let configuration = resolve("");
    let zoom = configuration.native.zoom;
    assert!(zoom.remember);
    assert!(zoom.animation.enabled);
    assert!(!zoom.animation.reduced_motion);
    assert_eq!(zoom.animation.duration_ms, 120);
    let now = Instant::now();
    let mut controller = ZoomController::new(zoom, Some(1.25)).unwrap();
    close(controller.sample(now), 1.25);
    assert!(controller.apply(ZoomAction::In, 16.0, now).unwrap());
    close(controller.sample(now), 1.25);
    assert!(controller.needs_frame(now));
    close(controller.target_scale(), 1.375);
    let end = now + Duration::from_secs(1);
    close(controller.sample(end), 1.375);
    assert!(!controller.needs_frame(end));
}

#[test]
fn exact_logical_size_reset_fractional_steps_and_bounds_do_not_round_to_integer_sizes() {
    let mut configuration = ZoomConfiguration::default();
    configuration.animation.enabled = false;
    let mut controller = ZoomController::new(configuration, None).unwrap();
    let now = Instant::now();
    controller
        .apply(ZoomAction::SetSize(17.75), 15.25, now)
        .unwrap();
    close(controller.sample(now) * 15.25, 17.75);
    controller.apply(ZoomAction::Reset, 15.25, now).unwrap();
    close(controller.sample(now), 1.0);
    controller
        .apply(ZoomAction::Steps(0.25), 15.25, now)
        .unwrap();
    close(controller.sample(now), 1.1_f64.powf(0.25));
    controller
        .apply(ZoomAction::Steps(-0.25), 15.25, now)
        .unwrap();
    close(controller.sample(now), 1.0);
    controller
        .apply(ZoomAction::Steps(f64::MAX), 15.25, now)
        .unwrap();
    close(controller.sample(now), 3.0);
    controller
        .apply(ZoomAction::Steps(-f64::MAX), 15.25, now)
        .unwrap();
    close(controller.sample(now), 0.5);
    assert!(!controller.needs_frame(now));
}

#[test]
fn animation_retargets_from_current_sample_and_stops_without_sleeping() {
    let mut configuration = ZoomConfiguration::default();
    configuration.animation.easing = ZoomEasing::Linear;
    let mut controller = ZoomController::new(configuration, None).unwrap();
    let start = Instant::now();
    controller
        .apply(ZoomAction::SetScale(2.0), 16.0, start)
        .unwrap();
    let midpoint = start + Duration::from_millis(60);
    close(controller.sample(midpoint), 2.0_f64.sqrt());
    controller
        .apply(ZoomAction::SetScale(0.75), 16.0, midpoint)
        .unwrap();
    close(controller.sample(midpoint), 2.0_f64.sqrt());
    let end = midpoint + Duration::from_millis(120);
    close(controller.sample(end), 0.75);
    assert!(!controller.needs_frame(end));
    assert!(!controller.apply(ZoomAction::Consume, 16.0, end).unwrap());
    assert!(
        !controller
            .apply(ZoomAction::SetScale(0.75), 16.0, end)
            .unwrap()
    );
}

#[test]
fn reduced_motion_disabled_animation_and_zero_duration_apply_immediately() {
    for mode in 0..3 {
        let mut configuration = ZoomConfiguration::default();
        match mode {
            0 => configuration.animation.reduced_motion = true,
            1 => configuration.animation.enabled = false,
            _ => configuration.animation.duration_ms = 0,
        }
        configuration.remember = false;
        let now = Instant::now();
        let mut controller = ZoomController::new(configuration, Some(2.0)).unwrap();
        close(controller.sample(now), 1.0);
        controller
            .apply(ZoomAction::SetSize(17.5), 16.0, now)
            .unwrap();
        close(controller.sample(now), 17.5 / 16.0);
        assert!(!controller.needs_frame(now));
    }
}

#[test]
fn invalid_actions_do_not_poison_a_valid_controller() {
    let now = Instant::now();
    let mut controller = ZoomController::new(ZoomConfiguration::default(), None).unwrap();
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0] {
        assert!(
            controller
                .apply(ZoomAction::SetScale(value), 16.0, now)
                .is_err()
        );
        assert!(
            controller
                .apply(ZoomAction::SetSize(value), 16.0, now)
                .is_err()
        );
        assert!(
            controller
                .apply(ZoomAction::SetSize(17.5), value, now)
                .is_err()
        );
        close(controller.target_scale(), 1.0);
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            controller
                .apply(ZoomAction::Steps(value), 16.0, now)
                .is_err()
        );
    }
    assert!(!controller.needs_frame(now));
}

#[test]
fn shortcut_owns_repeat_and_release_even_when_modifiers_are_released_first() {
    let mut matcher = ZoomInputMatcher::new(&ZoomConfiguration::default()).unwrap();
    assert_eq!(matcher.input(&key("=", KeyPhase::Repeat, primary())), None);
    assert_eq!(
        matcher.input(&key("=", KeyPhase::Press, primary())),
        window_action(ZoomAction::In)
    );
    assert_eq!(
        matcher.input(&key("=", KeyPhase::Repeat, Modifiers::default())),
        window_action(ZoomAction::In)
    );
    assert_eq!(
        matcher.input(&key("=", KeyPhase::Release, Modifiers::default())),
        window_action(ZoomAction::Consume)
    );
    assert_eq!(
        matcher.input(&key("=", KeyPhase::Press, Modifiers::default())),
        None
    );
    assert_eq!(
        matcher.input(&key("0", KeyPhase::Press, primary())),
        window_action(ZoomAction::Reset)
    );
    assert_eq!(matcher.input(&InputEvent::Focus(false)), None);
    assert_eq!(matcher.input(&key("0", KeyPhase::Repeat, primary())), None);
}

#[test]
fn configured_keyboard_actions_use_tagged_values_and_exact_modifiers() {
    let configuration: ZoomConfiguration = toml::from_str(
        r#"
bindings = [
  { key = "s", modifiers = ["primary"], action = { action = "set_size", value = 17.75 } },
  { key = "z", modifiers = ["primary"], action = { action = "set_scale", value = 1.25 } },
  { key = "r", modifiers = ["primary"], action = { action = "reset" } },
]
"#,
    )
    .unwrap();
    let mut matcher = ZoomInputMatcher::new(&configuration).unwrap();
    let mut extra = primary();
    extra.alt = true;
    assert_eq!(matcher.input(&key("s", KeyPhase::Press, extra)), None);
    let mut controller = ZoomController::new(configuration, None).unwrap();
    let now = Instant::now();
    for (text, expected) in [("s", 17.75 / 16.0), ("z", 1.25), ("r", 1.0)] {
        let action = matcher
            .input(&key(text, KeyPhase::Press, primary()))
            .unwrap();
        assert_eq!(action.scope, ZoomScope::Window);
        controller.apply(action.action, 16.0, now).unwrap();
        close(controller.target_scale(), expected);
        assert_eq!(
            matcher.input(&key(text, KeyPhase::Release, primary())),
            window_action(ZoomAction::Consume)
        );
    }
}

#[test]
fn window_local_matchers_and_controllers_do_not_share_transient_state() {
    let configuration = ZoomConfiguration::default();
    let mut first = ZoomInputMatcher::new(&configuration).unwrap();
    let mut second = ZoomInputMatcher::new(&configuration).unwrap();
    assert_eq!(
        first.input(&key("=", KeyPhase::Press, primary())),
        window_action(ZoomAction::In)
    );
    assert_eq!(second.input(&key("=", KeyPhase::Release, primary())), None);
    let now = Instant::now();
    let mut first = ZoomController::new(configuration.clone(), None).unwrap();
    let second = ZoomController::new(configuration, None).unwrap();
    first.apply(ZoomAction::SetScale(1.75), 16.0, now).unwrap();
    close(first.target_scale(), 1.75);
    close(second.target_scale(), 1.0);
    assert!(!second.needs_frame(now));
}

#[test]
fn wheel_is_fractional_modifier_gated_and_bounded() {
    let mut matcher = ZoomInputMatcher::new(&ZoomConfiguration::default()).unwrap();
    let scroll = |delta, modifiers| InputEvent::Scroll { delta, modifiers };
    let pane_modifiers = Modifiers {
        alt: true,
        ..primary()
    };
    let pane_action = |action| {
        Some(ScopedZoomAction {
            scope: ZoomScope::Pane,
            action,
        })
    };
    let pixels = ScrollDelta::Pixels { x: 0.0, y: 10.0 };
    assert_eq!(matcher.input(&scroll(pixels, Modifiers::default())), None);
    assert_eq!(matcher.input(&scroll(pixels, primary())), None);
    assert_eq!(
        matcher.input(&scroll(pixels, pane_modifiers)),
        pane_action(ZoomAction::Steps(0.025))
    );
    assert_eq!(
        matcher.input(&scroll(
            ScrollDelta::Lines { x: 0.0, y: 0.25 },
            pane_modifiers
        )),
        pane_action(ZoomAction::Steps(0.025))
    );
    assert_eq!(
        matcher.input(&scroll(
            ScrollDelta::Pixels { x: 5.0, y: 0.0 },
            pane_modifiers
        )),
        pane_action(ZoomAction::Consume)
    );
    assert_eq!(
        matcher.input(&scroll(
            ScrollDelta::Lines {
                x: 0.0,
                y: f64::NAN
            },
            pane_modifiers
        )),
        pane_action(ZoomAction::Consume)
    );
    assert_eq!(
        matcher.input(&scroll(
            ScrollDelta::Lines {
                x: 0.0,
                y: f64::MAX
            },
            pane_modifiers
        )),
        pane_action(ZoomAction::Steps(3.2))
    );
    let mut configuration = ZoomConfiguration::default();
    configuration.wheel.enabled = false;
    let mut disabled = ZoomInputMatcher::new(&configuration).unwrap();
    assert_eq!(disabled.input(&scroll(pixels, pane_modifiers)), None);
}

#[test]
fn fractional_raster_cache_keeps_sizes_distinct_and_preserves_terminal_columns() {
    let mut rasterizer = TextRasterizer::bundled().unwrap();
    let style = TextStyle::default();
    let original = rasterizer.rasterize_cell("M", 1, style, 16.0).unwrap();
    let fractional = rasterizer.rasterize_cell("M", 1, style, 16.25).unwrap();
    assert!(!Arc::ptr_eq(&original, &fractional));
    assert!(Arc::ptr_eq(
        &fractional,
        &rasterizer.rasterize_cell("M", 1, style, 16.25).unwrap()
    ));
    assert_eq!(fractional.columns, 1);
    assert!(!fractional.glyphs.is_empty());
    assert_ne!(
        rasterizer.metrics(16.0).unwrap().advance,
        rasterizer.metrics(16.25).unwrap().advance
    );
    for (text, columns) in [("e\u{301}", 1), ("界", 2), ("\u{301}", 0)] {
        let cell = rasterizer
            .rasterize_cell(text, columns, style, 20.125)
            .unwrap();
        assert_eq!(cell.columns, columns);
        assert_eq!(cell.source_bytes, text.len());
    }
}

#[test]
fn remembered_target_is_isolated_by_runtime_and_survives_native_config_edits() {
    let root = tempfile::tempdir().unwrap();
    let mut configuration = resolve("");
    configuration.inputs.paths = BtermPaths::from_root(root.path().to_owned()).unwrap();
    let first = PreferenceStore::for_configuration(&configuration, "owned-runtime-a").unwrap();
    assert_eq!(first.load().unwrap(), None);
    assert!(!first.path().exists());
    first
        .save(ZoomPreferences {
            target_scale: 1.375,
        })
        .unwrap();
    assert_eq!(first.load().unwrap().unwrap().target_scale, 1.375);
    let other = PreferenceStore::for_configuration(&configuration, "owned-runtime-b").unwrap();
    assert_eq!(other.path(), first.path());
    assert_eq!(other.load().unwrap(), None);
    other
        .save(ZoomPreferences {
            target_scale: 0.875,
        })
        .unwrap();
    assert_eq!(first.load().unwrap().unwrap().target_scale, 1.375);
    configuration.native.typography.size = 17.25;
    configuration.native.zoom.step = 1.25;
    let edited = PreferenceStore::for_configuration(&configuration, "owned-runtime-a").unwrap();
    assert_eq!(edited.load().unwrap().unwrap().target_scale, 1.375);
    first.save(ZoomPreferences { target_scale: 1.0 }).unwrap();
    assert_eq!(edited.load().unwrap().unwrap().target_scale, 1.0);
}

#[test]
fn corrupt_preferences_are_explicit_and_not_silently_replaced() {
    let root = tempfile::tempdir().unwrap();
    let mut configuration = resolve("");
    configuration.inputs.paths = BtermPaths::from_root(root.path().to_owned()).unwrap();
    let store = PreferenceStore::for_configuration(&configuration, "owned-runtime").unwrap();
    store.save(ZoomPreferences { target_scale: 1.25 }).unwrap();
    for contents in [
        b"not JSON".as_slice(),
        br#"{"version":999,"entries":[]}"#.as_slice(),
    ] {
        std::fs::write(store.path(), contents).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(ZoomPreferences { target_scale: 1.5 }).is_err());
        assert_eq!(std::fs::read(store.path()).unwrap(), contents);
    }
}

#[test]
fn remembered_targets_are_validated_and_restored_with_current_limits() {
    for target_scale in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0] {
        assert!(ZoomPreferences { target_scale }.validate().is_err());
    }
    for target_scale in [f64::MIN_POSITIVE, 100.01, f64::MAX] {
        ZoomPreferences { target_scale }.validate().unwrap();
    }
    let now = Instant::now();
    for (remembered, expected) in [
        (100.01, 3.0),
        (f64::MAX, 3.0),
        (f64::MIN_POSITIVE, 0.5),
        (0.01, 0.5),
        (f64::NAN, 1.0),
    ] {
        let controller =
            ZoomController::new(ZoomConfiguration::default(), Some(remembered)).unwrap();
        close(controller.sample(now), expected);
        assert!(!controller.needs_frame(now));
    }
}

#[test]
fn typography_validation_rejects_degenerate_metrics_and_unbounded_family_lists() {
    let valid = TypographyConfiguration {
        size: 15.25,
        line_height: 1.125,
        letter_spacing: 0.25,
        ..TypographyConfiguration::default()
    };
    valid.validate().unwrap();
    for size in [f32::NAN, f32::INFINITY, 0.0, 3.99, 128.01] {
        assert!(
            TypographyConfiguration {
                size,
                ..valid.clone()
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        TypographyConfiguration {
            fallback: vec!["Mono".into(); 17],
            ..valid.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        TypographyConfiguration {
            family: Some("\n".into()),
            ..valid
        }
        .validate()
        .is_err()
    );
}
