//! Configuration paths for bmux
//!
//! This module handles cross-platform configuration directory resolution.

use std::path::PathBuf;

/// Configuration paths for bmux
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// Base configuration directory
    pub config_dir: PathBuf,
    /// Runtime directory for sockets and temporary files
    pub runtime_dir: PathBuf,
    /// Data directory for persistent data
    pub data_dir: PathBuf,
}

impl ConfigPaths {
    /// Create a new `ConfigPaths` with explicit directories
    #[must_use]
    pub const fn new(config_dir: PathBuf, runtime_dir: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            config_dir,
            runtime_dir,
            data_dir,
        }
    }

    /// Get the config file path
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("bmux.toml")
    }

    /// Get the themes directory path
    #[must_use]
    pub fn themes_dir(&self) -> PathBuf {
        self.config_dir.join("themes")
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
        default_state_dir()
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
        return dirs::home_dir().map_or_else(
            || PathBuf::from(".").join("bmux").join("state"),
            |home| {
                home.join("Library")
                    .join("Application Support")
                    .join("bmux")
                    .join("State")
            },
        );
    }

    #[cfg(target_os = "windows")]
    {
        return dirs::data_local_dir().map_or_else(
            || PathBuf::from(".").join("bmux").join("state"),
            |base| base.join("bmux").join("State"),
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        return std::env::var_os("XDG_STATE_HOME").map_or_else(
            || {
                dirs::home_dir().map_or_else(
                    || PathBuf::from(".").join("bmux").join("state"),
                    |home| home.join(".local").join("state").join("bmux"),
                )
            },
            |base| PathBuf::from(base).join("bmux"),
        );
    }
}

fn default_log_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("BMUX_LOG_DIR") {
        return PathBuf::from(path);
    }

    #[cfg(target_os = "macos")]
    {
        return dirs::home_dir().map_or_else(
            || PathBuf::from(".").join("bmux").join("logs"),
            |home| home.join("Library").join("Logs").join("bmux"),
        );
    }

    #[cfg(target_os = "windows")]
    {
        return dirs::data_local_dir().map_or_else(
            || PathBuf::from(".").join("bmux").join("logs"),
            |base| base.join("bmux").join("Logs"),
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        default_state_dir().join("logs")
    }
}

fn stable_fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

impl Default for ConfigPaths {
    fn default() -> Self {
        let config_dir = std::env::var_os("BMUX_CONFIG_DIR").map_or_else(
            || {
                dirs::config_dir().map_or_else(
                    || {
                        dirs::home_dir().map_or_else(
                            || PathBuf::from(".bmux"),
                            |d| d.join(".config").join("bmux"),
                        )
                    },
                    |d| d.join("bmux"),
                )
            },
            PathBuf::from,
        );

        let data_dir = std::env::var_os("BMUX_DATA_DIR").map_or_else(
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

        let runtime_dir = if let Some(path) = std::env::var_os("BMUX_RUNTIME_DIR") {
            PathBuf::from(path)
        } else if cfg!(unix) {
            std::env::var("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .map_or_else(|_| std::env::temp_dir().join("bmux"), |d| d.join("bmux"))
        } else {
            std::env::temp_dir().join("bmux")
        };

        Self::new(config_dir, runtime_dir, data_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigPaths, stable_fnv1a64};
    use std::path::PathBuf;

    #[test]
    fn server_socket_uses_runtime_dir() {
        let paths = ConfigPaths::new(
            PathBuf::from("/config"),
            PathBuf::from("/runtime"),
            PathBuf::from("/data"),
        );
        assert_eq!(paths.server_socket(), PathBuf::from("/runtime/server.sock"));
    }

    #[test]
    fn server_pid_file_uses_runtime_dir() {
        let paths = ConfigPaths::new(
            PathBuf::from("/config"),
            PathBuf::from("/runtime"),
            PathBuf::from("/data"),
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
        );
        assert_eq!(paths.server_named_pipe(), paths.server_named_pipe());
    }

    #[test]
    fn server_named_pipe_changes_with_runtime_dir() {
        let a = ConfigPaths::new(
            PathBuf::from("/config"),
            PathBuf::from("/runtime/a"),
            PathBuf::from("/data"),
        );
        let b = ConfigPaths::new(
            PathBuf::from("/config"),
            PathBuf::from("/runtime/b"),
            PathBuf::from("/data"),
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
        );
        assert!(paths.server_socket().ends_with("server.sock"));
    }

    #[test]
    fn hash_fnv1a_known_vector() {
        assert_eq!(stable_fnv1a64(b"bmux"), 0xbb09969bbc2c17fd);
    }
}
