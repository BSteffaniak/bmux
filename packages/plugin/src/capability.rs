use crate::{PluginError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HostScope(String);

impl HostScope {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if is_valid_capability_id(&value) {
            Ok(Self(value))
        } else {
            Err(PluginError::InvalidHostScope { scope: value })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    #[must_use]
    pub fn is_hot_path(&self) -> bool {
        matches!(
            self.as_str(),
            "bmux.terminal.input_intercept" | "bmux.terminal.output_intercept"
        )
    }
}

impl fmt::Display for HostScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PluginFeature(String);

impl PluginFeature {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if is_valid_capability_id(&value) {
            Ok(Self(value))
        } else {
            Err(PluginError::InvalidPluginFeature { feature: value })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for PluginFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn is_valid_capability_id(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::{HostScope, PluginFeature};

    #[test]
    fn host_scope_requires_stable_ascii_format() {
        assert!(HostScope::new("bmux.sessions.read").is_ok());
        assert!(HostScope::new("Bmux.Sessions.Read").is_err());
    }

    #[test]
    fn plugin_feature_requires_stable_ascii_format() {
        assert!(PluginFeature::new("acme.timeline").is_ok());
        assert!(PluginFeature::new("Acme.Timeline").is_err());
    }
}
