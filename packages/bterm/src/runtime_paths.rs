//! Complete, explicit BMUX roots. Resolution never consults environment variables.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use bmux_config::{BmuxConfig, ConfigLoadOverrides, ConfigPaths};

/// Bterm's own locations, independent of host BMUX selectors and runtime roots.
/// Capturing these paths never creates directories or opens a companion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtermPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl BtermPaths {
    /// Fully isolated locations for an explicitly selected managed root.
    pub fn from_root(root: PathBuf) -> Result<Self> {
        validate_absolute(&root)?;
        Ok(Self {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            log_dir: root.join("logs"),
            cache_dir: root.join("cache"),
        })
    }

    /// Resolve only the supplied environment snapshot. BMUX variables are ignored.
    pub fn from_environment(cwd: &Path, env: &BTreeMap<String, OsString>) -> Result<Self> {
        Self::for_application("bterm", "BTERM", cwd, env)
    }

    pub(crate) fn for_application(
        application: &str,
        prefix: &str,
        cwd: &Path,
        env: &BTreeMap<String, OsString>,
    ) -> Result<Self> {
        validate_absolute(cwd)?;
        let home = env
            .get("HOME")
            .or_else(|| env.get("USERPROFILE"))
            .map(PathBuf::from);
        let selected = |name: &str, fallback: Result<PathBuf>| -> Result<PathBuf> {
            let path = if let Some(value) = env.get(&format!("{prefix}_{name}_DIR")) {
                PathBuf::from(value)
            } else {
                fallback?
            };
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            validate_absolute(&path)?;
            Ok(path)
        };
        let base = |key: &str, suffix: &str| -> Result<PathBuf> {
            if let Some(value) = env.get(key) {
                return Ok(PathBuf::from(value));
            }
            Ok(home
                .as_ref()
                .context("home directory missing from captured environment")?
                .join(suffix))
        };
        let config = if cfg!(target_os = "macos") {
            base("XDG_CONFIG_HOME", "Library/Application Support")
        } else if cfg!(windows) {
            base("APPDATA", "AppData/Roaming")
        } else {
            base("XDG_CONFIG_HOME", ".config")
        };
        let data = if cfg!(target_os = "macos") {
            base("XDG_DATA_HOME", "Library/Application Support")
        } else if cfg!(windows) {
            base("LOCALAPPDATA", "AppData/Local")
        } else {
            base("XDG_DATA_HOME", ".local/share")
        };
        let state = base("XDG_STATE_HOME", ".local/state");
        let cache = if cfg!(target_os = "macos") {
            base("XDG_CACHE_HOME", "Library/Caches")
        } else {
            base("XDG_CACHE_HOME", ".cache")
        };
        let state_dir = selected("STATE", state.map(|path| path.join(application)))?;
        let log_default = if cfg!(target_os = "macos") {
            base("XDG_LOG_HOME", "Library/Logs").map(|path| path.join(application))
        } else {
            Ok(state_dir.join("logs"))
        };
        Ok(Self {
            config_dir: selected("CONFIG", config.map(|path| path.join(application)))?,
            data_dir: selected("DATA", data.map(|path| path.join(application)))?,
            state_dir,
            log_dir: selected("LOG", log_default)?,
            cache_dir: selected("CACHE", cache.map(|path| path.join(application)))?,
        })
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("bterm.toml")
    }

    #[must_use]
    pub fn state_config_file(&self) -> PathBuf {
        self.state_dir.join("config-overlay.toml")
    }

    #[must_use]
    pub fn default_managed_root(&self) -> PathBuf {
        self.state_dir.join("bmux")
    }

    /// Explicit Bterm-owned installation roots; no BMUX selector redirects
    /// these roots. Configured selection derives boot/target transport later.
    /// The config directory is for private launch snapshots, not source discovery.
    /// Resolve complete roots without transport derivation; configured selection
    /// supplies the final short, target-scoped endpoint after hashing inputs.
    pub fn managed_base_roots(&self) -> Result<RuntimeRoots> {
        let mut roots = RuntimeRoots::from_root(self.default_managed_root())?;
        if !roots.data_dir.try_exists()? {
            roots.data_dir = self.data_dir.join("bmux");
        }
        Ok(roots)
    }

    pub fn managed_roots(&self) -> Result<RuntimeRoots> {
        self.managed_base_roots()
    }
}

