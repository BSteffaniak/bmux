//! Tabs-owned configuration for fresh-shell directory selection.
//!
//! Parsing does not touch the filesystem: paths belong to the execution host.
//! Runtime resolution and explicit command overrides are separate operations.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CwdSettings {
    #[serde(default)]
    pub new_pane: DirectoryPolicy,
    #[serde(default)]
    pub new_tab: DirectoryPolicy,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DirectoryPolicy {
    #[default]
    RuntimeDefault,
    Home,
    Path {
        path: String,
    },
    SourcePane {
        fallback: DirectoryFallback,
    },
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DirectoryFallback {
    Home,
    RuntimeDefault,
}

impl CwdSettings {
    pub fn parse(settings: Option<&toml::Value>) -> Result<Self, String> {
        let Some(value) = settings.and_then(|settings| settings.get("cwd")) else {
            return Ok(Self::default());
        };
        for operation in ["new_pane", "new_tab"] {
            if let Some(policy) = value.get(operation).and_then(toml::Value::as_table) {
                let source = policy.get("source").and_then(toml::Value::as_str);
                for key in policy.keys() {
                    let allowed = key == "source"
                        || (key == "path" && source == Some("path"))
                        || (key == "fallback" && source == Some("source-pane"));
                    if !allowed {
                        return Err(format!("invalid tabs cwd.{operation} field: {key}"));
                    }
                }
            }
        }
        let settings: Self = value
            .clone()
            .try_into()
            .map_err(|error| format!("invalid tabs cwd settings: {error}"))?;
        settings.new_pane.validate("new_pane")?;
        settings.new_tab.validate("new_tab")?;
        Ok(settings)
    }
}

impl DirectoryPolicy {
    fn validate(&self, operation: &str) -> Result<(), String> {
        if let Self::Path { path } = self
            && (!Path::new(path).is_absolute() || path.contains('\0'))
        {
            return Err(format!(
                "tabs cwd.{operation}.path must be an absolute path without NUL bytes"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: &str) -> Result<CwdSettings, String> {
        CwdSettings::parse(Some(&toml::from_str::<toml::Value>(value).unwrap()))
    }

    #[test]
    fn absent_settings_preserve_runtime_default_independently() {
        assert_eq!(CwdSettings::parse(None).unwrap(), CwdSettings::default());
        let settings = parse("[cwd.new_pane]\nsource = 'home'").unwrap();
        assert_eq!(settings.new_pane, DirectoryPolicy::Home);
        assert_eq!(settings.new_tab, DirectoryPolicy::RuntimeDefault);
    }

    #[test]
    fn pane_and_tab_policies_are_independent() {
        let settings = parse(
            "[cwd.new_pane]\nsource = 'source-pane'\nfallback = 'home'\n\
             [cwd.new_tab]\nsource = 'path'\npath = '/work/project'",
        )
        .unwrap();
        assert_eq!(
            settings.new_pane,
            DirectoryPolicy::SourcePane {
                fallback: DirectoryFallback::Home
            }
        );
        assert_eq!(
            settings.new_tab,
            DirectoryPolicy::Path {
                path: "/work/project".into()
            }
        );
    }

    #[test]
    fn inherited_directory_requires_an_explicit_nonrecursive_fallback() {
        for value in [
            "source = 'source-pane'",
            "source = 'source-pane'\nfallback = 'source-pane'",
            "source = 'source-pane'\nfallback = 'path'",
        ] {
            assert!(parse(&format!("[cwd.new_pane]\n{value}")).is_err());
        }
        assert!(
            parse("[cwd.new_pane]\nsource = 'source-pane'\nfallback = 'runtime-default'").is_ok()
        );
    }

    #[test]
    fn invalid_or_ambiguous_settings_are_not_silently_ignored() {
        for value in [
            "source = 'unknown'",
            "source = 'path'",
            "source = 'path'\npath = 'relative'",
            "source = 'path'\npath = '~/project'",
            "source = 'path'\npath = '$HOME/project'",
            "source = 'home'\npath = '/ignored'",
            "source = 'home'\nfallback = 'runtime-default'",
            "source = 'runtime-default'\nextra = true",
        ] {
            assert!(
                parse(&format!("[cwd.new_tab]\n{value}")).is_err(),
                "{value}"
            );
        }
        assert!(parse("[cwd.new_workspace]\nsource = 'home'").is_err());
    }

    #[test]
    fn path_existence_is_not_checked_on_the_config_reading_host() {
        assert!(parse("[cwd.new_tab]\nsource = 'path'\npath = '/execution-host-only'").is_ok());
    }
}
