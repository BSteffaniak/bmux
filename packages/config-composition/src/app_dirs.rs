//! Per-platform application directories resolved from explicit context.
//!
//! | directory | macOS                                   | Linux and other Unix                          | Windows                       |
//! | --------- | --------------------------------------- | --------------------------------------------- | ----------------------------- |
//! | config    | `$XDG_CONFIG_HOME/<app>` or `~/.config/<app>` | same                                    | `%APPDATA%\<app>`             |
//! | data      | `~/Library/Application Support/<app>`   | `$XDG_DATA_HOME/<app>` or `~/.local/share/<app>` | `%LOCALAPPDATA%\<app>\data` |
//! | cache     | `~/Library/Caches/<app>`                | `$XDG_CACHE_HOME/<app>` or `~/.cache/<app>`   | `%LOCALAPPDATA%\<app>\cache`  |
//! | runtime   | `<data>/run`                            | `$XDG_RUNTIME_DIR/<app>` or `<data>/run`      | `%LOCALAPPDATA%\<app>\run`    |
//!
//! Configuration uses `~/.config` on macOS too, matching [`crate::Layer::UserXdg`]
//! and most command-line tools. As everywhere in this crate the environment is
//! caller-supplied, never sampled from the process. Empty or relative base
//! variables are ignored, as the XDG Base Directory specification requires.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{Error, ErrorKind, Result};

/// Directory conventions to resolve [`AppDirs`] for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    /// macOS.
    MacOs,
    /// Linux and other Unix-like systems (XDG Base Directory conventions).
    Unix,
    /// Windows.
    Windows,
}

impl Platform {
    /// The platform this binary was compiled for.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// Resolved per-application directories. Nothing is created on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppDirs {
    /// User configuration (holds `config.toml`).
    pub config: PathBuf,
    /// Persistent application data.
    pub data: PathBuf,
    /// Disposable caches.
    pub cache: PathBuf,
    /// Runtime files such as sockets and lock files.
    pub runtime: PathBuf,
}

impl AppDirs {
    /// Resolves `application`'s directories for `platform` from `environment`.
    ///
    /// # Errors
    ///
    /// * [`ErrorKind::InvalidPolicy`] if `application` is not a single path component
    /// * [`ErrorKind::InvalidContext`] if a base directory needs the home
    ///   directory and it is unknown (see [`home_dir`])
    pub fn resolve(
        application: &str,
        environment: &BTreeMap<String, String>,
        platform: Platform,
    ) -> Result<Self> {
        crate::capture::single_component(application)?;
        let home = || home_dir(environment, platform).ok_or(Error::new(ErrorKind::InvalidContext));
        let base = |variable: &str, fallback: &[&str]| -> Result<PathBuf> {
            match absolute_var(environment, variable) {
                Some(base) => Ok(base),
                None => Ok(fallback.iter().fold(home()?, |path, part| path.join(part))),
            }
        };
        Ok(match platform {
            Platform::Windows => {
                let local = base("LOCALAPPDATA", &["AppData", "Local"])?.join(application);
                Self {
                    config: base("APPDATA", &["AppData", "Roaming"])?.join(application),
                    data: local.join("data"),
                    cache: local.join("cache"),
                    runtime: local.join("run"),
                }
            }
            Platform::MacOs => {
                let library = home()?.join("Library");
                let data = library.join("Application Support").join(application);
                Self {
                    config: base("XDG_CONFIG_HOME", &[".config"])?.join(application),
                    runtime: data.join("run"),
                    data,
                    cache: library.join("Caches").join(application),
                }
            }
            Platform::Unix => {
                let data = base("XDG_DATA_HOME", &[".local", "share"])?.join(application);
                Self {
                    config: base("XDG_CONFIG_HOME", &[".config"])?.join(application),
                    runtime: absolute_var(environment, "XDG_RUNTIME_DIR")
                        .map_or_else(|| data.join("run"), |dir| dir.join(application)),
                    data,
                    cache: base("XDG_CACHE_HOME", &[".cache"])?.join(application),
                }
            }
        })
    }

    /// `<config>/config.toml`.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }
}

/// The home directory from `environment`: `HOME`, or on Windows
/// `USERPROFILE` before `HOME`. Empty or relative values are ignored.
#[must_use]
pub fn home_dir(environment: &BTreeMap<String, String>, platform: Platform) -> Option<PathBuf> {
    if platform == Platform::Windows {
        absolute_var(environment, "USERPROFILE").or_else(|| absolute_var(environment, "HOME"))
    } else {
        absolute_var(environment, "HOME")
    }
}

/// Expands a leading `~` (alone, or followed by `/`, or `\` on Windows) to
/// [`home_dir`]. `~user` and every other path are returned unchanged.
///
/// # Errors
///
/// * [`ErrorKind::InvalidContext`] if the path needs the home directory and it is unknown
pub fn expand_home(
    path: &str,
    environment: &BTreeMap<String, String>,
    platform: Platform,
) -> Result<PathBuf> {
    let rest = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/").or_else(|| {
            (platform == Platform::Windows)
                .then(|| path.strip_prefix("~\\"))
                .flatten()
        })
    };
    match rest {
        None => Ok(PathBuf::from(path)),
        Some(rest) => {
            let home =
                home_dir(environment, platform).ok_or(Error::new(ErrorKind::InvalidContext))?;
            Ok(if rest.is_empty() {
                home
            } else {
                home.join(rest)
            })
        }
    }
}

/// A non-empty, absolute path from `variable`.
pub(crate) fn absolute_var(
    environment: &BTreeMap<String, String>,
    variable: &str,
) -> Option<PathBuf> {
    environment
        .get(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| Path::is_absolute(path))
}
