//! Cargo-style dependency requirements for plugin manifests. Independent of API/ABI negotiation.
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

#[derive(Clone, Copy, Debug)]
pub struct ParseError(&'static str);
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ParseError {}

fn numeric(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}
fn wildcard(s: &str) -> bool {
    matches!(s, "*" | "x" | "X")
}

fn identifiers(s: &str, prerelease: bool) -> Result<(), ParseError> {
    for id in s.split('.') {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(ParseError("invalid version identifier"));
        }
        if prerelease && id.len() > 1 && id.starts_with('0') && numeric(id) {
            return Err(ParseError("leading zero in numeric prerelease identifier"));
        }
    }
    Ok(())
}

impl Comparator {
    fn parse(s: &str, op: Op, implicit: bool) -> Result<Self, ParseError> {
        let (left, build) = s.split_once('+').map_or((s, None), |(a, b)| (a, Some(b)));
        let (core, pre) = left
            .split_once('-')
            .map_or((left, None), |(a, b)| (a, Some(b)));
        let mut result = Self {
            op,
            core: [0; 3],
            precision: 0,
            pre: String::new(),
        };
        for (i, n) in core.split('.').enumerate() {
            if i >= 3 {
                return Err(ParseError("too many numeric components"));
            }
            if wildcard(n) && i > 0 {
                if implicit {
                    result.op = Op::Exact;
                }
                continue;
            }
            if i != result.precision || !numeric(n) || (n.len() > 1 && n.starts_with('0')) {
                return Err(ParseError("invalid numeric version component"));
            }
            result.core[i] = n
                .parse()
                .map_err(|_| ParseError("version component exceeds u64"))?;
            result.precision += 1;
        }
        if pre.is_some() || build.is_some() {
            if result.precision != 3 {
                return Err(ParseError("version suffix requires major.minor.patch"));
            }
            if let Some(p) = pre {
                identifiers(p, true)?;
                result.pre = p.into();
            }
            if let Some(b) = build {
                identifiers(b, false)?;
            }
        }
        Ok(result)
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
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        let c = Comparator::parse(s, Op::Exact, false)?;
        if c.precision != 3 {
            return Err(ParseError("expected major.minor.patch"));
        }
        Ok(Self {
            core: c.core,
            pre: c.pre,
        })
    }
}
impl VersionReq {
    /// Parse a plugin dependency requirement.
    /// # Errors
    /// Rejects malformed or excessively long comparator lists.
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        let s = s.trim_matches(' ');
        if wildcard(s) {
            return Ok(Self(Vec::new()));
        }
        let mut items = Vec::new();
        for part in s.split(',') {
            if items.len() == 32 {
                return Err(ParseError("too many comparators"));
            }
            let part = part.trim_matches(' ');
            let op = [
                (">=", Op::AtLeast),
                ("<=", Op::AtMost),
                ("=", Op::Exact),
                (">", Op::Greater),
                ("<", Op::Less),
                ("~", Op::Tilde),
                ("^", Op::Caret),
            ]
            .into_iter()
            .find_map(|(p, op)| {
                part.strip_prefix(p)
                    .map(|rest| (rest.trim_start_matches(' '), op, false))
            })
            .unwrap_or((part, Op::Caret, true));
            items.push(Comparator::parse(op.0, op.1, op.2)?);
        }
        Ok(Self(items))
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
