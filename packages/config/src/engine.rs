//! TOML compatibility adapters. Selection and scope policy remain BMUX-owned;
//! recursive merge and ordered inheritance are shared engine operations.
use bmux_config_composition::{ConfigValue, ErrorKind, Profile, ProfileLimits, TomlCodec};
use std::collections::BTreeMap;

use crate::{CompositionProfile, ConfigError, Result, canonical_profile_id};

fn into_engine(value: toml::Value) -> ConfigValue {
    match value {
        toml::Value::String(value) => ConfigValue::String(value),
        toml::Value::Integer(value) => ConfigValue::Integer(value),
        toml::Value::Float(value) => ConfigValue::Float(value),
        toml::Value::Boolean(value) => ConfigValue::Boolean(value),
        toml::Value::Datetime(value) => ConfigValue::Datetime(value.to_string()),
        toml::Value::Array(value) => {
            ConfigValue::Array(value.into_iter().map(into_engine).collect())
        }
        toml::Value::Table(value) => ConfigValue::Table(
            value
                .into_iter()
                .map(|(key, value)| (key, into_engine(value)))
                .collect(),
        ),
    }
}

fn from_engine(value: ConfigValue) -> toml::Value {
    match value {
        ConfigValue::String(value) => toml::Value::String(value),
        ConfigValue::Integer(value) => toml::Value::Integer(value),
        ConfigValue::Float(value) => toml::Value::Float(value),
        ConfigValue::Boolean(value) => toml::Value::Boolean(value),
        // These strings were produced only by into_engine from valid datetimes.
        ConfigValue::Datetime(value) => {
            toml::Value::Datetime(value.parse().expect("TOML datetime round trip is valid"))
        }
        ConfigValue::Array(value) => {
            toml::Value::Array(value.into_iter().map(from_engine).collect())
        }
        ConfigValue::Table(value) => toml::Value::Table(
            value
                .into_iter()
                .map(|(key, value)| (key, from_engine(value)))
                .collect(),
        ),
    }
}

pub fn merge(base: &mut toml::Value, overlay: toml::Value) {
    let mut merged = into_engine(base.clone());
    bmux_config_composition::merge_recursive_replace(&mut merged, into_engine(overlay));
    *base = from_engine(merged);
}

pub fn capture(path: &std::path::Path) -> Result<Option<toml::Value>> {
    let bytes = match bmux_config_composition::capture_file(path, usize::MAX) {
        Ok(bytes) => bytes,
        Err(error) if error.kind == ErrorKind::Missing => return Ok(None),
        Err(error) => {
            return Err(ConfigError::ReadError {
                error: format!("{error} ({})", path.display()),
            });
        }
    };
    let invalid = || ConfigError::ParseError {
        error: format!(
            "invalid TOML in {} (source values redacted)",
            path.display()
        ),
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| ConfigError::ReadError {
        error: format!(
            "invalid UTF-8 in {} (source values redacted)",
            path.display()
        ),
    })?;
    TomlCodec::parse(text)
        .map(from_engine)
        .map(Some)
        .map_err(|_| invalid())
}

pub fn profile(
    requested: &str,
    profiles: &BTreeMap<String, CompositionProfile>,
) -> Result<toml::Table> {
    let profiles = profiles
        .iter()
        .map(|(id, profile)| {
            (
                id.clone(),
                Profile {
                    parents: profile
                        .extends
                        .iter()
                        .map(|id| canonical_profile_id(id))
                        .collect(),
                    value: into_engine(toml::Value::Table(profile.patch.clone())),
                },
            )
        })
        .collect();
    // The callback is pure recursive replacement. Cache complete parent patches
    // as the legacy resolver did, while still overlaying every ordered edge.
    let resolved = bmux_config_composition::resolve_profile_cached(
        &canonical_profile_id(requested),
        &profiles,
        // Compatibility loading historically imposed no graph limits.
        ProfileLimits {
            max_depth: usize::MAX,
            max_expansions: usize::MAX,
        },
        |base, overlay| {
            bmux_config_composition::merge_recursive_replace(base, overlay);
            Ok(())
        },
    )
    .map_err(|error| ConfigError::InvalidValue {
        field: if error.kind == ErrorKind::ProfileCycle {
            "composition.profiles"
        } else {
            "composition.active_profile"
        }
        .to_owned(),
        value: if error.kind == ErrorKind::ProfileCycle {
            "profile inheritance cycle detected".to_owned()
        } else {
            error.to_string()
        },
    })?;
    let toml::Value::Table(table) = from_engine(resolved) else {
        unreachable!("profile patches are tables and recursive merges preserve tables")
    };
    Ok(table)
}
