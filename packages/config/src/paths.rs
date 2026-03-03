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
        self.data_dir.join("logs")
    }

    /// Get persisted local runtime state file path
    #[must_use]
    pub fn runtime_layout_state_file(&self) -> PathBuf {
        self.data_dir.join("runtime").join("last-layout.json")
    }

    /// Get persisted protocol trace file path
    #[must_use]
    pub fn protocol_trace_file(&self) -> PathBuf {
        self.data_dir.join("runtime").join("protocol-trace.json")
    }

    /// Get the server socket path
    #[must_use]
    pub fn server_socket(&self) -> PathBuf {
        self.runtime_dir.join("server.sock")
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
        std::fs::create_dir_all(self.plugins_dir())?;
        std::fs::create_dir_all(self.sessions_dir())?;
        std::fs::create_dir_all(self.logs_dir())?;
        std::fs::create_dir_all(self.config_dir.join("themes"))?;
        Ok(())
    }
}

impl Default for ConfigPaths {
    fn default() -> Self {
        let config_dir = dirs::config_dir().map_or_else(
            || {
                dirs::home_dir().map_or_else(
                    || PathBuf::from(".bmux"),
                    |d| d.join(".config").join("bmux"),
                )
            },
            |d| d.join("bmux"),
        );

        let data_dir = dirs::data_dir().map_or_else(
            || {
                dirs::home_dir().map_or_else(
                    || PathBuf::from(".bmux"),
                    |d| d.join(".local").join("share").join("bmux"),
                )
            },
            |d| d.join("bmux"),
        );

        let runtime_dir = if cfg!(unix) {
            std::env::var("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .map_or_else(|_| std::env::temp_dir().join("bmux"), |d| d.join("bmux"))
        } else {
            std::env::temp_dir().join("bmux")
        };

        Self::new(config_dir, runtime_dir, data_dir)
    }
}
