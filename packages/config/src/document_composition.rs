//! BMUX profile syntax adapted to the shared provenance-preserving graph resolver.
use crate::{
    ConfigError, Result, built_in_composition_profiles, canonical_profile_id,
    parse_composition_config,
};
use bmux_config_composition::{
    ConfigValue, Document, MergePolicy, PathSegment, Profile, ProfileLimits, SourceId, TomlCodec,
};
use std::collections::BTreeMap;

pub fn error(error: bmux_config_composition::Error) -> ConfigError {
    ConfigError::ParseError {
        error: error.to_string(),
    }
}

pub fn profiles(raw: &Document) -> Result<BTreeMap<String, Profile<Document>>> {
    let value = TomlCodec::to_toml(&raw.value).map_err(error)?;
    let composition =
        parse_composition_config(value.as_table().ok_or_else(|| ConfigError::ParseError {
            error: "config root must be a table".into(),
        })?)?;
    let mut profiles: BTreeMap<String, Profile<Document>> = built_in_composition_profiles()
        .into_iter()
        .map(|(name, profile)| {
            let value = TomlCodec::from_toml(toml::Value::Table(profile.patch)).map_err(error)?;
            Ok((
                name,
                Profile {
                    parents: profile.extends,
                    value: Document::new(value, SourceId(0)),
                },
            ))
        })
        .collect::<Result<_>>()?;
    for (name, profile) in composition.profiles {
        let canonical = canonical_profile_id(&name);
        if canonical.is_empty() {
            return Err(ConfigError::InvalidValue {
                field: "composition.profiles".into(),
                value: "profile id must not be empty".into(),
            });
        }
        let selection = ["composition", "profiles", name.as_str(), "patch"]
            .map(|key| PathSegment::Key(key.into()));
        let patch = raw
            .select(&selection)
            .unwrap_or_else(|| Document::new(ConfigValue::Table(BTreeMap::new()), SourceId(0)));
        profiles.insert(
            canonical,
            Profile {
                parents: profile
                    .extends
                    .iter()
                    .map(|name| canonical_profile_id(name))
                    .collect(),
                value: patch,
            },
        );
    }
    Ok(profiles)
}

pub fn profile(name: &str, profiles: &BTreeMap<String, Profile<Document>>) -> Result<Document> {
    bmux_config_composition::resolve_profile_cached(
        &canonical_profile_id(name),
        profiles,
        ProfileLimits {
            max_depth: usize::MAX,
            max_expansions: usize::MAX,
        },
        |base, incoming| base.merge(incoming, &MergePolicy::default()),
    )
    .map_err(|error| ConfigError::InvalidValue {
        field: if error.kind == bmux_config_composition::ErrorKind::ProfileCycle {
            "composition.profiles"
        } else {
            "composition.active_profile"
        }
        .into(),
        value: if error.kind == bmux_config_composition::ErrorKind::ProfileCycle {
            "profile inheritance cycle detected".into()
        } else {
            error.to_string()
        },
    })
}
