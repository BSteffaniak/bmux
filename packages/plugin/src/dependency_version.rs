//! Cargo-style dependency requirements for plugin manifests. Independent of API/ABI negotiation.
//!
//! Parsing, matching and error messages follow `semver` 1.0.28, which this module
//! replaced. `tests/fixtures/dependency_versions.txt` freezes that crate's decisions;
//! `tools/plugin-version-oracle` regenerates it.
use std::{cmp::Ordering, fmt, str::FromStr};

/// A parsed plugin version. Build metadata does not participate in requirement matching.
#[derive(Clone, Debug)]
pub struct Version {
    core: [u64; 3],
    pre: String,
}

/// A conjunction of up to 32 Cargo-style comparators.
#[derive(Clone, Debug)]
pub struct VersionReq(Vec<Comparator>);

#[derive(Clone, Debug)]
struct Comparator {
    op: Op,
    core: [u64; 3],
    /// Number of numeric components given: `1` is 1, `1.2.*` is 2, `1.2.3` is 3.
    precision: usize,
    pre: String,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Exact,
    Greater,
    Less,
    AtLeast,
    AtMost,
    Caret,
    Tilde,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Position {
    Major,
    Minor,
    Patch,
    Pre,
    Build,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Major => "major version number",
            Self::Minor => "minor version number",
            Self::Patch => "patch version number",
            Self::Pre => "pre-release identifier",
            Self::Build => "build metadata",
        })
    }
}

/// Why a version or requirement was rejected, worded as `semver` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError(ErrorKind);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorKind {
    Empty,
    UnexpectedEnd(Position),
    UnexpectedChar(Position, char),
    UnexpectedCharAfter(Position, char),
    ExpectedCommaFound(Position, char),
    LeadingZero(Position),
    Overflow(Position),
    EmptySegment(Position),
    WildcardNotTheOnlyComparator(char),
    UnexpectedAfterWildcard,
    ExcessiveComparators,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            ErrorKind::Empty => f.write_str("empty string, expected a semver version"),
            ErrorKind::UnexpectedEnd(pos) => {
                write!(f, "unexpected end of input while parsing {pos}")
            }
            ErrorKind::UnexpectedChar(pos, ch) => {
                write!(f, "unexpected character {ch:?} while parsing {pos}")
            }
            ErrorKind::UnexpectedCharAfter(pos, ch) => {
                write!(f, "unexpected character {ch:?} after {pos}")
            }
            ErrorKind::ExpectedCommaFound(pos, ch) => {
                write!(f, "expected comma after {pos}, found {ch:?}")
            }
            ErrorKind::LeadingZero(pos) => write!(f, "invalid leading zero in {pos}"),
            ErrorKind::Overflow(pos) => write!(f, "value of {pos} exceeds u64::MAX"),
            ErrorKind::EmptySegment(pos) => write!(f, "empty identifier segment in {pos}"),
            ErrorKind::WildcardNotTheOnlyComparator(ch) => write!(
                f,
                "wildcard req ({ch}) must be the only comparator in the version req"
            ),
            ErrorKind::UnexpectedAfterWildcard => {
                f.write_str("unexpected character after wildcard in version req")
            }
            ErrorKind::ExcessiveComparators => {
                f.write_str("excessive number of version comparators")
            }
        }
    }
}

impl std::error::Error for ParseError {}

const MAX_COMPARATORS: usize = 32;

const fn error<T>(kind: ErrorKind) -> Result<T, ParseError> {
    Err(ParseError(kind))
}

/// The first character, for error reporting. Callers only pass non-empty text.
fn first(text: &str) -> char {
    text.chars().next().unwrap_or_default()
}

