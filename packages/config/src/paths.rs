//! Configuration paths for bmux
//!
//! This module handles cross-platform configuration directory resolution.

use std::path::PathBuf;

/// Generated docs metadata for path/environment overrides.
pub struct EnvOverrideDoc {
    /// Environment variable name.
    pub variable: &'static str,
    /// Config or runtime area affected by the override.
    pub scope: &'static str,
    /// Human-readable behavior summary.
    pub description: &'static str,
}

/// Path and runtime environment overrides recognized by bmux.
pub static ENV_OVERRIDE_DOCS: &[EnvOverrideDoc] = &[
    EnvOverrideDoc {
        variable: "BMUX_CONFIG_DIR",
        scope: "config",
        description: "Overrides the config directory root and disables fallback candidate chaining.",
    },
    EnvOverrideDoc {
        variable: "BMUX_RUNTIME_DIR",
        scope: "runtime",
        description: "Overrides the runtime root used for sockets and temp runtime artifacts.",
    },
    EnvOverrideDoc {
        variable: "BMUX_RUNTIME_NAME",
        scope: "runtime",
        description: "Selects the runtime instance name; non-default values use runtime subdirectories.",
    },
    EnvOverrideDoc {
        variable: "BMUX_DATA_DIR",
        scope: "data",
        description: "Overrides the persistent data directory (plugins and runtime identity files).",
    },
    EnvOverrideDoc {
        variable: "BMUX_STATE_DIR",
        scope: "state",
        description: "Overrides the persistent state directory (recordings, traces, and runtime layout state).",
    },
    EnvOverrideDoc {
        variable: "BMUX_LOG_DIR",
        scope: "logs",
        description: "Overrides the log directory used by bmux file logging.",
    },
    EnvOverrideDoc {
        variable: crate::RECORDINGS_DIR_OVERRIDE_ENV,
        scope: "recordings",
        description: "Overrides recording storage root for recording CLI/runtime resolution.",
    },
];

/// Configuration paths for bmux
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// Primary (canonical) configuration directory.
    ///
    /// On macOS this is `~/Library/Application Support/bmux`, on Linux
    /// `~/.config/bmux`, etc.  Used by [`ensure_dirs`] to create the
    /// directory structure and passed to plugins as the authoritative
    /// config root.
    pub config_dir: PathBuf,
    /// Ordered list of candidate config directories (primary first).
    ///
    /// File lookups probe each candidate in order and return the first
    /// hit, giving users a fallback chain (e.g. `~/.config/bmux` on
    /// macOS when the native location has no matching file).
    config_dir_candidates: Vec<PathBuf>,
    /// Runtime directory for sockets and temporary files
    pub runtime_dir: PathBuf,
    /// Data directory for persistent data
    pub data_dir: PathBuf,
    /// State directory for persisted local state (recordings, layout, traces)
    pub state_dir: PathBuf,
}

impl ConfigPaths {
    /// Create a new `ConfigPaths` with explicit directories.
    ///
    /// The candidate chain defaults to `[config_dir]` (no fallback).
    /// Use [`Default::default`] to get the platform-aware fallback chain.
    #[must_use]
    pub fn new(
        config_dir: PathBuf,
        runtime_dir: PathBuf,
        data_dir: PathBuf,
        state_dir: PathBuf,
    ) -> Self {
        let config_dir_candidates = vec![config_dir.clone()];
        Self {
            config_dir,
            config_dir_candidates,
            runtime_dir,
            data_dir,
            state_dir,
        }
    }

    /// Resolve a relative path against the config directory candidate chain.
    ///
    /// Returns the first candidate where `candidate/relative` exists on disk,
    /// or `primary/relative` if none match (so callers always get a usable
    /// path for creation).
    #[must_use]
    pub fn resolve(&self, relative: impl AsRef<std::path::Path>) -> PathBuf {
        let relative = relative.as_ref();
        self.config_dir_candidates
            .iter()
            .map(|dir| dir.join(relative))
            .find(|p| p.exists())
            .unwrap_or_else(|| self.config_dir.join(relative))
    }

