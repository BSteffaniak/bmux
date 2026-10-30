use super::*;

fn binding(view: &str) -> PaneViewBinding {
    PaneViewBinding::new(view, "authority/pane-one").unwrap()
}

fn configuration() -> ZoomConfiguration {
    let mut policy = ZoomConfiguration::default();
    policy.animation.enabled = false;
    policy
}

#[test]
fn same_source_has_independent_local_views() {
    let now = Instant::now();
    let mut state = PaneZoomState::new(configuration()).unwrap();
    let first = binding("view/first");
    let second = binding("view/second");
    state.bind(first.clone()).unwrap();
    state.bind(second.clone()).unwrap();
    state
        .apply(&first, ZoomAction::SetScale(2.0), 14.0, now)
        .unwrap();
    assert_eq!(state.sample(&first, now), Some(2.0));
    assert_eq!(state.sample(&second, now), Some(1.0));
    assert!(!state.bind(first.clone()).unwrap());
    assert_eq!(state.target_scale(&first), Some(2.0));
    assert!(state.remove(&first));
    assert!(state.apply(&first, ZoomAction::In, 14.0, now).is_err());
}

#[test]
fn restoration_requires_exact_binding_and_never_overwrites_local_intent() {
    let first = binding("view/first");
    assert!(first.verify_restore(&binding("view/second")).is_err());
    assert!(
        first
            .verify_restore(&PaneViewBinding::new("view/first", "other-pane").unwrap())
            .is_err()
    );
    let token = first.verify_restore(&first).unwrap();
    let mut state = PaneZoomState::new(configuration()).unwrap();
    assert!(state.restore(&token, 99.0).unwrap());
    assert_eq!(state.target_scale(&first), Some(3.0));
    assert!(!state.restore(&token, 1.5).unwrap());
    assert_eq!(state.target_scale(&first), Some(3.0));
    assert!(state.restore(&token, f64::NAN).is_err());
}

#[test]
fn remembering_disabled_and_reconfiguration_are_local_policy() {
    let first = binding("view/first");
    let token = first.verify_restore(&first).unwrap();
    let mut policy = configuration();
    policy.remember = false;
    let mut state = PaneZoomState::new(policy.clone()).unwrap();
    state.restore(&token, 2.0).unwrap();
    assert_eq!(state.target_scale(&first), Some(1.0));
    let now = Instant::now();
    state
        .apply(&first, ZoomAction::SetScale(2.0), 14.0, now)
        .unwrap();
    policy.max_scale = 1.5;
    state.reconfigure(policy.clone(), now).unwrap();
    assert_eq!(state.target_scale(&first), Some(1.5));
    policy.step = 1.0;
    assert!(state.reconfigure(policy, now).is_err());
    assert_eq!(state.target_scale(&first), Some(1.5));
}

#[test]
fn identities_and_live_state_are_bounded() {
    assert!(PaneViewBinding::new("", "pane").is_err());
    assert!(PaneViewBinding::new("view\n", "pane").is_err());
    assert!(PaneViewBinding::new(&"v".repeat(MAX_ID_BYTES + 1), "pane").is_err());
    let mut state = PaneZoomState::new(configuration()).unwrap();
    for index in 0..MAX_VIEWS {
        state.bind(binding(&format!("view/{index}"))).unwrap();
    }
    assert!(state.bind(binding("overflow")).is_err());
    // Existing views remain usable at capacity.
    assert!(!state.bind(binding("view/0")).unwrap());
    assert_ne!(
        binding("first").preference_key(),
        binding("second").preference_key()
    );
    assert_ne!(
        PaneViewBinding::new("ab", "c").unwrap().preference_key(),
        PaneViewBinding::new("a", "bc").unwrap().preference_key()
    );
}

#[test]
fn newly_observed_committed_scale_is_not_a_preference_restore() {
    let mut policy = configuration();
    policy.remember = false;
    let mut state = PaneZoomState::new(policy).unwrap();
    let first = binding("view/observed");
    assert!(state.bind_current(first.clone(), 1.75).unwrap());
    assert_eq!(state.target_scale(&first), Some(1.75));
    assert!(!state.bind_current(first.clone(), 1.0).unwrap());
    assert_eq!(state.target_scale(&first), Some(1.75));
}

#[test]
fn animation_is_sampled_without_committing_authority() {
    let now = Instant::now();
    let mut state = PaneZoomState::new(ZoomConfiguration::default()).unwrap();
    let first = binding("view/first");
    state.bind(first.clone()).unwrap();
    state
        .apply(&first, ZoomAction::SetScale(2.0), 14.0, now)
        .unwrap();
    assert_eq!(state.sample(&first, now), Some(1.0));
    assert_eq!(state.target_scale(&first), Some(2.0));
    assert!(state.needs_frame(now));
}
