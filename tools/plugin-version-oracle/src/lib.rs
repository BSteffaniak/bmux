//! Run with `cargo test --locked --manifest-path tools/plugin-version-oracle/Cargo.toml`.
#[cfg(test)]
#[path = "../../../packages/plugin/src/dependency_version.rs"]
mod candidate;

#[test]
fn plugin_compatibility_decisions() {
    use std::{collections::BTreeSet, fs, path::Path};
    let mut versions = BTreeSet::new();
    let mut requirements = BTreeSet::new();
    for item in fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins")).unwrap() {
        let manifest = item.unwrap().path().join("plugin.toml");
        if !manifest.exists() {
            continue;
        }
        for line in fs::read_to_string(manifest).unwrap().lines() {
            if let Some((key, value)) = line.split_once('=') {
                let dest = match key.trim() {
                    "version" => &mut versions,
                    "version_req" => &mut requirements,
                    _ => continue,
                };
                dest.insert(value.trim().trim_matches('"').to_owned());
            }
        }
    }
    assert!(requirements.contains(">=0.0.1-alpha.0"));
    for major in 0..3 {
        for minor in 0..3 {
            for patch in 0..3 {
                for suffix in [
                    "",
                    "+001",
                    "-0",
                    "-alpha.2",
                    "-alpha.10",
                    "-alpha",
                    "-999999999999999999999999",
                    "-a-b+c.01",
                ] {
                    versions.insert(format!("{major}.{minor}.{patch}{suffix}"));
                }
            }
        }
    }
    for text in [
        "*",
        "x",
        "X",
        "",
        "1..0",
        "18446744073709551615.0.0",
        "18446744073709551616.0.0",
        "1.2.03",
        "1.2.3-01",
        "1.2.3+a..b",
        "1.2.3-ß",
        "1.2.3\n",
        "1.2.3-",
        "1.2.3+",
        "1.2.3.4",
        "v1.2.3",
        "1.2.3+a+b",
    ] {
        versions.insert(text.into());
        requirements.insert(text.into());
    }
    for n in [31, 32, 33] {
        requirements.insert(vec!["^0"; n].join(","));
    }
    for op in ["", "=", ">", ">=", "<", "<=", "~", "^", "!="] {
        for body in [
            "0",
            "1",
            "0.0",
            "1.1",
            "0.2",
            "0.0.1",
            "1.1.1",
            "0.*",
            "1.2.X",
            "1.x.*",
            "1.x.0",
            "0.0.1-alpha",
            "0.0.1+abc",
            "1.1.1-0",
            "1.2+abc",
            "*",
            "01.0",
            "",
            " 1.1",
            "\t1.1",
            "1 || 2",
            "1 - 2",
        ] {
            let r = format!("{op}{body}");
            for extra in ["", ", >=0.0.1-0", ", <1.1.1-alpha", ",", ", *"] {
                requirements.insert(format!("{r}{extra}"));
            }
        }
    }
    let versions = versions
        .iter()
        .filter_map(|s| {
            let a = candidate::Version::parse(s);
            let b = semver::Version::parse(s);
            assert_eq!(a.is_ok(), b.is_ok(), "version {s:?}");
            Some((s, a.ok()?, b.ok()?))
        })
        .collect::<Vec<_>>();
    for r in requirements {
        let a = candidate::VersionReq::parse(&r);
        let b = semver::VersionReq::parse(&r);
        assert_eq!(a.is_ok(), b.is_ok(), "requirement {r:?}");
        if let (Ok(a), Ok(b)) = (a, b) {
            for (s, x, y) in &versions {
                assert_eq!(a.matches(x), b.matches(y), "{r:?} vs {s:?}");
            }
        }
    }
}
