use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ApiVersion {
    pub major: u16,
    pub minor: u16,
}

impl ApiVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    #[must_use]
    pub fn is_compatible_with(self, minimum: Self, maximum: Option<Self>) -> bool {
        if self < minimum {
            return false;
        }
        if let Some(maximum) = maximum {
            self <= maximum
        } else {
            true
        }
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl FromStr for ApiVersion {
    type Err = String;

    /// Parse an `ApiVersion` from a string in `"major.minor"` or `"major"` format.
    ///
    /// When the minor component is omitted it defaults to `0`.
    ///
    /// # Errors
    ///
    /// Returns a descriptive error string if the input is empty, contains
    /// non-numeric components, or has more than two dot-separated parts.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err("version must not be empty".to_string());
        }

        let mut parts = value.split('.');
        let major = parts
            .next()
            .ok_or_else(|| "missing major version".to_string())?
            .parse::<u16>()
            .map_err(|_| format!("invalid major version '{value}'"))?;
        let minor = match parts.next() {
            Some(part) => part
                .parse::<u16>()
                .map_err(|_| format!("invalid minor version '{value}'"))?,
            None => 0,
        };

        if parts.next().is_some() {
            return Err(format!("invalid version '{value}'"));
        }

        Ok(Self { major, minor })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRange {
    pub minimum: ApiVersion,
    pub maximum: Option<ApiVersion>,
}

impl VersionRange {
    #[must_use]
    pub const fn at_least(minimum: ApiVersion) -> Self {
        Self {
            minimum,
            maximum: None,
        }
    }

    #[must_use]
    pub const fn bounded(minimum: ApiVersion, maximum: ApiVersion) -> Self {
        Self {
            minimum,
            maximum: Some(maximum),
        }
    }

    #[must_use]
    pub fn contains(self, version: ApiVersion) -> bool {
        version.is_compatible_with(self.minimum, self.maximum)
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.maximum {
            Some(maximum) => write!(f, "{}..={maximum}", self.minimum),
            None => write!(f, "{}+", self.minimum),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiVersion, VersionRange};

    #[test]
    fn parses_single_component_version() {
        let version: ApiVersion = "1".parse().expect("version should parse");
        assert_eq!(version, ApiVersion::new(1, 0));
    }

    #[test]
    fn parses_major_minor_version() {
        let version: ApiVersion = "2.7".parse().expect("version should parse");
        assert_eq!(version, ApiVersion::new(2, 7));
    }

    #[test]
    fn range_contains_only_supported_versions() {
        let range = VersionRange::bounded(ApiVersion::new(1, 1), ApiVersion::new(1, 4));
        assert!(range.contains(ApiVersion::new(1, 1)));
        assert!(range.contains(ApiVersion::new(1, 4)));
        assert!(!range.contains(ApiVersion::new(1, 0)));
        assert!(!range.contains(ApiVersion::new(1, 5)));
    }
}