    /// Get the config file path, resolved through the candidate chain.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.resolve("bmux.toml")
    }

    /// Get the themes directory path (primary, for directory creation).
    #[must_use]
    pub fn themes_dir(&self) -> PathBuf {
        self.config_dir.join("themes")
    }

    /// Resolve a theme file by name through the candidate chain.
    ///
    /// Checks `themes/{name}.toml` in each candidate directory and returns
    /// the first that exists, or the primary path if none match.
    #[must_use]
    pub fn resolve_theme_file(&self, name: &str) -> PathBuf {
        self.resolve(format!("themes/{name}.toml"))
    }

    /// Get the plugins directory path
    #[must_use]
    pub fn plugins_dir(&self) -> PathBuf {
        self.data_dir.join("plugins")
    }

    /// Get the sessions directory path
    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.runtime_dir.join("sessions")
    }

    /// Get the logs directory path
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        default_log_dir()
    }

    /// Get persisted state root directory path
    #[must_use]
    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.clone()
    }

    /// Get persisted local runtime state file path
    #[must_use]
    pub fn runtime_layout_state_file(&self) -> PathBuf {
        self.state_dir().join("runtime").join("last-layout.json")
    }

    /// Get persisted protocol trace file path
    #[must_use]
    pub fn protocol_trace_file(&self) -> PathBuf {
        self.state_dir().join("runtime").join("protocol-trace.json")
    }

    /// Get recordings root directory path
    #[must_use]
    pub fn recordings_dir(&self) -> PathBuf {
        self.state_dir().join("runtime").join("recordings")
    }

    /// Get persisted terminfo prompt state file path
    #[must_use]
    pub fn terminfo_prompt_state_file(&self) -> PathBuf {
        self.state_dir()
            .join("runtime")
            .join("terminfo-prompt-state.json")
    }

    /// Get persisted profile principal id file path.
    #[must_use]
    pub fn principal_id_file(&self) -> PathBuf {
        self.data_dir.join("runtime").join("principal-id")
    }

    /// Get the server socket path
    #[must_use]
    pub fn server_socket(&self) -> PathBuf {
        self.runtime_dir.join("server.sock")
    }

    /// Get the server pid file path.
    #[must_use]
    pub fn server_pid_file(&self) -> PathBuf {
        self.runtime_dir.join("server.pid")
    }

    /// Get the server named pipe path for Windows transports.
    ///
    /// The value is deterministic and user-scoped so multiple users on the same
    /// machine do not collide.
    #[must_use]
    pub fn server_named_pipe(&self) -> String {
        let user_raw = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());
        let user = sanitize_endpoint_component(&user_raw);
        let runtime_hash = stable_fnv1a64(self.runtime_dir.to_string_lossy().as_bytes());
        format!(r"\\.\pipe\bmux-{user}-{runtime_hash:016x}")
    }

    /// Return the candidate config directories in priority order.
    ///
    /// On macOS the candidates are the OS-native `~/Library/Application Support/bmux`
    /// followed by the XDG-style `~/.config/bmux`. On other platforms (where the
    /// native path *is* the XDG path) only one candidate is returned.
    /// When `BMUX_CONFIG_DIR` is set, returns only that single entry.
    #[must_use]
    pub fn config_dir_candidates(&self) -> &[PathBuf] {
        &self.config_dir_candidates
    }

    /// Ensure all necessary directories exist
    ///
    /// # Errors
    ///
    /// Returns an error if any directory cannot be created.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.runtime_dir)?;
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.state_dir())?;
        std::fs::create_dir_all(self.recordings_dir())?;
        std::fs::create_dir_all(self.plugins_dir())?;
        std::fs::create_dir_all(self.sessions_dir())?;
        std::fs::create_dir_all(self.logs_dir())?;
        std::fs::create_dir_all(self.config_dir.join("themes"))?;
        Ok(())
    }
}

