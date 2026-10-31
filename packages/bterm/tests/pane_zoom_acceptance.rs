//! Public Bterm pane-view contracts, with owned configuration and preference IO.
//! These are not native-window, PTY, or production attach-bridge acceptance.
//!
//! Native checklist (not automated evidence): use two panes and two views of the
//! same session; record PTY sizes before/after normal zoom; exercise alternate
//! screen ownership, history selection/copy of `e\u{301}界👩‍💻`, images under
//! overlays, keyboard-focus vs hovered fractional wheel, rejected output/retry,
//! and named-view reopen. On Linux repeat under Wayland and X11. Move a window
//! between differently scaled monitors while animating; inspect cursor, image,
//! selection, IME candidate placement and accessibility bounds against paint.
//! Repeat with reduced motion and a screen reader. Do not infer these results
//! from the headless tests here or the synthetic zoom_window_acceptance example.
//!
//! Manual executable entry (only after building and verifying an isolated test
//! runtime, never an active user root):
//! `bterm --runtime-root <owned-root> --session <owned-session-uuid>
//! --view-binding acceptance-window-a --no-bmux-config --config <owned-config>`.
//! Open a second window with `acceptance-window-b`, then reopen window-a with
//! the same explicit binding to check restore. The root must contain all four
//! isolated runtime/config/data/state directories. Capture platform, compositor,
//! monitor scales, executable identity and observed results; a successful launch
//! alone is not acceptance. This file does not launch that command.

use std::{collections::BTreeMap, time::Instant};

use bterm::{
    configuration::{ConfigOptions, ResolvedConfiguration, SourcePolicy},
    pane_zoom::{PaneViewBinding, PaneZoomState},
    runtime_paths::BtermPaths,
    zoom::{ZoomAction, ZoomConfiguration, ZoomInputMatcher, ZoomScope},
    zoom_preferences::{PreferenceStore, ZoomPreferences},
};
use bterm_native_platform::input::{
    InputEvent, KeyEvent, KeyPhase, LogicalKey, Modifiers, PhysicalKey, ScrollDelta,
};

fn configuration(root: &std::path::Path) -> ResolvedConfiguration {
    ConfigOptions {
        policy: SourcePolicy::Explicit,
        ..ConfigOptions::default()
    }
    .capture_selected_with_environment(
        BtermPaths::from_root(root.to_owned()).unwrap(),
        root.to_owned(),
        BTreeMap::new(),
    )
    .unwrap()
    .resolve()
    .unwrap()
}

fn binding(view: &str, pane: &str) -> PaneViewBinding {
    PaneViewBinding::new(view, pane).unwrap()
}

fn policy() -> ZoomConfiguration {
    let mut policy = ZoomConfiguration::default();
    policy.animation.enabled = false;
    policy
}

fn primary() -> Modifiers {
    Modifiers {
        super_key: cfg!(target_os = "macos"),
        control: !cfg!(target_os = "macos"),
        ..Modifiers::default()
    }
}

#[test]
fn named_view_restore_requires_actual_source_and_does_not_overwrite_local_intent() {
    let root = tempfile::tempdir().unwrap();
    let configuration = configuration(root.path());
    let store = PreferenceStore::for_configuration(&configuration, "owned/named-runtime").unwrap();
    let original = binding("window/left", "authority/session/pane-a");
    let token = original.verify_restore(&original).unwrap();
    store
        .save_pane(
            &token,
            ZoomPreferences {
                target_scale: 1.375,
            },
        )
        .unwrap();
    for unrelated in [
        binding("window/right", "authority/session/pane-a"),
        binding("window/left", "authority/session/pane-b"),
        binding("window/left", "other-authority/session/pane-a"),
    ] {
        assert!(original.verify_restore(&unrelated).is_err());
        let other = unrelated.verify_restore(&unrelated).unwrap();
        assert_eq!(store.load_pane(&other).unwrap(), None);
    }
    let other_runtime =
        PreferenceStore::for_configuration(&configuration, "owned/other-runtime").unwrap();
    assert_eq!(other_runtime.load_pane(&token).unwrap(), None);
    let saved = store.load_pane(&token).unwrap().unwrap().target_scale;
    let now = Instant::now();
    let mut reopened = PaneZoomState::new(policy()).unwrap();
    assert!(reopened.restore(&token, saved).unwrap());
    reopened
        .apply(&original, ZoomAction::SetScale(1.625), 16.0, now)
        .unwrap();
    assert!(!reopened.restore(&token, saved).unwrap());
    assert_eq!(reopened.target_scale(&original), Some(1.625));
    assert!(reopened.remove(&original));
    assert_eq!(
        store.load_pane(&token).unwrap().unwrap().target_scale,
        1.375
    );
}

