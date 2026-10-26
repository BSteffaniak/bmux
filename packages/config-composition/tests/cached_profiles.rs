use bmux_config_composition::*;
use std::collections::BTreeMap;
fn value(text: &str) -> ConfigValue {
    TomlCodec::parse(text).unwrap()
}
fn merge(old: &mut ConfigValue, new: ConfigValue) -> Result<()> {
    old.overlay_replace(new);
    Ok(())
}
#[test]
fn cached_and_uncached_results_match_ordered_diamonds_and_duplicate_parents() {
    let profiles = BTreeMap::from([
        (
            "base".into(),
            Profile {
                parents: vec![],
                value: value("base=true\nitems=[1]\n[a]\nx=1\ny=2"),
            },
        ),
        (
            "left".into(),
            Profile {
                parents: vec!["base".into()],
                value: value("items=[2]\n[a]\nx=3"),
            },
        ),
        (
            "right".into(),
            Profile {
                parents: vec!["base".into()],
                value: value("[a]\ny=4"),
            },
        ),
        (
            "child".into(),
            Profile {
                parents: vec!["left".into(), "right".into(), "left".into()],
                value: value("child=true"),
            },
        ),
    ]);
    let uncached = resolve_profile("child", &profiles, ProfileLimits::default(), merge).unwrap();
    let cached =
        resolve_profile_cached("child", &profiles, ProfileLimits::default(), merge).unwrap();
    assert_eq!(cached, uncached);
    assert_eq!(
        cached,
        value("base=true\nchild=true\nitems=[2]\n[a]\nx=3\ny=2")
    );
}
#[test]
fn repeated_parent_graph_has_linear_callback_bound_not_exponential_expansion() {
    let count = 1000;
    let mut profiles = BTreeMap::new();
    for index in 0..count {
        profiles.insert(
            index.to_string(),
            Profile {
                parents: if index == 0 {
                    vec![]
                } else {
                    vec![(index - 1).to_string(); 2]
                },
                value: value(&format!("winner={index}")),
            },
        );
    }
    let mut callbacks = 0;
    let result = resolve_profile_cached(
        &(count - 1).to_string(),
        &profiles,
        ProfileLimits {
            max_depth: count,
            max_expansions: count,
        },
        |old, new| {
            callbacks += 1;
            merge(old, new)
        },
    )
    .unwrap();
    assert_eq!(result, value("winner=999"));
    assert_eq!(callbacks, 2 * (count - 1));
}
#[test]
fn cached_subgraph_height_cannot_bypass_depth_limit() {
    // Resolve shared first through the short path, then reuse it through a longer
    // path. Cached reuse must account for its complete inherited height.
    let profile = |parents: Vec<&str>| Profile {
        parents: parents.into_iter().map(str::to_owned).collect(),
        value: 0,
    };
    let profiles = BTreeMap::from([
        ("leaf".into(), profile(vec![])),
        ("shared".into(), profile(vec!["leaf"])),
        ("middle".into(), profile(vec!["shared"])),
        ("root".into(), profile(vec!["shared", "middle"])),
    ]);
    let merge = |_: &mut i32, _| Ok(());
    assert_eq!(
        resolve_profile_cached(
            "root",
            &profiles,
            ProfileLimits {
                max_depth: 3,
                max_expansions: 10
            },
            merge
        )
        .unwrap_err()
        .kind,
        ErrorKind::Limit
    );
    assert!(
        resolve_profile_cached(
            "root",
            &profiles,
            ProfileLimits {
                max_depth: 4,
                max_expansions: 4
            },
            merge
        )
        .is_ok()
    );
    assert_eq!(
        resolve_profile_cached(
            "root",
            &profiles,
            ProfileLimits {
                max_depth: 4,
                max_expansions: 3
            },
            merge
        )
        .unwrap_err()
        .kind,
        ErrorKind::Limit
    );
}
#[test]
fn cached_errors_remain_structured_and_cycle_detection_is_not_memoized_away() {
    let mut profiles = BTreeMap::from([(
        "root".into(),
        Profile {
            parents: vec!["missing".into()],
            value: 0,
        },
    )]);
    let merge = |_: &mut i32, _| Ok(());
    assert_eq!(
        resolve_profile_cached("root", &profiles, ProfileLimits::default(), merge)
            .unwrap_err()
            .kind,
        ErrorKind::UnknownProfile
    );
    profiles.get_mut("root").unwrap().parents = vec!["root".into()];
    assert_eq!(
        resolve_profile_cached("root", &profiles, ProfileLimits::default(), merge)
            .unwrap_err()
            .kind,
        ErrorKind::ProfileCycle
    );
    assert_eq!(
        resolve_profile_cached(
            "root",
            &profiles,
            ProfileLimits {
                max_depth: 0,
                max_expansions: 0
            },
            merge
        )
        .unwrap_err()
        .kind,
        ErrorKind::Limit
    );
}