fn sanitize_endpoint_component(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            output.push(ch);
        } else {
            output.push('-');
        }
    }

    let trimmed = output.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn default_state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("BMUX_STATE_DIR") {
        return PathBuf::from(path);
    }

    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map_or_else(
            || PathBuf::from(".").join("bmux").join("state"),
            |home| {
                home.join("Library")
                    .join("Application Support")
                    .join("bmux")
                    .join("State")
            },
        )
    }

    #[cfg(target_os = "windows")]
    {
        dirs::data_local_dir().map_or_else(
            || PathBuf::from(".").join("bmux").join("state"),
            |base| base.join("bmux").join("State"),
        )
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("XDG_STATE_HOME").map_or_else(
            || {
                dirs::home_dir().map_or_else(
                    || PathBuf::from(".").join("bmux").join("state"),
                    |home| home.join(".local").join("state").join("bmux"),
                )
            },
            |base| PathBuf::from(base).join("bmux"),
        )
    }
}

fn default_log_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("BMUX_LOG_DIR") {
        return PathBuf::from(path);
    }

    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map_or_else(
            || PathBuf::from(".").join("bmux").join("logs"),
            |home| home.join("Library").join("Logs").join("bmux"),
        )
    }

    #[cfg(target_os = "windows")]
    {
        dirs::data_local_dir().map_or_else(
            || PathBuf::from(".").join("bmux").join("logs"),
            |base| base.join("bmux").join("Logs"),
        )
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        default_state_dir().join("logs")
    }
}

fn stable_fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Build the ordered list of candidate config directories.
///
/// The primary (OS-native) directory is always first. On macOS, the XDG-style
/// `~/.config/bmux` is appended as a fallback when it differs from the primary.
fn build_config_dir_candidates() -> Vec<PathBuf> {
    let primary = dirs::config_dir().map_or_else(
        || {
            dirs::home_dir().map_or_else(
                || PathBuf::from(".bmux"),
                |d| d.join(".config").join("bmux"),
            )
        },
        |d| d.join("bmux"),
    );

    #[cfg(target_os = "macos")]
    {
        let mut candidates = vec![primary];
        if let Some(home) = dirs::home_dir() {
            let xdg = home.join(".config").join("bmux");
            if candidates[0] != xdg {
                candidates.push(xdg);
            }
        }
        #[allow(clippy::needless_return)]
        return candidates;
    }

    #[cfg(not(target_os = "macos"))]
    vec![primary]
}

/// Resolve a config directory from an ordered list of candidates.
///
/// Returns the first candidate directory that exists on disk, or the first
/// candidate (the canonical/primary path) if none exist.
#[cfg(test)]
fn resolve_first_existing(candidates: &[PathBuf]) -> PathBuf {
    candidates
        .iter()
        .find(|p| p.exists())
        .unwrap_or(&candidates[0])
        .clone()
}

