//! Replays `semver` 1.0.28 decisions frozen by `tools/plugin-version-oracle`.
use bmux_plugin::dependency_version::{Version, VersionReq};

const FIXTURE: &str = include_str!("fixtures/dependency_versions.txt");

fn unhex(text: &str) -> String {
    let bytes = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn parse_errors_and_matches_agree_with_frozen_semver_decisions() {
    let mut versions = Vec::new();
    let (mut version_rows, mut requirement_rows, mut decisions) = (0, 0, 0);
    for line in FIXTURE.lines().filter(|line| !line.starts_with('#')) {
        let mut fields = line.splitn(4, ' ');
        let (kind, input, status) = (fields.next(), fields.next(), fields.next());
        let (Some(kind), Some(input), Some(status)) = (kind, input, status) else {
            panic!("malformed fixture row {line:?}");
        };
        let detail = fields.next().unwrap_or_default();
        let input = unhex(input);
        match (kind, status) {
            ("V", "ok") => {
                versions.push(
                    Version::parse(&input).unwrap_or_else(|e| panic!("version {input:?}: {e}")),
                );
                version_rows += 1;
            }
            ("V", "err") => {
                let error = Version::parse(&input).expect_err(&input).to_string();
                assert_eq!(error, detail, "version {input:?}");
                version_rows += 1;
            }
            ("R", "ok") => {
                let req = VersionReq::parse(&input)
                    .unwrap_or_else(|e| panic!("requirement {input:?}: {e}"));
                assert_eq!(detail.len(), versions.len().div_ceil(4), "row {input:?}");
                for (i, version) in versions.iter().enumerate() {
                    let nibble = detail.as_bytes()[i / 4];
                    let nibble = char::from(nibble).to_digit(16).unwrap();
                    let expected = nibble & (8 >> (i % 4)) != 0;
                    assert_eq!(req.matches(version), expected, "{input:?} vs version #{i}");
                    decisions += 1;
                }
                requirement_rows += 1;
            }
            ("R", "err") => {
                let error = VersionReq::parse(&input).expect_err(&input).to_string();
                assert_eq!(error, detail, "requirement {input:?}");
                requirement_rows += 1;
            }
            _ => panic!("malformed fixture row {line:?}"),
        }
    }
    // Guard against a truncated or emptied fixture silently passing.
    assert!(versions.len() > 200 && version_rows > 400, "{version_rows}");
    assert!(requirement_rows > 2000 && decisions > 90_000, "{decisions}");
}