#[test]
fn persistence_contains_logical_target_not_dpi_window_scale_or_animation_sample() {
    let root = tempfile::tempdir().unwrap();
    let mut configuration = configuration(root.path());
    let store = PreferenceStore::for_configuration(&configuration, "owned/runtime").unwrap();
    let view = binding("named/window", "authority/pane");
    let token = view.verify_restore(&view).unwrap();
    let now = Instant::now();
    let mut state = PaneZoomState::new(ZoomConfiguration::default()).unwrap();
    state.bind(view.clone()).unwrap();
    state
        .apply(&view, ZoomAction::SetScale(1.75), 16.0, now)
        .unwrap();
    assert_eq!(state.sample(&view, now), Some(1.0));
    store
        .save_pane(
            &token,
            ZoomPreferences {
                target_scale: state.target_scale(&view).unwrap(),
            },
        )
        .unwrap();
    // Native typography edits must not invalidate this runtime/config namespace.
    configuration.native.typography.size = 19.25;
    let edited = PreferenceStore::for_configuration(&configuration, "owned/runtime").unwrap();
    for dpi in [1.0, 1.25, 1.5, 2.0] {
        for window_scale in [0.75, 1.0, 1.375] {
            let target = edited.load_pane(&token).unwrap().unwrap().target_scale;
            assert_eq!(target, 1.75);
            // This arithmetic is a logical persistence contract, not monitor evidence.
            let physical_size = 19.25 * target * window_scale * dpi;
            assert!((physical_size / dpi / window_scale - 19.25 * target).abs() < 1e-10);
        }
    }
    assert_eq!(edited.load().unwrap().unwrap().target_scale, 1.0);
}

#[test]
fn fractional_scoped_actions_preserve_distinct_local_view_state() {
    let now = Instant::now();
    let mut policy = policy();
    for binding in &mut policy.bindings {
        binding.scope = ZoomScope::Pane;
    }
    policy.wheel.scope = ZoomScope::Pane;
    policy.wheel.modifiers = vec![bterm::zoom::ZoomModifier::Primary];
    let mut matcher = ZoomInputMatcher::new(&policy).unwrap();
    let mut state = PaneZoomState::new(policy).unwrap();
    let focused = binding("window/one/view-a", "authority/pane-a");
    let hovered = binding("window/one/view-b", "authority/pane-b");
    let other_window = binding("window/two/view-a", "authority/pane-a");
    for view in [&focused, &hovered, &other_window] {
        state.bind(view.clone()).unwrap();
    }
    let key = InputEvent::Key(KeyEvent {
        physical: PhysicalKey::Code("equal".into()),
        logical: LogicalKey::Character("=".into()),
        phase: KeyPhase::Press,
        modifiers: primary(),
        text: None,
    });
    let action = matcher.input(&key).unwrap();
    assert_eq!(action.scope, ZoomScope::Pane);
    state.apply(&focused, action.action, 16.0, now).unwrap();
    let wheel = matcher
        .input(&InputEvent::Scroll {
            delta: ScrollDelta::Lines { x: 0.0, y: 0.25 },
            modifiers: primary(),
        })
        .unwrap();
    assert_eq!(wheel.scope, ZoomScope::Pane);
    let ZoomAction::Steps(steps) = wheel.action else {
        panic!("fractional wheel lost");
    };
    assert!(steps > 0.0 && steps < 1.0);
    state.apply(&hovered, wheel.action, 16.0, now).unwrap();
    assert!(state.target_scale(&hovered).unwrap() > 1.0);
    assert!(state.target_scale(&hovered).unwrap() < state.target_scale(&focused).unwrap());
    assert_eq!(state.target_scale(&other_window), Some(1.0));
    // Caller chooses these bindings; production focus/hover routing needs the CLI fixture.
    assert!(
        matcher
            .input(&InputEvent::Scroll {
                delta: ScrollDelta::Lines { x: 0.0, y: 0.25 },
                modifiers: Modifiers::default(),
            })
            .is_none()
    );
}

#[test]
fn invalid_policy_is_atomic_and_live_view_work_is_bounded() {
    let mut state = PaneZoomState::new(policy()).unwrap();
    let now = Instant::now();
    for index in 0..256 {
        state
            .bind(binding(&format!("view/{index}"), "authority/pane"))
            .unwrap();
    }
    assert!(state.bind(binding("overflow", "authority/pane")).is_err());
    let view = binding("view/0", "authority/pane");
    state
        .apply(&view, ZoomAction::SetScale(1.5), 16.0, now)
        .unwrap();
    let mut invalid = policy();
    invalid.max_scale = f64::NAN;
    assert!(state.reconfigure(invalid, now).is_err());
    assert_eq!(state.target_scale(&view), Some(1.5));
    assert!(state.remove(&view));
    assert!(
        state
            .bind(binding("replacement", "authority/pane"))
            .unwrap()
    );
}