impl ConfigPaths {
    /// Build paths from explicit overrides (no env var reads).
    ///
    /// Each `Option` parameter corresponds to an environment variable override.
    /// `None` means "use the platform default" (same as when the env var is unset).
    #[allow(clippy::too_many_lines)]
    pub fn from_overrides(
        runtime_name: &str,
        runtime_dir_override: Option<&std::ffi::OsStr>,
        config_dir_override: Option<&std::ffi::OsStr>,
        data_dir_override: Option<&std::ffi::OsStr>,
    ) -> Self {
        let config_dir_candidates = config_dir_override
            .map_or_else(build_config_dir_candidates, |p| vec![PathBuf::from(p)]);
        let config_dir = config_dir_candidates[0].clone();

        let data_dir = data_dir_override.map_or_else(
            || {
                dirs::data_dir().map_or_else(
                    || {
                        dirs::home_dir().map_or_else(
                            || PathBuf::from(".bmux"),
                            |d| d.join(".local").join("share").join("bmux"),
                        )
                    },
                    |d| d.join("bmux"),
                )
            },
            PathBuf::from,
        );

        let runtime_root = runtime_dir_override.map_or_else(
            || {
                if cfg!(unix) {
                    std::env::var("XDG_RUNTIME_DIR")
                        .map(PathBuf::from)
                        .map_or_else(|_| std::env::temp_dir().join("bmux"), |d| d.join("bmux"))
                } else {
                    std::env::temp_dir().join("bmux")
                }
            },
            PathBuf::from,
        );
        let runtime_dir = if runtime_name == "default" {
            runtime_root
        } else {
            runtime_root.join("runtimes").join(runtime_name)
        };

        let mut paths = Self::new(config_dir, runtime_dir, data_dir, default_state_dir());
        paths.config_dir_candidates = config_dir_candidates;
        paths
    }
}

impl Default for ConfigPaths {
    fn default() -> Self {
        let runtime_name = std::env::var("BMUX_RUNTIME_NAME")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "default".to_string());

        Self::from_overrides(
            &runtime_name,
            std::env::var_os("BMUX_RUNTIME_DIR").as_deref(),
            std::env::var_os("BMUX_CONFIG_DIR").as_deref(),
            std::env::var_os("BMUX_DATA_DIR").as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigPaths, resolve_first_existing, stable_fnv1a64};
    use std::path::PathBuf;

    /// Build a `ConfigPaths` with a custom candidate chain for testing.
    fn paths_with_candidates(candidates: Vec<PathBuf>) -> ConfigPaths {
        let mut paths = ConfigPaths::new(
            candidates[0].clone(),
            PathBuf::from("/runtime"),
            PathBuf::from("/data"),
            PathBuf::from("/state"),
        );
        paths.config_dir_candidates = candidates;
        paths
    }

    #[test]
    fn server_socket_uses_runtime_dir() {
        let paths = ConfigPaths::new(
            PathBuf::from("/config"),
            PathBuf::from("/runtime"),
            PathBuf::from("/data"),
            PathBuf::from("/state"),
        );
        assert_eq!(paths.server_socket(), PathBuf::from("/runtime/server.sock"));
    }

    #[test]
    fn server_pid_file_uses_runtime_dir() {
        let paths = ConfigPaths::new(
            PathBuf::from("/config"),
            PathBuf::from("/runtime"),
            PathBuf::from("/data"),
            PathBuf::from("/state"),
        );
        assert_eq!(
            paths.server_pid_file(),
            PathBuf::from("/runtime/server.pid")
        );
    }

    #[test]
    fn server_named_pipe_is_stable_for_same_runtime() {
        let paths = ConfigPaths::new(
            PathBuf::from("/config"),
            PathBuf::from("/runtime/path"),
            PathBuf::from("/data"),
            PathBuf::from("/state"),
        );
        assert_eq!(paths.server_named_pipe(), paths.server_named_pipe());
    }

    #[test]
    fn server_named_pipe_changes_with_runtime_dir() {
        let a = ConfigPaths::new(
            PathBuf::from("/config"),
            PathBuf::from("/runtime/a"),
            PathBuf::from("/data"),
            PathBuf::from("/state"),
        );
        let b = ConfigPaths::new(
            PathBuf::from("/config"),
            PathBuf::from("/runtime/b"),
            PathBuf::from("/data"),
            PathBuf::from("/state"),
        );
        assert_ne!(a.server_named_pipe(), b.server_named_pipe());
    }

