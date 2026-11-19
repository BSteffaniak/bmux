//! Frozen `semver` 1.0.28 decisions for `bmux_plugin::dependency_version`.
//!
//! `cargo run --locked > ../../packages/plugin/tests/fixtures/dependency_versions.txt`
//! regenerates the fixture; `cargo test --locked` checks that it is current and also
//! compares the candidate directly. The fixture generator never uses candidate code.
use std::collections::BTreeSet;
use std::fmt::Write as _;

#[cfg(test)]
#[path = "../../../packages/plugin/src/dependency_version.rs"]
mod candidate;

const SUFFIXES: [&str; 8] = [
    "",
    "+001",
    "-0",
    "-alpha.2",
    "-alpha.10",
    "-alpha",
    "-999999999999999999999999",
    "-a-b+c.01",
];

/// Malformed inputs tried as both versions and requirements.
const EDGES: [&str; 17] = [
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
];

const OPS: [&str; 9] = ["", "=", ">", ">=", "<", "<=", "~", "^", "!="];

const BODIES: [&str; 22] = [
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
];

const EXTRAS: [&str; 5] = ["", ", >=0.0.1-0", ", <1.1.1-alpha", ",", ", *"];

/// Deterministic xorshift, so the corpus never depends on the host or a seed file.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        let len = u64::try_from(items.len()).unwrap();
        items[usize::try_from(self.next() % len).unwrap()]
    }

    fn version(&mut self, numbers: &[&str]) -> String {
        let count = 1 + self.next() % 4;
        let mut text = (0..count)
            .map(|_| self.pick(numbers))
            .collect::<Vec<_>>()
            .join(".");
        let ids = ["0", "1", "10", "01", "alpha", "rc", "x-y", "", "a b"];
        if self.next().is_multiple_of(3) {
            text = format!("{text}-{}.{}", self.pick(&ids), self.pick(&ids));
        }
        if self.next().is_multiple_of(5) {
            text = format!("{text}+{}", self.pick(&ids));
        }
        text
    }
}

fn corpus() -> (BTreeSet<String>, BTreeSet<String>) {
    let mut versions = BTreeSet::new();
    let mut requirements = BTreeSet::new();
    for major in 0..3 {
        for minor in 0..3 {
            for patch in 0..3 {
                for suffix in SUFFIXES {
                    versions.insert(format!("{major}.{minor}.{patch}{suffix}"));
                }
            }
        }
    }
    for text in EDGES {
        versions.insert(text.into());
        requirements.insert(text.into());
    }
    requirements.insert(">=0.0.1-alpha.0".into());
    for n in [31, 32, 33] {
        requirements.insert(vec!["^0"; n].join(","));
    }
    requirements.insert(format!("{},", vec!["^0"; 32].join(",")));
    for op in OPS {
        for body in BODIES {
            for extra in EXTRAS {
                requirements.insert(format!("{op}{body}{extra}"));
            }
        }
    }
    let mut rng = Rng(0x5eed_0f01_2345_6789);
    let numbers = ["0", "1", "2", "10", "00", "01", "*", "x", "", " 1"];
    for _ in 0..300 {
        versions.insert(rng.version(&numbers[..4]));
    }
    for _ in 0..1500 {
        let count = 1 + rng.next() % 3;
        let req = (0..count)
            .map(|_| {
                let op = rng.pick(&OPS[..8]);
                let space = rng.pick(&["", "", " "]);
                format!("{op}{space}{}", rng.version(&numbers))
            })
            .collect::<Vec<_>>()
            .join(rng.pick(&[",", ", ", " ,", " "]));
        requirements.insert(req);
    }
    (versions, requirements)
}

fn hex(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x}").unwrap();
        out
    })
}

/// The fixture text, computed only with `semver`.
///
/// # Panics
/// Never in practice: formatting into a `String` cannot fail.
#[must_use]
pub fn fixture() -> String {
    let (versions, requirements) = corpus();
    let mut out = String::from(
        "# semver 1.0.28 decisions. Regenerate with tools/plugin-version-oracle.\n\
         # Inputs are hex-encoded UTF-8. `V <input> ok` or `V <input> err <message>`.\n\
         # `R <input> ok <bits>`: hex, most significant bit first, one bit per valid\n\
         # version in listed order, set when the requirement matches it.\n",
    );
    let mut valid = Vec::new();
    for text in &versions {
        match semver::Version::parse(text) {
            Ok(version) => {
                writeln!(out, "V {} ok", hex(text)).unwrap();
                valid.push(version);
            }
            Err(error) => writeln!(out, "V {} err {error}", hex(text)).unwrap(),
        }
    }
    for text in &requirements {
        match semver::VersionReq::parse(text) {
            Ok(req) => {
                let bits = valid
                    .chunks(4)
                    .map(|chunk| {
                        let nibble = chunk
                            .iter()
                            .enumerate()
                            .filter(|(_, version)| req.matches(version))
                            .fold(0_u32, |acc, (i, _)| acc | (8 >> i));
                        char::from_digit(nibble, 16).unwrap()
                    })
                    .collect::<String>();
                writeln!(out, "R {} ok {bits}", hex(text)).unwrap();
            }
            Err(error) => writeln!(out, "R {} err {error}", hex(text)).unwrap(),
        }
    }
    out
}

#[test]
fn fixture_is_current() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packages/plugin/tests/fixtures/dependency_versions.txt"
    );
    assert!(
        std::fs::read_to_string(path).unwrap() == fixture(),
        "stale fixture; regenerate with `cargo run --locked > {path}`"
    );
}

/// Exact parse outcomes, error text and match decisions, including real plugin manifests.
#[test]
fn candidate_matches_semver() {
    use std::{fs, path::Path};
    let (mut versions, mut requirements) = corpus();
    let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
    for item in fs::read_dir(plugins).unwrap() {
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
    let versions = versions
        .iter()
        .filter_map(|s| {
            let a = candidate::Version::parse(s).map_err(|e| e.to_string());
            let b = semver::Version::parse(s).map_err(|e| e.to_string());
            assert_eq!(a.as_ref().err(), b.as_ref().err(), "version {s:?}");
            Some((s, a.ok()?, b.ok()?))
        })
        .collect::<Vec<_>>();
    for r in requirements {
        let a = candidate::VersionReq::parse(&r).map_err(|e| e.to_string());
        let b = semver::VersionReq::parse(&r).map_err(|e| e.to_string());
        assert_eq!(a.as_ref().err(), b.as_ref().err(), "requirement {r:?}");
        if let (Ok(a), Ok(b)) = (a, b) {
            for (s, x, y) in &versions {
                assert_eq!(a.matches(x), b.matches(y), "{r:?} vs {s:?}");
            }
        }
    }
}
