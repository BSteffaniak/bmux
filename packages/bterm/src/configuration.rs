//! Captured Bterm configuration policy. No companion or directory creation is needed.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use bmux_config::{BmuxConfig, ConfigFamily, ConfigScopeTarget, ConfigSource};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::runtime_paths::BtermPaths;

/// Which ambient source families are admitted during capture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourcePolicy {
    /// Normal Bterm sources, followed by the host BMUX family.
    #[default]
    Inherit,
    /// Normal Bterm sources, with no host BMUX family.
    BtermOnly,
    /// Only caller-selected source paths; environment never adds sources.
    Explicit,
}

/// An ordered source family: base < primary < mutable state < environment < CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourcePaths {
    pub base: Option<PathBuf>,
    pub primary: Option<PathBuf>,
    pub state: Option<PathBuf>,
    pub environment: Option<PathBuf>,
    pub cli: Option<PathBuf>,
    /// Root-to-leaf scoped local files, following the ordinary family sources.
    pub local: Vec<PathBuf>,
}

impl SourcePaths {
    fn sources(&self, cwd: &Path) -> Vec<ConfigSource> {
        [
            (&self.base, false),
            (&self.primary, false),
            (&self.state, false),
            (&self.environment, true),
            (&self.cli, true),
        ]
        .into_iter()
        .filter_map(|(path, required)| {
            path.as_ref().map(|path| ConfigSource {
                path: absolute(cwd, path),
                required,
            })
        })
        .chain(self.local.iter().map(|path| ConfigSource {
            path: absolute(cwd, path),
            required: false,
        }))
        .collect()
    }

    fn capture(&self, cwd: &Path) -> Result<ConfigFamily> {
        let mut family = ConfigFamily::capture(&self.sources(cwd))?;
        let ordinary = family.documents.len() - self.local.len();
        for document in &mut family.documents[ordinary..] {
            document.local = true;
        }
        Ok(family)
    }
}

/// Inputs to discovery, distinct from the captured files used for resolution.
#[derive(Debug, Clone, Default)]
pub struct ConfigOptions {
    pub policy: SourcePolicy,
    pub bterm: SourcePaths,
    pub bmux: SourcePaths,
    pub forced_profile: Option<String>,
    pub scope: Option<ConfigScopeTarget>,
}

/// Native settings are additive to the complete BMUX root schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct NativeConfiguration {
    pub window: WindowConfiguration,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfiguration {
    pub decorations: bool,
}

impl Default for WindowConfiguration {
    fn default() -> Self {
        Self { decorations: true }
    }
}

/// Captured files and their exact selected paths. Retain this value for reload.
#[derive(Clone)]
pub struct ConfigInputs {
    pub paths: BtermPaths,
    pub options: ConfigOptions,
    pub cwd: PathBuf,
    pub bterm: ConfigFamily,
    pub bmux: ConfigFamily,
    // Captured once, never serialized into diagnostics.
    environment: BTreeMap<String, OsString>,
}

/// Safe source metadata: no arbitrary TOML values or environment values.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SourceProvenance {
    pub family: String,
    pub path: PathBuf,
    pub required: bool,
    pub present: bool,
    /// SHA-256 of the parsed TOML representation, not an unstable file reread.
    pub sha256: Option<String>,
}

/// Full runtime result. The snapshot is private launch data, NOT inspect output.
#[derive(Clone)]
pub struct ResolvedConfiguration {
    pub bmux: BmuxConfig,
    pub native: NativeConfiguration,
    pub bmux_toml: String,
    pub generation: String,
    pub sources: Vec<SourceProvenance>,
    pub resolution: serde_json::Value,
    pub inputs: ConfigInputs,
}

impl ConfigOptions {
    /// Capture the environment and working directory once, without creating paths.
    pub fn capture(self, managed_root: Option<&Path>) -> Result<ConfigInputs> {
        let cwd = std::env::current_dir().context("capture Bterm working directory")?;
        let environment = std::env::vars_os()
            .filter_map(|(key, value)| key.into_string().ok().map(|key| (key, value)))
            .collect();
        self.capture_with_environment(cwd, environment, managed_root)
    }

