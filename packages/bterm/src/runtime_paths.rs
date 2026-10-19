//! Complete, explicit BMUX roots. Resolution never consults environment variables.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, ensure};
use bmux_config::{BmuxConfig, ConfigLoadOverrides, ConfigPaths};

/// A single runtime's complete configuration, endpoint, data and state namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRoots {
    pub config_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl RuntimeRoots {
    /// Resolve a dedicated root using the same layout for managed and external runtimes.
    /// The root must be absolute and must not contain `..`; nothing is created.
    pub fn from_root(root: PathBuf) -> Result<Self> {
        validate_absolute(&root)?;
        Ok(Self {
            config_dir: root.join("config"),
            runtime_dir: root.join("runtime"),
            data_dir: root.join("data"),
            state_dir: root.join("state"),
        })
    }

    /// Build BMUX's existing path model without its ambient candidate fallback chain.
    #[must_use]
    pub fn config_paths(&self) -> ConfigPaths {
        ConfigPaths::new(
            self.config_dir.clone(),
            self.runtime_dir.clone(),
            self.data_dir.clone(),
            self.state_dir.clone(),
        )
    }

    /// Reuse BMUX's configuration schema and precedence with caller-captured overrides.
    /// No process-global configuration override or environment mutation is installed.
    pub fn load_config(&self, overrides: &ConfigLoadOverrides) -> Result<BmuxConfig> {
        for path in [
            &overrides.base_config_path,
            &overrides.env_config_path,
            &overrides.cli_config_path,
        ]
        .into_iter()
        .flatten()
        {
            validate_absolute(path)?;
        }
        Ok(BmuxConfig::load_isolated_with_paths_and_overrides(
            &self.config_paths(),
            overrides,
        )?)
    }
}

pub(crate) fn validate_absolute(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "runtime/config paths must be absolute: {}",
        path.display()
    );
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "runtime/config paths must not contain '..': {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_roots_have_no_config_fallback() {
        let root = std::env::temp_dir().join("bterm-roots-unit");
        let roots = RuntimeRoots::from_root(root.clone()).unwrap();
        let paths = roots.config_paths();
        assert_eq!(
            paths.resolve("missing.toml"),
            root.join("config/missing.toml")
        );
        assert_eq!(paths.runtime_dir, root.join("runtime"));
        assert_eq!(paths.data_dir, root.join("data"));
        assert_eq!(paths.state_dir, root.join("state"));
    }

    #[test]
    fn rejects_relative_and_traversing_roots() {
        assert!(RuntimeRoots::from_root(PathBuf::from("relative")).is_err());
        assert!(RuntimeRoots::from_root(std::env::temp_dir().join("a/../b")).is_err());
    }

    #[test]
    fn isolated_config_preserves_explicit_layer_precedence() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("bterm-config-{}-{unique}", std::process::id()));
        let roots = RuntimeRoots::from_root(root.clone()).unwrap();
        let paths = roots.config_paths();
        std::fs::create_dir_all(&roots.config_dir).unwrap();
        std::fs::create_dir_all(paths.state_config_file().parent().unwrap()).unwrap();
        let base = root.join("base.toml");
        let env = root.join("environment.toml");
        let cli = root.join("cli.toml");
        for (path, limit) in [
            (&base, 111),
            (&paths.config_file(), 222),
            (&paths.state_config_file(), 333),
            (&env, 444),
            (&cli, 555),
        ] {
            std::fs::write(path, format!("[general]\nscrollback_limit = {limit}\n")).unwrap();
        }
        let mut overrides = ConfigLoadOverrides {
            base_config_path: Some(base),
            env_config_path: Some(env),
            cli_config_path: Some(cli),
        };
        assert_eq!(
            roots
                .load_config(&overrides)
                .unwrap()
                .general
                .scrollback_limit,
            555
        );
        overrides.cli_config_path = None;
        assert_eq!(
            roots
                .load_config(&overrides)
                .unwrap()
                .general
                .scrollback_limit,
            444
        );
        overrides.env_config_path = None;
        assert_eq!(
            roots
                .load_config(&overrides)
                .unwrap()
                .general
                .scrollback_limit,
            333
        );
        std::fs::remove_file(paths.state_config_file()).unwrap();
        assert_eq!(
            roots
                .load_config(&overrides)
                .unwrap()
                .general
                .scrollback_limit,
            222
        );
        std::fs::remove_file(paths.config_file()).unwrap();
        assert_eq!(
            roots
                .load_config(&overrides)
                .unwrap()
                .general
                .scrollback_limit,
            111
        );
        overrides.base_config_path = None;
        assert_eq!(
            roots
                .load_config(&overrides)
                .unwrap()
                .general
                .scrollback_limit,
            BmuxConfig::default().general.scrollback_limit
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_relative_config_overrides_before_loading() {
        let roots =
            RuntimeRoots::from_root(std::env::temp_dir().join("bterm-config-unit")).unwrap();
        let overrides = ConfigLoadOverrides {
            cli_config_path: Some(PathBuf::from("relative.toml")),
            ..ConfigLoadOverrides::default()
        };
        assert!(roots.load_config(&overrides).is_err());
    }
}
