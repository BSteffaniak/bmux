//! `[plugins.settings."bmux.feedback"]`.

use std::path::PathBuf;

use bcode_session_bridge_runtime::{AutoPreference, NamedExecutable, RuntimeMode, RuntimeOptions};
use serde::Deserialize;

/// How users name the Bcode executable, for messages.
const NAMING_HINT: &str = "BCODE_BIN or `bcode` in [plugins.settings.\"bmux.feedback\"]";

/// Longest recording span the agent may attach by default, in seconds.
const DEFAULT_MAX_CAPTURE_SECONDS: u64 = 120;

/// The plugin's settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Which Bcode receives feedback: `auto`, `installed`, or `embedded`.
    pub runtime: RuntimeMode,
    /// What `auto` uses when no Bcode executable is named: the Bcode embedded in bmux when
    /// this build has one, else an installed Bcode.
    pub auto_preference: AutoPreference,
    /// The Bcode executable; `BCODE_BIN` outranks it.
    pub bcode: Option<PathBuf>,
    /// Longest recording span the agent may attach, in seconds.
    pub max_capture_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            runtime: RuntimeMode::Auto,
            auto_preference: if cfg!(feature = "embedded-bcode") {
                AutoPreference::Embedded
            } else {
                AutoPreference::Installed
            },
            bcode: None,
            max_capture_seconds: DEFAULT_MAX_CAPTURE_SECONDS,
        }
    }
}

impl Settings {
    /// Settings from the plugin's configuration table; defaults when absent.
    ///
    /// # Errors
    /// Describes an unknown key or a value of the wrong type.
    pub fn parse(value: Option<&toml::Value>) -> Result<Self, String> {
        value.map_or_else(
            || Ok(Self::default()),
            |value| {
                value
                    .clone()
                    .try_into()
                    .map_err(|error| format!("invalid bmux.feedback settings: {error}"))
            },
        )
    }

    /// How to start the bridge to the Bcode that receives feedback.
    #[must_use]
    pub fn runtime_options(&self) -> RuntimeOptions {
        RuntimeOptions {
            mode: self.runtime,
            auto_preference: self.auto_preference,
            named: NamedExecutable {
                flag: None,
                configured: self.bcode.clone(),
            },
            naming_hint: NAMING_HINT.to_owned(),
            #[cfg(feature = "embedded-bcode")]
            embedded: Some(bcode_session_bridge_embedded::embedded()),
            ..RuntimeOptions::new(crate::APP_ID)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_settings_prefer_the_embedded_bcode_when_built_in() {
        let settings = Settings::parse(None).unwrap();
        assert_eq!(settings, Settings::default());
        let options = settings.runtime_options();
        assert_eq!(options.app, "bmux");
        let embedded = cfg!(feature = "embedded-bcode");
        assert_eq!(options.embedded.is_some(), embedded);
        assert_eq!(
            options.auto_preference,
            if embedded {
                AutoPreference::Embedded
            } else {
                AutoPreference::Installed
            }
        );
    }

    #[test]
    fn settings_name_the_runtime_and_executable() {
        let value: toml::Value = toml::from_str(
            "runtime = \"installed\"\nbcode = \"/opt/bcode\"\nmax_capture_seconds = 30\n",
        )
        .unwrap();
        let settings = Settings::parse(Some(&value)).unwrap();
        assert_eq!(settings.runtime, RuntimeMode::Installed);
        assert_eq!(settings.max_capture_seconds, 30);
        assert_eq!(
            settings.runtime_options().named.configured,
            Some(PathBuf::from("/opt/bcode"))
        );
    }

    #[test]
    fn unknown_settings_are_refused() {
        let value: toml::Value = toml::from_str("runtme = \"auto\"\n").unwrap();
        assert!(Settings::parse(Some(&value)).is_err());
    }
}
