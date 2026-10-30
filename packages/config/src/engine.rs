//! TOML compatibility adapters. Selection and scope policy remain BMUX-owned;
//! recursive merge and ordered inheritance are shared engine operations.
use bmux_config_composition::{ConfigValue, TomlCodec};
#[cfg(test)]
use bmux_config_composition::{ErrorKind, Profile, ProfileLimits};
use std::collections::BTreeMap;

#[cfg(test)]
use crate::{CompositionProfile, canonical_profile_id};
use crate::{ConfigError, Result};

fn into_engine(value: toml::Value) -> ConfigValue {
    TomlCodec::from_toml(value).expect("configuration TOML respects codec depth bounds")
}

fn from_engine(value: &ConfigValue) -> toml::Value {
    TomlCodec::to_toml(value).expect("engine values originated from valid TOML")
}

pub fn merge(base: &mut toml::Value, overlay: toml::Value) {
    let mut merged = into_engine(base.clone());
    bmux_config_composition::merge_recursive_replace(&mut merged, into_engine(overlay));
    *base = from_engine(&merged);
}

pub fn capture(path: &std::path::Path) -> Result<Option<toml::Value>> {
    use bmux_config_composition::{Layer, LoadContext, Loader, Requirement};
    let cwd = path.parent().ok_or_else(|| ConfigError::ReadError {
        error: "configuration path has no parent".into(),
    })?;
    let captured = Loader::builder()
        .layer(
            "file",
            Layer::File {
                path: path.to_owned(),
                requirement: Requirement::Optional,
            },
        )
        .build()
        .and_then(|loader| loader.capture(&LoadContext::new(cwd.to_owned(), BTreeMap::new())?))
        .map_err(|error| ConfigError::ReadError {
            error: format!("{error} ({})", path.display()),
        })?;
    if captured.sources().len() == 1 {
        return Ok(None);
    }
    captured
        .resolve(&TomlCodec)
        .and_then(|resolved| TomlCodec::to_toml(&resolved.document.value))
        .map(Some)
        .map_err(|_| ConfigError::ParseError {
            error: format!(
                "invalid TOML in {} (source values redacted)",
                path.display()
            ),
        })
}

/// Compose already selected documents through the canonical ordered layer engine.
pub fn compose(
    layers: Vec<(String, bmux_config_composition::Layer)>,
) -> Result<Option<toml::Value>> {
    use bmux_config_composition::{LoadContext, Loader};
    let cwd = std::env::current_dir()
        .or_else(|error| {
            layers
                .iter()
                .find_map(|(_, layer)| match layer {
                    bmux_config_composition::Layer::File { path, .. } if path.is_absolute() => {
                        path.parent().map(std::path::Path::to_owned)
                    }
                    _ => None,
                })
                .ok_or(error)
        })
        .map_err(|error| ConfigError::ReadError {
            error: error.to_string(),
        })?;
    let mut builder = Loader::builder().duplicates(bmux_config_composition::DuplicatePolicy::Allow);
    for (name, layer) in layers {
        builder = builder.layer(name, layer);
    }
    let captured = builder
        .build()
        .and_then(|loader| loader.capture(&LoadContext::new(cwd, BTreeMap::new())?))
        .map_err(|error| ConfigError::ParseError {
            error: error.to_string(),
        })?;
    if captured.sources().len() == 1 {
        return Ok(None);
    }
    let resolved = captured
        .resolve(&TomlCodec)
        .map_err(|error| ConfigError::ParseError {
            error: error.to_string(),
        })?;
    TomlCodec::to_toml(&resolved.document.value)
        .map(Some)
        .map_err(|error| ConfigError::ParseError {
            error: error.to_string(),
        })
}

#[cfg(test)]
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
    let toml::Value::Table(table) = from_engine(&resolved) else {
        unreachable!("profile patches are tables and recursive merges preserve tables")
    };
    Ok(table)
}