    #[cfg(windows)]
    #[test]
    fn server_named_pipe_uses_windows_pipe_prefix() {
        let paths = ConfigPaths::new(
            PathBuf::from("C:/config"),
            PathBuf::from("C:/runtime"),
            PathBuf::from("C:/data"),
            PathBuf::from("C:/state"),
        );
        assert!(paths.server_named_pipe().starts_with(r"\\.\pipe\"));
    }

    #[cfg(unix)]
    #[test]
    fn server_socket_file_name_is_server_sock() {
        let paths = ConfigPaths::new(
            PathBuf::from("/config"),
            PathBuf::from("/runtime"),
            PathBuf::from("/data"),
            PathBuf::from("/state"),
        );
        assert!(paths.server_socket().ends_with("server.sock"));
    }

    #[test]
    fn hash_fnv1a_known_vector() {
        assert_eq!(stable_fnv1a64(b"bmux"), 0xbb09_969b_bc2c_17fd);
    }

    // -- resolve_first_existing (the old directory-level helper, kept for unit coverage) --

    #[test]
    fn resolve_first_existing_returns_primary_when_none_exist() {
        let tmp = std::env::temp_dir().join("bmux_test_resolve_first_existing");
        let _ = std::fs::remove_dir_all(&tmp);
        let primary = tmp.join("primary");
        let fallback = tmp.join("fallback");

        let result = resolve_first_existing(&[primary.clone(), fallback]);
        assert_eq!(result, primary);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_first_existing_returns_fallback_when_primary_absent() {
        let tmp = std::env::temp_dir().join("bmux_test_resolve_fallback");
        let _ = std::fs::remove_dir_all(&tmp);
        let primary = tmp.join("primary");
        let fallback = tmp.join("fallback");

        std::fs::create_dir_all(&fallback).unwrap();
        let result = resolve_first_existing(&[primary, fallback.clone()]);
        assert_eq!(result, fallback);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_first_existing_prefers_primary_when_both_exist() {
        let tmp = std::env::temp_dir().join("bmux_test_resolve_both");
        let _ = std::fs::remove_dir_all(&tmp);
        let primary = tmp.join("primary");
        let fallback = tmp.join("fallback");

        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        let result = resolve_first_existing(&[primary.clone(), fallback]);
        assert_eq!(result, primary);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- ConfigPaths::resolve (file-level candidate chain) --

    #[test]
    fn resolve_returns_primary_path_when_no_candidate_has_file() {
        let tmp = std::env::temp_dir().join("bmux_test_resolve_none");
        let _ = std::fs::remove_dir_all(&tmp);
        let primary = tmp.join("primary");
        let fallback = tmp.join("fallback");

        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();

        let paths = paths_with_candidates(vec![primary.clone(), fallback]);
        assert_eq!(paths.resolve("bmux.toml"), primary.join("bmux.toml"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_finds_file_in_fallback_when_primary_lacks_it() {
        let tmp = std::env::temp_dir().join("bmux_test_resolve_fallback_file");
        let _ = std::fs::remove_dir_all(&tmp);
        let primary = tmp.join("primary");
        let fallback = tmp.join("fallback");

        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        std::fs::write(fallback.join("bmux.toml"), "# fallback").unwrap();

        let paths = paths_with_candidates(vec![primary, fallback.clone()]);
        assert_eq!(paths.resolve("bmux.toml"), fallback.join("bmux.toml"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_prefers_primary_when_both_have_file() {
        let tmp = std::env::temp_dir().join("bmux_test_resolve_primary_file");
        let _ = std::fs::remove_dir_all(&tmp);
        let primary = tmp.join("primary");
        let fallback = tmp.join("fallback");

        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        std::fs::write(primary.join("bmux.toml"), "# primary").unwrap();
        std::fs::write(fallback.join("bmux.toml"), "# fallback").unwrap();

        let paths = paths_with_candidates(vec![primary.clone(), fallback]);
        assert_eq!(paths.resolve("bmux.toml"), primary.join("bmux.toml"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_works_for_nested_paths() {
        let tmp = std::env::temp_dir().join("bmux_test_resolve_nested");
        let _ = std::fs::remove_dir_all(&tmp);
        let primary = tmp.join("primary");
        let fallback = tmp.join("fallback");

        std::fs::create_dir_all(primary.join("themes")).unwrap();
        std::fs::create_dir_all(fallback.join("themes")).unwrap();
        // Theme only in fallback
        std::fs::write(fallback.join("themes").join("night.toml"), "name='night'").unwrap();

        let paths = paths_with_candidates(vec![primary.clone(), fallback.clone()]);
        assert_eq!(
            paths.resolve("themes/night.toml"),
            fallback.join("themes").join("night.toml")
        );

        // Now also create in primary — primary should win.
        std::fs::write(primary.join("themes").join("night.toml"), "name='night'").unwrap();
        assert_eq!(
            paths.resolve("themes/night.toml"),
            primary.join("themes").join("night.toml")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn config_file_uses_resolve() {
        let tmp = std::env::temp_dir().join("bmux_test_config_file_resolve");
        let _ = std::fs::remove_dir_all(&tmp);
        let primary = tmp.join("primary");
        let fallback = tmp.join("fallback");

        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        std::fs::write(fallback.join("bmux.toml"), "# config").unwrap();

        let paths = paths_with_candidates(vec![primary, fallback.clone()]);
        assert_eq!(paths.config_file(), fallback.join("bmux.toml"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_theme_file_uses_resolve() {
        let tmp = std::env::temp_dir().join("bmux_test_resolve_theme");
        let _ = std::fs::remove_dir_all(&tmp);
        let primary = tmp.join("primary");
        let fallback = tmp.join("fallback");

        std::fs::create_dir_all(primary.join("themes")).unwrap();
        std::fs::create_dir_all(fallback.join("themes")).unwrap();
        std::fs::write(fallback.join("themes").join("dracula.toml"), "").unwrap();

        let paths = paths_with_candidates(vec![primary, fallback.clone()]);
        assert_eq!(
            paths.resolve_theme_file("dracula"),
            fallback.join("themes").join("dracula.toml")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn config_dir_candidates_returns_at_least_one() {
        let paths = ConfigPaths::default();
        assert!(
            !paths.config_dir_candidates().is_empty(),
            "config_dir_candidates must return at least one path"
        );
    }

    #[test]
    fn default_runtime_name_uses_root_runtime_dir() {
        let paths = ConfigPaths::from_overrides(
            "default",
            Some(std::ffi::OsStr::new("/tmp/bmux-runtime-root")),
            None,
            None,
        );
        assert_eq!(paths.runtime_dir, PathBuf::from("/tmp/bmux-runtime-root"));
    }

    #[test]
    fn named_runtime_name_uses_namespaced_runtime_dir() {
        let paths = ConfigPaths::from_overrides(
            "dev",
            Some(std::ffi::OsStr::new("/tmp/bmux-runtime-root")),
            None,
            None,
        );
        assert_eq!(
            paths.runtime_dir,
            PathBuf::from("/tmp/bmux-runtime-root")
                .join("runtimes")
                .join("dev")
        );
    }

    #[test]
    fn missing_runtime_name_defaults_to_default_runtime() {
        let paths = ConfigPaths::from_overrides(
            "default",
            Some(std::ffi::OsStr::new("/tmp/bmux-runtime-root")),
            None,
            None,
        );
        assert_eq!(paths.runtime_dir, PathBuf::from("/tmp/bmux-runtime-root"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn config_dir_candidates_includes_xdg_fallback_on_macos() {
        let paths = ConfigPaths::default();
        let candidates = paths.config_dir_candidates();
        assert!(
            candidates.len() >= 2,
            "macOS should have at least 2 candidates (native + XDG), got {candidates:?}"
        );
        let xdg = dirs::home_dir().unwrap().join(".config").join("bmux");
        assert!(
            candidates.contains(&xdg),
            "candidates should include XDG path {xdg:?}, got {candidates:?}"
        );
    }
}