/// A single runtime's complete configuration, endpoint, data and state namespace.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRoots {
    pub config_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl RuntimeRoots {
    /// Normalize existing ancestors too, so creation of an absent leaf cannot
    /// change its scope identity. Reject traversal rather than guessing intent.
    pub fn normalized(&self) -> Result<Self> {
        Ok(Self {
            config_dir: normalize_absolute(&self.config_dir)?,
            runtime_dir: normalize_absolute(&self.runtime_dir)?,
            data_dir: normalize_absolute(&self.data_dir)?,
            state_dir: normalize_absolute(&self.state_dir)?,
        })
    }

    /// Derive writable runtime-local state without feeding generated paths back
    /// into identity. Durable data stays explicit; callers must fence its writers.
    pub fn for_target(&self, target: &str) -> Result<Self> {
        self.for_target_boot(target, &boot_identity()?)
    }

    fn for_target_boot(&self, target: &str, boot: &str) -> Result<Self> {
        use sha2::{Digest as _, Sha256};
        use std::fmt::Write as _;
        ensure!(
            target.len() == 64
                && target
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "invalid managed target digest"
        );
        ensure!(
            boot.len() == 32 && boot.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid OS boot identity"
        );
        let roots = self.normalized()?;
        let mut digest = Sha256::new();
        digest.update(b"bterm-transport-v1\0");
        digest.update(target.as_bytes());
        digest.update(boot.as_bytes());
        let mut transport = String::with_capacity(64);
        for byte in digest.finalize() {
            write!(transport, "{byte:02x}")?;
        }
        // A fixed short platform path avoids sockaddr_un limits even when HOME
        // or explicit durable roots are long. Startup MUST secure this directory
        // to the effective user before opening locks, records, or endpoints.
        #[cfg(unix)]
        let transport_parent = {
            // SAFETY: geteuid has no arguments and only reads process identity.
            let uid = unsafe { libc::geteuid() };
            PathBuf::from(format!("/tmp/bt-{uid}"))
        };
        #[cfg(not(unix))]
        let transport_parent = roots.runtime_dir.join("targets");
        let runtime_dir = transport_parent.join(transport);
        #[cfg(unix)]
        ensure!(
            runtime_dir
                .join("server.sock")
                .as_os_str()
                .as_encoded_bytes()
                .len()
                < if cfg!(target_os = "macos") { 104 } else { 108 },
            "managed socket path exceeds platform limit"
        );
        let local = roots.state_dir.join("targets").join(target);
        Ok(Self {
            config_dir: local.join("config"),
            runtime_dir,
            data_dir: roots.data_dir,
            state_dir: roots.state_dir,
        })
    }

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

    /// Managed transport names are valid only for one OS boot. Durable roots do
    /// not move, and external selections deliberately retain their exact layout.
    pub fn managed(root: PathBuf) -> Result<Self> {
        Self::managed_for_boot(root, &boot_identity()?)
    }

    fn managed_for_boot(root: PathBuf, boot: &str) -> Result<Self> {
        ensure!(
            boot.len() == 32 && boot.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid OS boot identity"
        );
        let runtime_dir = root.join("run").join(boot);
        let mut roots = Self::from_root(root)?;
        // Keep the full boot UUID while fitting macOS's short sockaddr_un.
        roots.runtime_dir = runtime_dir;
        #[cfg(unix)]
        ensure!(
            roots
                .runtime_dir
                .join("server.sock")
                .as_os_str()
                .as_encoded_bytes()
                .len()
                < if cfg!(target_os = "macos") { 104 } else { 108 },
            "managed socket path is too long; select a shorter absolute managed root"
        );
        Ok(roots)
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

/// Canonicalize the longest existing ancestor, retaining absent leaves. This is
/// stable before/after first installation and does not create anything.
fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    validate_absolute(path)?;
    let clean: PathBuf = path
        .components()
        .filter(|part| *part != Component::CurDir)
        .collect();
    let mut ancestor = clean.as_path();
    let mut suffix = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for component in suffix.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Ok(metadata) = ancestor.symlink_metadata() {
                    ensure!(
                        !metadata.file_type().is_symlink(),
                        "cannot select a dangling runtime root symlink"
                    );
                }
                suffix.push(
                    ancestor
                        .file_name()
                        .context("runtime root has no existing ancestor")?
                        .to_os_string(),
                );
                ancestor = ancestor
                    .parent()
                    .context("runtime root has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("normalize selected runtime root"),
        }
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

/// Read the kernel's boot UUID, never wall time, process identity or a random
/// per-launch value. Failure must not silently select a different namespace.
fn boot_identity() -> Result<String> {
    #[cfg(target_os = "linux")]
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("read OS boot identity")?;
    #[cfg(target_os = "macos")]
    let value = {
        let mut buffer = [0_u8; 128];
        let mut length = buffer.len();
        // SAFETY: name is NUL-terminated; buffer and length describe writable
        // storage for this read-only sysctl. No new value is supplied.
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buffer.as_mut_ptr().cast(),
                &raw mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("read OS boot identity");
        }
        ensure!(length <= buffer.len(), "oversized OS boot identity");
        std::str::from_utf8(&buffer[..length])
            .context("invalid OS boot identity encoding")?
            .trim_end_matches('\0')
            .to_owned()
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("boot-scoped managed runtimes require Linux or macOS");
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let value = value.trim();
        ensure!(
            value.len() == 36
                && value.bytes().enumerate().all(|(index, byte)| {
                    if [8, 13, 18, 23].contains(&index) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                }),
            "invalid OS boot UUID"
        );
        Ok(value.replace('-', "").to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_transport_fits_long_durable_roots_and_is_boot_scoped() {
        let root = (0..30).fold(PathBuf::from("/explicit-installation"), |path, _| {
            path.join("long-directory")
        });
        let base = RuntimeRoots::from_root(root).unwrap();
        let target = "a".repeat(64);
        let first = base
            .for_target_boot(&target, "0123456789abcdef0123456789abcdef")
            .unwrap();
        let second = base
            .for_target_boot(&target, "fedcba9876543210fedcba9876543210")
            .unwrap();
        assert_ne!(first.runtime_dir, second.runtime_dir);
        assert_eq!(first.state_dir, base.state_dir);
        assert_eq!(first.data_dir, base.data_dir);
        assert_eq!(first.config_dir, second.config_dir);
        #[cfg(unix)]
        assert!(
            first
                .runtime_dir
                .join("server.sock")
                .as_os_str()
                .as_encoded_bytes()
                .len()
                < 104
        );
        assert!(
            base.for_target_boot("../escape", "0123456789abcdef0123456789abcdef")
                .is_err()
        );
    }

    #[test]
    fn absent_root_normalization_ignores_dot_components_without_creation() {
        let root = std::env::temp_dir().join(format!("bterm-absent-scope-{}", std::process::id()));
        let direct = RuntimeRoots::from_root(root.clone())
            .unwrap()
            .normalized()
            .unwrap();
        let dotted = RuntimeRoots::from_root(root.join("."))
            .unwrap()
            .normalized()
            .unwrap();
        assert_eq!(direct, dotted);
        assert!(!root.exists());
    }

    #[test]
    fn boot_namespace_changes_only_transport_and_keeps_checkout_isolation() {
        let root = PathBuf::from("/bterm-test");
        let first =
            RuntimeRoots::managed_for_boot(root.clone(), "0123456789abcdef0123456789abcdef")
                .unwrap();
        let again =
            RuntimeRoots::managed_for_boot(root.clone(), "0123456789abcdef0123456789abcdef")
                .unwrap();
        let reboot =
            RuntimeRoots::managed_for_boot(root.clone(), "fedcba9876543210fedcba9876543210")
                .unwrap();
        let external = RuntimeRoots::from_root(root).unwrap();
        assert_eq!(first, again);
        assert_ne!(first.runtime_dir, reboot.runtime_dir);
        assert_ne!(first.runtime_dir, external.runtime_dir);
        for roots in [&reboot, &external] {
            assert_eq!(first.config_dir, roots.config_dir);
            assert_eq!(first.data_dir, roots.data_dir);
            assert_eq!(first.state_dir, roots.state_dir);
        }
        let other = RuntimeRoots::managed_for_boot(
            PathBuf::from("/other-checkout"),
            "0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        assert_ne!(first.runtime_dir, other.runtime_dir);
        assert_ne!(first.data_dir, other.data_dir);
        assert_ne!(first.state_dir, other.state_dir);
    }

    #[test]
    fn boot_identity_is_stable_and_rejects_invalid_names_and_long_paths() {
        assert_eq!(boot_identity().unwrap(), boot_identity().unwrap());
        for invalid in [
            "",
            "../escape",
            "unknown",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert!(RuntimeRoots::managed_for_boot(PathBuf::from("/bterm-test"), invalid).is_err());
        }
        assert!(
            RuntimeRoots::managed_for_boot(
                PathBuf::from(format!("/{}", "x".repeat(108))),
                "0123456789abcdef0123456789abcdef"
            )
            .is_err()
        );
    }

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
            env_config_toml: None,
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