fn numeric(input: &str, pos: Position) -> Result<(u64, &str), ParseError> {
    let digits = input.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return error(
            input
                .chars()
                .next()
                .map_or(ErrorKind::UnexpectedEnd(pos), |ch| {
                    ErrorKind::UnexpectedChar(pos, ch)
                }),
        );
    }
    if digits > 1 && input.starts_with('0') {
        return error(ErrorKind::LeadingZero(pos));
    }
    let mut value = 0_u64;
    for digit in input[..digits].bytes() {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(digit - b'0')))
            .ok_or(ParseError(ErrorKind::Overflow(pos)))?;
    }
    Ok((value, &input[digits..]))
}

fn dot(input: &str, pos: Position) -> Result<&str, ParseError> {
    match input.strip_prefix('.') {
        Some(rest) => Ok(rest),
        None if input.is_empty() => error(ErrorKind::UnexpectedEnd(pos)),
        None => error(ErrorKind::UnexpectedCharAfter(pos, first(input))),
    }
}

fn wildcard(input: &str) -> Option<(char, &str)> {
    let ch = input
        .chars()
        .next()
        .filter(|ch| matches!(ch, '*' | 'x' | 'X'))?;
    Some((ch, &input[1..]))
}

/// Dot-separated `[0-9A-Za-z-]+` identifiers, stopping at the first other character.
/// Returns an empty identifier only when no segment starts at all.
fn identifier(input: &str, pos: Position) -> Result<(&str, &str), ParseError> {
    let mut end = 0;
    loop {
        let segment = input[end..]
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-')
            .count();
        let boundary = input.as_bytes().get(end + segment);
        if segment == 0 {
            if end == 0 && boundary != Some(&b'.') {
                return Ok(("", input));
            }
            return error(ErrorKind::EmptySegment(pos));
        }
        let text = &input[end..end + segment];
        if pos == Position::Pre
            && segment > 1
            && text.starts_with('0')
            && text.bytes().all(|b| b.is_ascii_digit())
        {
            return error(ErrorKind::LeadingZero(pos));
        }
        end += segment;
        if boundary == Some(&b'.') {
            end += 1;
        } else {
            return Ok(input.split_at(end));
        }
    }
}

/// A non-empty identifier after a `-` or `+` separator.
fn suffix(input: &str, pos: Position) -> Result<(&str, &str), ParseError> {
    let (id, rest) = identifier(input, pos)?;
    if id.is_empty() {
        return error(ErrorKind::EmptySegment(pos));
    }
    Ok((id, rest))
}

fn op(input: &str) -> Option<(Op, &str)> {
    [
        (">=", Op::AtLeast),
        ("<=", Op::AtMost),
        ("=", Op::Exact),
        (">", Op::Greater),
        ("<", Op::Less),
        ("~", Op::Tilde),
        ("^", Op::Caret),
    ]
    .into_iter()
    .find_map(|(prefix, op)| input.strip_prefix(prefix).map(|rest| (op, rest)))
}

impl Comparator {
    /// Parse one comparator; returns it, the last position parsed, and the text after it.
    fn parse(input: &str) -> Result<(Self, Position, &str), ParseError> {
        let (mut op, text, implicit) =
            op(input).map_or((Op::Caret, input, true), |(op, rest)| (op, rest, false));
        let mut text = text.trim_start_matches(' ');
        let mut pos = Position::Major;
        let mut core = [0; 3];
        let mut precision = 1;
        (core[0], text) = numeric(text, pos)?;
        let mut after_wildcard = false;
        for next in [Position::Minor, Position::Patch] {
            let Some(rest) = text.strip_prefix('.') else {
                break;
            };
            pos = next;
            if let Some((_, rest)) = wildcard(rest) {
                // `1.*` means `=1` when no operator is given; `^1.*` means `^1`.
                if implicit {
                    op = Op::Exact;
                }
                after_wildcard = true;
                text = rest;
            } else if after_wildcard {
                return error(ErrorKind::UnexpectedAfterWildcard);
            } else {
                // A numeric component never follows a wildcard, so it lands at `precision`.
                (core[precision], text) = numeric(rest, pos)?;
                precision += 1;
            }
        }
        let mut pre = "";
        if precision == 3 {
            if let Some(rest) = text.strip_prefix('-') {
                pos = Position::Pre;
                (pre, text) = suffix(rest, pos)?;
            }
            if let Some(rest) = text.strip_prefix('+') {
                pos = Position::Build;
                (_, text) = suffix(rest, pos)?;
            }
        }
        let comparator = Self {
            op,
            core,
            precision,
            pre: pre.into(),
        };
        Ok((comparator, pos, text.trim_start_matches(' ')))
    }