    /// Dependency-injected discovery; does not read the process environment.
    /// An explicit managed root is self-contained by default. Use selected paths
    /// through `capture_selected` to deliberately import anything outside it.
    pub fn capture_with_environment(
        mut self,
        cwd: PathBuf,
        environment: BTreeMap<String, OsString>,
        managed_root: Option<&Path>,
    ) -> Result<ConfigInputs> {
        ensure!(cwd.is_absolute(), "captured cwd must be absolute");
        let paths = if let Some(root) = managed_root {
            self.policy = SourcePolicy::Explicit;
            self.bmux
                .primary
                .get_or_insert_with(|| root.join("config/bmux.toml"));
            self.bmux
                .state
                .get_or_insert_with(|| root.join("state/config-overlay.toml"));
            BtermPaths::from_root(root.to_path_buf())?
        } else {
            BtermPaths::from_environment(&cwd, &environment)?
        };
        if self.policy != SourcePolicy::Explicit || managed_root.is_some() {
            self.bterm
                .primary
                .get_or_insert_with(|| paths.config_file());
            self.bterm
                .state
                .get_or_insert_with(|| paths.state_config_file());
        }
        if self.policy != SourcePolicy::Explicit {
            fill_environment_sources(&mut self.bterm, "BTERM", &environment);
            if self.policy == SourcePolicy::Inherit {
                // Host selectors affect config inputs ONLY, never managed writable roots.
                let host = BtermPaths::for_application("bmux", "BMUX", &cwd, &environment)?;
                let mut primary = host.config_dir.join("bmux.toml");
                // Preserve BMUX's macOS fallback without consulting ambient state.
                if cfg!(target_os = "macos")
                    && !environment.contains_key("BMUX_CONFIG_DIR")
                    && !primary.exists()
                    && let Some(home) = environment.get("HOME")
                {
                    let fallback = PathBuf::from(home).join(".config/bmux/bmux.toml");
                    if fallback.exists() {
                        primary = fallback;
                    }
                }
                self.bmux.primary.get_or_insert(primary);
                self.bmux
                    .state
                    .get_or_insert_with(|| host.state_dir.join("config-overlay.toml"));
                fill_environment_sources(&mut self.bmux, "BMUX", &environment);
            }
        }
        if self.policy != SourcePolicy::Explicit && self.scope.is_some() {
            discover_local_sources(&mut self.bterm, &cwd, "bterm.toml");
            if self.policy == SourcePolicy::Inherit {
                discover_local_sources(&mut self.bmux, &cwd, "bmux.toml");
            }
        }
        self.capture_selected_with_environment(paths, cwd, environment)
    }

    /// Capture already-selected sandbox paths. No ambient source discovery or
    /// process environment reads occur, regardless of the policy label.
    pub fn capture_selected(self, paths: BtermPaths, cwd: PathBuf) -> Result<ConfigInputs> {
        self.capture_selected_with_environment(paths, cwd, BTreeMap::new())
    }

    pub fn capture_selected_with_environment(
        self,
        paths: BtermPaths,
        cwd: PathBuf,
        environment: BTreeMap<String, OsString>,
    ) -> Result<ConfigInputs> {
        ensure!(cwd.is_absolute(), "captured cwd must be absolute");
        for path in [
            &paths.config_dir,
            &paths.data_dir,
            &paths.state_dir,
            &paths.log_dir,
            &paths.cache_dir,
        ] {
            crate::runtime_paths::validate_absolute(path)?;
        }
        let bterm = self.bterm.capture(&cwd)?;
        let bmux = self.bmux.capture(&cwd)?;
        Ok(ConfigInputs {
            paths,
            options: self,
            cwd,
            bterm,
            bmux,
            environment,
        })
    }
}

impl ConfigInputs {
    /// Re-read the same selected paths, without rediscovering host configuration.
    pub fn reload(&self) -> Result<Self> {
        self.options.clone().capture_selected_with_environment(
            self.paths.clone(),
            self.cwd.clone(),
            self.environment.clone(),
        )
    }