    fn matches(&self, v: &Version) -> bool {
        let n = self.precision;
        let core_cmp = v.core[..n].cmp(&self.core[..n]);
        // Missing components describe a prefix, not a zero-padded bound.
        let cmp = core_cmp.then_with(|| {
            if n == 3 {
                cmp_pre(&v.pre, &self.pre)
            } else {
                Ordering::Equal
            }
        });
        match self.op {
            Op::Exact => core_cmp.is_eq() && v.pre == self.pre,
            Op::Greater => cmp.is_gt(),
            Op::Less => cmp.is_lt(),
            Op::AtLeast => cmp.is_gt() || core_cmp.is_eq() && v.pre == self.pre,
            Op::AtMost => cmp.is_lt() || core_cmp.is_eq() && v.pre == self.pre,
            Op::Tilde => {
                v.core[..n.min(2)] == self.core[..n.min(2)]
                    && (core_cmp.is_gt() || core_cmp.is_eq() && !cmp_pre(&v.pre, &self.pre).is_lt())
            }
            Op::Caret => {
                let fixed = self.core[..n]
                    .iter()
                    .position(|&x| x != 0)
                    .map_or(n, |i| i + 1);
                v.core[..fixed] == self.core[..fixed]
                    && (core_cmp.is_gt()
                        || core_cmp.is_eq() && (n < 3 || !cmp_pre(&v.pre, &self.pre).is_lt()))
            }
        }
    }
}

// Numeric prerelease identifiers have arbitrary precision, unlike core numbers.
fn cmp_pre(a: &str, b: &str) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, _) | (_, true) => return a.is_empty().cmp(&b.is_empty()),
        _ => {}
    }
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (Some(x), Some(y)) => {
                let numeric = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
                let cmp = match (numeric(x), numeric(y)) {
                    (true, true) => x.len().cmp(&y.len()).then_with(|| x.cmp(y)),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => x.cmp(y),
                };
                if !cmp.is_eq() {
                    return cmp;
                }
            }
            (x, y) => return x.is_some().cmp(&y.is_some()),
        }
    }
}

impl Version {
    /// Parse a complete semantic version, validating any build metadata.
    /// # Errors
    /// Rejects malformed versions and oversized major, minor or patch numbers.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        if text.is_empty() {
            return error(ErrorKind::Empty);
        }
        let (major, text) = numeric(text, Position::Major)?;
        let (minor, text) = numeric(dot(text, Position::Major)?, Position::Minor)?;
        let (patch, mut text) = numeric(dot(text, Position::Minor)?, Position::Patch)?;
        let core = [major, minor, patch];
        let mut pos = Position::Patch;
        let mut pre = "";
        if let Some(rest) = text.strip_prefix('-') {
            pos = Position::Pre;
            (pre, text) = suffix(rest, pos)?;
        }
        if let Some(rest) = text.strip_prefix('+') {
            pos = Position::Build;
            (_, text) = suffix(rest, pos)?;
        }
        if !text.is_empty() {
            return error(ErrorKind::UnexpectedCharAfter(pos, first(text)));
        }
        Ok(Self {
            core,
            pre: pre.into(),
        })
    }
}

impl VersionReq {
    /// Parse a plugin dependency requirement.
    /// # Errors
    /// Rejects malformed or excessively long comparator lists.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut text = text.trim_start_matches(' ');
        if let Some((ch, rest)) = wildcard(text) {
            let rest = rest.trim_start_matches(' ');
            return if rest.is_empty() {
                Ok(Self(Vec::new()))
            } else if rest.starts_with(',') {
                error(ErrorKind::WildcardNotTheOnlyComparator(ch))
            } else {
                error(ErrorKind::UnexpectedAfterWildcard)
            };
        }
        let mut comparators = Vec::new();
        loop {
            let (comparator, pos, rest) = Comparator::parse(text).map_err(|mut err| {
                if let Some((ch, rest)) = wildcard(text) {
                    let rest = rest.trim_start_matches(' ');
                    if rest.is_empty() || rest.starts_with(',') {
                        err = ParseError(ErrorKind::WildcardNotTheOnlyComparator(ch));
                    }
                }
                err
            })?;
            comparators.push(comparator);
            if rest.is_empty() {
                return Ok(Self(comparators));
            }
            let Some(rest) = rest.strip_prefix(',') else {
                return error(ErrorKind::ExpectedCommaFound(pos, first(rest)));
            };
            if comparators.len() == MAX_COMPARATORS {
                return error(ErrorKind::ExcessiveComparators);
            }
            text = rest.trim_start_matches(' ');
        }
    }

    /// Prereleases require an explicit prerelease comparator with the same numeric triple.
    #[must_use]
    pub fn matches(&self, v: &Version) -> bool {
        self.0.iter().all(|c| c.matches(v))
            && (v.pre.is_empty() || self.0.iter().any(|c| c.core == v.core && !c.pre.is_empty()))
    }
}

impl FromStr for VersionReq {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl FromStr for Version {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::{Version, VersionReq};

    fn matches(req: &str, version: &str) -> bool {
        VersionReq::parse(req)
            .unwrap()
            .matches(&Version::parse(version).unwrap())
    }

    #[test]
    fn caret_and_tilde_follow_cargo_compatibility_ranges() {
        assert!(matches("1.2", "1.9.0"));
        assert!(!matches("1.2", "2.0.0"));
        assert!(matches("^0.2.3", "0.2.9"));
        assert!(!matches("^0.2.3", "0.3.0"));
        assert!(!matches("^0.0.3", "0.0.4"));
        assert!(matches("~1.2.3", "1.2.9"));
        assert!(!matches("~1.2.3", "1.3.0"));
        assert!(matches("1.*", "1.7.0"));
        assert!(matches("*", "9.0.0"));
    }

    #[test]
    fn prereleases_need_an_explicit_matching_triple() {
        assert!(matches(">=0.0.1-alpha.0", "0.0.1-alpha.1"));
        assert!(matches(">=0.0.1-alpha.0", "0.1.0"));
        assert!(!matches(">=0.0.1-alpha.0", "0.1.0-alpha.0"));
        assert!(!matches("*", "1.0.0-rc.1"));
        assert!(matches("^1.0.0-alpha.2", "1.0.0-alpha.10"));
        assert!(matches("1.2.3+build", "1.2.3+other"));
    }

    #[test]
    fn errors_name_the_offending_position_and_character() {
        let version = |s| Version::parse(s).unwrap_err().to_string();
        let req = |s| VersionReq::parse(s).unwrap_err().to_string();
        assert_eq!(
            version("v1.2.3"),
            "unexpected character 'v' while parsing major version number"
        );
        assert_eq!(
            version("1.2"),
            "unexpected end of input while parsing minor version number"
        );
        assert_eq!(
            version("1.02.0"),
            "invalid leading zero in minor version number"
        );
        assert_eq!(
            version("1.2.3-"),
            "empty identifier segment in pre-release identifier"
        );
        assert_eq!(
            req(">=1.2 <2"),
            "expected comma after minor version number, found '<'"
        );
        assert_eq!(
            req("*, >1"),
            "wildcard req (*) must be the only comparator in the version req"
        );
        assert_eq!(
            req(&vec!["^0"; 33].join(",")),
            "excessive number of version comparators"
        );
    }
}