    /// Resolve captured documents with the shared BMUX composition/merge engine.
    ///
    /// The current shared resolver retains ambient auto-profile/path-token
    /// evaluation. File discovery is captured; deterministic evaluation awaits
    /// its explicit evaluation-context entry point. Do not claim this result is
    /// environment-isolated until that shared API is wired here.
    pub fn resolve(&self) -> Result<ResolvedConfiguration> {
        let context = bmux_config::ConfigEvaluationContext {
            cwd: Some(self.cwd.clone()),
            environment: self.environment.clone(),
        };
        let resolved = bmux_config::resolve_application_configuration_with_context(
            &self.bterm,
            &self.bmux,
            self.options.forced_profile.as_deref(),
            self.options.scope.as_ref(),
            &context,
        )?;
        let native: NativeConfiguration =
            resolved.application.clone().try_into().map_err(|_| {
                anyhow::anyhow!("invalid [bterm] configuration (source values redacted)")
            })?;
        // Pin the complete effective BMUX configuration so companion-side legacy
        // primary/state layers cannot reintroduce values omitted from a sparse
        // source document. Composition has already resolved exactly once.
        let bmux_toml = toml::to_string(&resolved.startup_snapshot()?)
            .context("serialize captured BMUX configuration")?;
        let mut sources = Vec::new();
        for (name, family) in [("bterm", &self.bterm), ("bmux", &self.bmux)] {
            for document in &family.documents {
                sources.push(SourceProvenance {
                    family: name.to_owned(),
                    path: document.source.path.clone(),
                    required: document.source.required,
                    present: document.value.is_some(),
                    sha256: document
                        .value
                        .as_ref()
                        .map(|value| hash(value.to_string().as_bytes())),
                });
            }
        }
        let mut fingerprint = Sha256::new();
        fingerprint.update(b"bterm-configuration-v1\0");
        fingerprint.update(bmux_toml.as_bytes());
        fingerprint.update(serde_json::to_vec(&native)?);
        fingerprint.update(serde_json::to_vec(&sources)?);
        let resolution = serde_json::json!({
            "bterm": resolved.application_family.resolution,
            "bmux": resolved.bmux_family.resolution,
            "origins": resolved.application_family.origins.iter().filter(|(key, _)| key.first().is_some_and(|part| part == "bterm")).map(|(key, path)| {
                serde_json::json!({"family": "bterm", "key": key, "source": path})
            }).collect::<Vec<_>>(),
        });
        Ok(ResolvedConfiguration {
            bmux: resolved.bmux,
            resolution,
            native,
            bmux_toml,
            generation: hex(&fingerprint.finalize()),
            sources,
            inputs: self.clone(),
        })
    }
}

impl ResolvedConfiguration {
    /// Allowlisted effective output. Never serialize BMUX config or raw TOML:
    /// plugin options, commands, arbitrary extensions and tokens may hold secrets.
    pub fn effective_json(&self) -> serde_json::Value {
        serde_json::json!({
            "generation": self.generation,
            "bterm": self.native,
            "bmux": "[redacted: private configuration]",
            "sources": self.sources,
            "composition": self.resolution,
        })
    }

    pub fn reload(&self) -> Result<Self> {
        self.inputs.reload()?.resolve()
    }
}

fn fill_environment_sources(
    paths: &mut SourcePaths,
    prefix: &str,
    env: &BTreeMap<String, OsString>,
) {
    if paths.environment.is_none() {
        paths.environment = env.get(&format!("{prefix}_CONFIG")).map(PathBuf::from);
    }
    if env
        .get(&format!("{prefix}_NO_BASE_CONFIG"))
        .is_some_and(|value| {
            matches!(
                value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    {
        paths.base = None;
    } else if paths.base.is_none() {
        paths.base = env.get(&format!("{prefix}_BASE_CONFIG")).map(PathBuf::from);
    }
}

fn discover_local_sources(paths: &mut SourcePaths, cwd: &Path, filename: &str) {
    if !paths.local.is_empty() {
        return;
    }
    let mut ancestors = cwd.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    paths.local = ancestors
        .into_iter()
        .map(|path| path.join(filename))
        .filter(|path| Some(path) != paths.primary.as_ref() && path.exists())
        .collect();
}

fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn hash(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(result, "{byte:02x}").expect("writing into a String cannot fail");
    }
    result
}
