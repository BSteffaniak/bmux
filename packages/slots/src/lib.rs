//! Slot manifest for bmux multi-version installs.
//!
//! A *slot* is a named, declaratively-described bmux install. Each slot owns
//! its own `bmux-<slot>` binary, config tree, runtime dir, data/state/log dirs,
//! and server socket. Slots are enumerated by a single declarative manifest at
//! `~/.config/bmux/slots.toml` (or wherever `BMUX_SLOTS_MANIFEST` points).
//!
//! ## Design rules
//!
//! - The manifest is the source of truth. This crate is read-only against it
//!   by default; write-side helpers are opt-in and refuse to modify
//!   read-only manifests (e.g. Nix store paths).
//! - Paths support `~` expansion and a strict `${NAME}` / `${NAME:-default}`
//!   env interpolation grammar. Nothing else.
//! - Composable via a top-level `extend = [...]` list of additional manifest
//!   files. Later entries override earlier ones at the slot-name level.
//! - The top-level `default` field is purely presentational — there is no shim
//!   binary, so it does not affect resolution.
//! - Slot names validate as `[A-Za-z0-9._-]+` and cannot be any reserved
//!   keyword ("default", "current", "all").
//!
//! ## Example
//!
//! ```toml
//! default = "stable"
//! extend = ["~/.config/bmux/slots.local.toml"]
//!
//! [slots.stable]
//! binary = "/usr/local/bin/bmux-stable"
//! inherit_base = true
//!
//! [slots.dev]
//! binary = "${BMUX_DEV_BIN:-/home/you/GitHub/bmux/target/release/bmux}"
//! inherit_base = false
//! ```

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Environment variable that points at the primary manifest file.
///
/// Defaults to `<config_dir>/slots.toml` when unset. A value of `-` means
/// "read the manifest from stdin" (resolved by callers, not this crate).
pub const SLOTS_MANIFEST_ENV: &str = "BMUX_SLOTS_MANIFEST";

/// Environment variable that forces the active slot name.
///
/// Takes precedence over argv[0] parsing. Used by `bmux-env exec`, tests, and
/// re-exec scenarios.
pub const SLOT_NAME_ENV: &str = "BMUX_SLOT_NAME";

/// Environment variable that names the directory containing `bmux-<slot>`
/// binaries. Defaults to `~/.local/bin` if unset.
pub const SLOTS_BIN_DIR_ENV: &str = "BMUX_SLOTS_BIN_DIR";

/// Environment variable that names the root under which per-slot default dirs
/// are materialized (when a slot does not explicitly override them).
///
/// Defaults to the platform-appropriate data dir (e.g. `~/.local/share/bmux`).
pub const SLOTS_ROOT_ENV: &str = "BMUX_SLOTS_ROOT";

/// Colon-separated path prefixes that, when a manifest file lives under one of
/// them, cause write-side helpers to refuse to modify the file.
///
/// Always includes the built-in read-only prefixes (`/nix/store`, `/etc`).
pub const MANIFEST_READ_ONLY_PREFIXES_ENV: &str = "BMUX_MANIFEST_READ_ONLY_PREFIXES";

/// Environment variable that, when truthy, disables merging the shared
/// `base.toml` layer for this invocation.
pub const NO_BASE_CONFIG_ENV: &str = "BMUX_NO_BASE_CONFIG";

/// Reserved slot names that may not be used.
pub const RESERVED_SLOT_NAMES: &[&str] = &["default", "current", "all"];

/// Default slot name used as the presentational "default" when the manifest
/// does not specify one and no slots are present.
pub const DEFAULT_SLOT_FALLBACK: &str = "stable";

/// Errors produced by manifest loading / slot resolution.
#[derive(Debug, Error)]
pub enum SlotError {
    /// Failure reading a manifest file from disk.
    #[error("failed to read manifest {path:?}: {source}")]
    Io {
        /// Path being read.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },

    /// Failure parsing a manifest file's TOML.
    #[error("failed to parse manifest {path:?}: {source}")]
    Parse {
        /// Path being parsed.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: toml::de::Error,
    },

    /// Slot name is invalid.
    #[error("invalid slot name {name:?}: {reason}")]
    InvalidName {
        /// The offending name.
        name: String,
        /// Human-readable reason.
        reason: &'static str,
    },

    /// Two slots declare the same runtime_dir.
    #[error(
        "slots {a:?} and {b:?} both resolve to runtime_dir {runtime_dir:?} — refusing to load (each slot must have a unique runtime_dir)"
    )]
    DuplicateRuntimeDir {
        /// First slot's name.
        a: String,
        /// Second slot's name.
        b: String,
        /// The shared runtime_dir.
        runtime_dir: PathBuf,
    },

    /// Requested slot is not present in the manifest.
    #[error("slot {name:?} not found in manifest (known slots: {known:?})")]
    UnknownSlot {
        /// Requested name.
        name: String,
        /// Known slots (for the error message).
        known: Vec<String>,
    },

    /// A referenced `extend` file could not be resolved.
    #[error("extend path {path:?} could not be resolved: {reason}")]
    ExtendFailed {
        /// Unresolved path.
        path: PathBuf,
        /// Reason.
        reason: String,
    },

    /// Write-side operation attempted on a manifest the loader considers
    /// declaratively-managed (e.g. Nix store).
    #[error(
        "refusing to modify read-only manifest {path:?} — it is under a declarative-management prefix"
    )]
    ReadOnlyManifest {
        /// Path refused.
        path: PathBuf,
    },

    /// Env interpolation failure (unknown variable with no default).
    #[error("unknown environment variable {name} referenced in {field} (slot {slot:?})")]
    UnknownEnvVar {
        /// Env var name.
        name: String,
        /// Which manifest field.
        field: String,
        /// Which slot.
        slot: String,
    },
}

/// The top-level manifest as parsed from TOML (pre-merge form).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SlotManifestFile {
    /// Presentational default slot name. Does not affect resolution.
    pub default: Option<String>,

    /// Additional manifest files to merge. Later entries override earlier.
    #[serde(default)]
    pub extend: Vec<String>,

    /// Slot definitions by name.
    #[serde(default)]
    pub slots: BTreeMap<String, SlotSpec>,
}

/// A single slot's declaration as it appears in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotSpec {
    /// Absolute path to the `bmux-<name>` binary.
    pub binary: String,

    /// Whether to layer-merge the shared `~/.config/bmux/base.toml` under
    /// this slot's config. Defaults to true.
    #[serde(default = "default_true")]
    pub inherit_base: bool,

    /// Optional explicit config directory override.
    pub config_dir: Option<String>,

    /// Optional explicit runtime directory override.
    pub runtime_dir: Option<String>,

    /// Optional explicit data directory override.
    pub data_dir: Option<String>,

    /// Optional explicit state directory override.
    pub state_dir: Option<String>,

    /// Optional explicit log directory override.
    pub log_dir: Option<String>,
}

#[must_use]
const fn default_true() -> bool {
    true
}

/// A fully-resolved slot: all paths expanded, interpolated, canonicalized.
#[derive(Debug, Clone)]
pub struct Slot {
    /// The slot's name.
    pub name: String,
    /// Absolute path to the slot's binary.
    pub binary: PathBuf,
    /// Whether to merge `base.toml` underneath this slot's config.
    pub inherit_base: bool,
    /// Config directory for this slot.
    pub config_dir: PathBuf,
    /// Runtime directory for this slot.
    pub runtime_dir: PathBuf,
    /// Data directory for this slot.
    pub data_dir: PathBuf,
    /// State directory for this slot.
    pub state_dir: PathBuf,
    /// Log directory for this slot.
    pub log_dir: PathBuf,
}

/// The fully-resolved manifest.
#[derive(Debug, Clone)]
pub struct SlotManifest {
    /// The canonical path of the primary manifest file that was loaded.
    /// `None` when the manifest came from stdin or from synthesis.
    pub source: Option<PathBuf>,
    /// Presentational default slot name. Falls back to `DEFAULT_SLOT_FALLBACK`
    /// if the file did not name one and at least one slot exists.
    pub default: Option<String>,
    /// Resolved slots by name.
    pub slots: BTreeMap<String, Slot>,
}

impl SlotManifest {
    /// Build an empty manifest (no slots). Useful as a fallback.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            source: None,
            default: None,
            slots: BTreeMap::new(),
        }
    }

    /// Return the presentational default slot name, if any.
    ///
    /// Rules: explicit `default` field wins. Otherwise, if exactly one slot
    /// exists, that is the default. Otherwise, if a slot named
    /// `DEFAULT_SLOT_FALLBACK` exists, it is the default. Otherwise `None`.
    #[must_use]
    pub fn resolved_default(&self) -> Option<&Slot> {
        if let Some(name) = self.default.as_deref()
            && let Some(slot) = self.slots.get(name)
        {
            return Some(slot);
        }
        if self.slots.len() == 1 {
            return self.slots.values().next();
        }
        self.slots.get(DEFAULT_SLOT_FALLBACK)
    }

    /// Look up a slot by name.
    ///
    /// # Errors
    ///
    /// Returns `SlotError::UnknownSlot` if the slot is not in the manifest.
    pub fn get(&self, name: &str) -> Result<&Slot, SlotError> {
        self.slots.get(name).ok_or_else(|| SlotError::UnknownSlot {
            name: name.to_string(),
            known: self.slots.keys().cloned().collect(),
        })
    }

    /// Load the manifest from the default source.
    ///
    /// Resolution order:
    /// 1. `BMUX_SLOTS_MANIFEST` env var
    /// 2. `<config_dir>/slots.toml`
    /// 3. If the resolved primary file does not exist, an empty manifest is
    ///    returned (not an error) so legacy single-install setups continue to
    ///    work.
    ///
    /// # Errors
    ///
    /// Returns errors for malformed TOML, invalid slot names, unresolvable
    /// `extend` paths, or duplicate-runtime-dir conflicts.
    pub fn load_default() -> Result<Self, SlotError> {
        let path = default_manifest_path();
        if path.as_os_str() == "-" {
            return Self::load_from_reader(&mut io::stdin().lock(), None);
        }
        if !path.exists() {
            return Ok(Self::empty());
        }
        Self::load_from_path(&path)
    }

    /// Load a manifest from an explicit path.
    ///
    /// # Errors
    ///
    /// Returns errors for I/O, parse, or validation failures.
    pub fn load_from_path(path: &Path) -> Result<Self, SlotError> {
        let bytes = std::fs::read(path).map_err(|source| SlotError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let text = std::str::from_utf8(&bytes).map_err(|_| SlotError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "manifest is not valid UTF-8"),
        })?;
        let file: SlotManifestFile = toml::from_str(text).map_err(|source| SlotError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        let mut merged = file;
        // Merge extends recursively, relative to this file's directory.
        let base_dir = path.parent().map(Path::to_path_buf);
        let mut stack = vec![(path.to_path_buf(), merged.extend.clone())];
        let mut seen = vec![path.to_path_buf()];
        merged.extend.clear();

        while let Some((origin, extends)) = stack.pop() {
            for raw in extends {
                let expanded = expand_path_string(&raw, None).map_err(|e| match e {
                    InterpolationError::UnknownVar(name) => SlotError::UnknownEnvVar {
                        name,
                        field: "extend".to_string(),
                        slot: "<top-level>".to_string(),
                    },
                })?;
                let resolved = resolve_extend_path(&origin, &expanded);
                if !resolved.exists() {
                    // Tolerate missing extend files — this enables optional
                    // local overrides that may or may not be present.
                    continue;
                }
                if seen.iter().any(|p| p == &resolved) {
                    // Cycle guard.
                    continue;
                }
                seen.push(resolved.clone());

                let extend_bytes = std::fs::read(&resolved).map_err(|source| SlotError::Io {
                    path: resolved.clone(),
                    source,
                })?;
                let extend_text =
                    std::str::from_utf8(&extend_bytes).map_err(|_| SlotError::Io {
                        path: resolved.clone(),
                        source: io::Error::new(
                            io::ErrorKind::InvalidData,
                            "manifest is not valid UTF-8",
                        ),
                    })?;
                let extend_file: SlotManifestFile =
                    toml::from_str(extend_text).map_err(|source| SlotError::Parse {
                        path: resolved.clone(),
                        source,
                    })?;

                // Slot entries from later-merged files override earlier ones.
                for (name, spec) in extend_file.slots {
                    merged.slots.insert(name, spec);
                }
                if extend_file.default.is_some() {
                    merged.default = extend_file.default;
                }
                stack.push((resolved, extend_file.extend));
            }
        }

        Self::from_file(merged, Some(path.to_path_buf()), base_dir.as_deref())
    }

    /// Parse a manifest from an arbitrary reader (no extend support).
    ///
    /// `origin` is only used for error messages.
    ///
    /// # Errors
    ///
    /// Returns errors for I/O, parse, or validation failures.
    pub fn load_from_reader<R: io::Read>(
        reader: &mut R,
        origin: Option<PathBuf>,
    ) -> Result<Self, SlotError> {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|source| SlotError::Io {
                path: origin.clone().unwrap_or_else(|| PathBuf::from("<stdin>")),
                source,
            })?;
        let text = std::str::from_utf8(&bytes).map_err(|_| SlotError::Io {
            path: origin.clone().unwrap_or_else(|| PathBuf::from("<stdin>")),
            source: io::Error::new(io::ErrorKind::InvalidData, "manifest is not valid UTF-8"),
        })?;
        let file: SlotManifestFile = toml::from_str(text).map_err(|source| SlotError::Parse {
            path: origin.clone().unwrap_or_else(|| PathBuf::from("<stdin>")),
            source,
        })?;
        Self::from_file(file, origin, None)
    }

    fn from_file(
        file: SlotManifestFile,
        source: Option<PathBuf>,
        _base_dir: Option<&Path>,
    ) -> Result<Self, SlotError> {
        let mut slots = BTreeMap::new();
        for (name, spec) in file.slots {
            validate_slot_name(&name)?;
            let resolved = resolve_slot(&name, &spec)?;
            slots.insert(name, resolved);
        }

        // Detect duplicate runtime_dirs.
        let mut seen: BTreeMap<PathBuf, String> = BTreeMap::new();
        for slot in slots.values() {
            if let Some(prior) = seen.insert(slot.runtime_dir.clone(), slot.name.clone()) {
                return Err(SlotError::DuplicateRuntimeDir {
                    a: prior,
                    b: slot.name.clone(),
                    runtime_dir: slot.runtime_dir.clone(),
                });
            }
        }

        Ok(Self {
            source,
            default: file.default,
            slots,
        })
    }
}

/// Validate a slot name.
///
/// # Errors
///
/// Returns `SlotError::InvalidName` for empty, non-matching, or reserved names.
pub fn validate_slot_name(name: &str) -> Result<(), SlotError> {
    if name.is_empty() {
        return Err(SlotError::InvalidName {
            name: name.to_string(),
            reason: "empty",
        });
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(SlotError::InvalidName {
            name: name.to_string(),
            reason: "must match [A-Za-z0-9._-]+",
        });
    }
    if RESERVED_SLOT_NAMES.contains(&name) {
        return Err(SlotError::InvalidName {
            name: name.to_string(),
            reason: "is reserved",
        });
    }
    Ok(())
}

fn resolve_slot(name: &str, spec: &SlotSpec) -> Result<Slot, SlotError> {
    let binary = expand_to_pathbuf(&spec.binary, name, "binary")?;
    let slot_default_root = default_slots_root().join(name);
    let config_dir = spec
        .config_dir
        .as_deref()
        .map(|s| expand_to_pathbuf(s, name, "config_dir"))
        .transpose()?
        .unwrap_or_else(|| default_config_root().join("slots").join(name));
    let runtime_dir = spec
        .runtime_dir
        .as_deref()
        .map(|s| expand_to_pathbuf(s, name, "runtime_dir"))
        .transpose()?
        .unwrap_or_else(|| default_runtime_root().join("slots").join(name));
    let data_dir = spec
        .data_dir
        .as_deref()
        .map(|s| expand_to_pathbuf(s, name, "data_dir"))
        .transpose()?
        .unwrap_or_else(|| slot_default_root.join("data"));
    let state_dir = spec
        .state_dir
        .as_deref()
        .map(|s| expand_to_pathbuf(s, name, "state_dir"))
        .transpose()?
        .unwrap_or_else(|| default_state_root().join("slots").join(name));
    let log_dir = spec
        .log_dir
        .as_deref()
        .map(|s| expand_to_pathbuf(s, name, "log_dir"))
        .transpose()?
        .unwrap_or_else(|| state_dir.join("logs"));

    Ok(Slot {
        name: name.to_string(),
        binary,
        inherit_base: spec.inherit_base,
        config_dir,
        runtime_dir,
        data_dir,
        state_dir,
        log_dir,
    })
}

fn expand_to_pathbuf(raw: &str, slot: &str, field: &str) -> Result<PathBuf, SlotError> {
    let expanded = expand_path_string(raw, Some(slot)).map_err(|e| match e {
        InterpolationError::UnknownVar(name) => SlotError::UnknownEnvVar {
            name,
            field: field.to_string(),
            slot: slot.to_string(),
        },
    })?;
    Ok(PathBuf::from(expanded))
}

/// Default manifest path: `BMUX_SLOTS_MANIFEST` env, or `<config_dir>/slots.toml`.
#[must_use]
pub fn default_manifest_path() -> PathBuf {
    if let Some(raw) = std::env::var_os(SLOTS_MANIFEST_ENV) {
        return PathBuf::from(raw);
    }
    default_config_root().join("slots.toml")
}

/// Default config root: `~/.config/bmux` (or platform equivalent).
///
/// Deliberately simpler than `bmux_config::paths::ConfigPaths` — this crate
/// can't depend on it.
#[must_use]
pub fn default_config_root() -> PathBuf {
    dirs::config_dir().map_or_else(
        || {
            dirs::home_dir().map_or_else(
                || PathBuf::from(".bmux"),
                |h| h.join(".config").join("bmux"),
            )
        },
        |d| d.join("bmux"),
    )
}

/// Default shared base-config path used as the low-precedence layer when a
/// slot has `inherit_base = true`.
#[must_use]
pub fn default_base_config_path() -> PathBuf {
    default_config_root().join("base.toml")
}

/// Default slots root: `$BMUX_SLOTS_ROOT` or platform data dir.
#[must_use]
pub fn default_slots_root() -> PathBuf {
    if let Some(raw) = std::env::var_os(SLOTS_ROOT_ENV) {
        return PathBuf::from(raw);
    }
    dirs::data_dir().map_or_else(
        || {
            dirs::home_dir().map_or_else(
                || PathBuf::from(".bmux").join("slots"),
                |h| h.join(".local").join("share").join("bmux").join("slots"),
            )
        },
        |d| d.join("bmux").join("slots"),
    )
}

fn default_runtime_root() -> PathBuf {
    if cfg!(unix) {
        std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("bmux")
    } else {
        std::env::temp_dir().join("bmux")
    }
}

fn default_state_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map_or_else(
        || {
            dirs::home_dir().map_or_else(
                || PathBuf::from(".bmux").join("state"),
                |h| h.join(".local").join("state").join("bmux"),
            )
        },
        |v| PathBuf::from(v).join("bmux"),
    )
}

/// Default bin dir for `bmux-<slot>` symlinks/binaries.
#[must_use]
pub fn default_bin_dir() -> PathBuf {
    if let Some(raw) = std::env::var_os(SLOTS_BIN_DIR_ENV) {
        return PathBuf::from(raw);
    }
    dirs::home_dir().map_or_else(
        || PathBuf::from(".bmux-bin"),
        |h| h.join(".local").join("bin"),
    )
}

fn resolve_extend_path(origin: &Path, raw: &str) -> PathBuf {
    let expanded = PathBuf::from(raw);
    if expanded.is_absolute() {
        return expanded;
    }
    if let Some(parent) = origin.parent() {
        return parent.join(&expanded);
    }
    expanded
}

/// Attempt to determine whether a manifest file is "declaratively-managed" —
/// i.e., under a read-only prefix like `/nix/store`.
///
/// Returns true when the file's canonical path starts with any of:
/// - built-in read-only prefixes (`/nix/store`, `/etc`)
/// - any entry in `BMUX_MANIFEST_READ_ONLY_PREFIXES` (colon-separated).
#[must_use]
pub fn is_read_only_manifest(path: &Path) -> bool {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let string = canonical.to_string_lossy();
    const BUILTIN: &[&str] = &["/nix/store/", "/etc/"];
    if BUILTIN.iter().any(|p| string.starts_with(p)) {
        return true;
    }
    if let Ok(extras) = std::env::var(MANIFEST_READ_ONLY_PREFIXES_ENV) {
        for p in extras.split(':').filter(|s| !s.is_empty()) {
            let trimmed = p.trim_end_matches('/');
            if string.starts_with(trimmed) {
                return true;
            }
        }
    }
    // Also treat any file that lacks write permission as read-only.
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().readonly()
    {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Path / env interpolation
// ---------------------------------------------------------------------------

/// Interpolation error.
#[derive(Debug)]
enum InterpolationError {
    UnknownVar(String),
}

/// Expand `~` (leading) and `${VAR}` / `${VAR:-default}` references.
///
/// Only the two forms are recognized; all other `$` sequences are emitted
/// verbatim so authors writing literal dollars in paths are not punished.
fn expand_path_string(raw: &str, _slot: Option<&str>) -> Result<String, InterpolationError> {
    let interpolated = interpolate_env(raw)?;
    Ok(expand_tilde(&interpolated))
}

fn interpolate_env(raw: &str) -> Result<String, InterpolationError> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Find matching close.
            if let Some(end) = raw[i + 2..].find('}') {
                let inner = &raw[i + 2..i + 2 + end];
                let (name, default) = if let Some((n, d)) = inner.split_once(":-") {
                    (n, Some(d))
                } else {
                    (inner, None)
                };
                if !is_valid_env_name(name) {
                    // Not a recognized interpolation; emit verbatim.
                    out.push('$');
                    out.push('{');
                    out.push_str(inner);
                    out.push('}');
                    i += end + 3;
                    continue;
                }
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        if let Some(d) = default {
                            out.push_str(d);
                        } else {
                            return Err(InterpolationError::UnknownVar(name.to_string()));
                        }
                    }
                }
                i += end + 3;
                continue;
            }
        }
        out.push(b as char);
        i += 1;
    }
    Ok(out)
}

fn is_valid_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

fn expand_tilde(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix('~') {
        if rest.is_empty() {
            if let Some(home) = dirs::home_dir() {
                return home.to_string_lossy().into_owned();
            }
            return "~".to_string();
        }
        if let Some(rest) = rest.strip_prefix('/')
            && let Some(home) = dirs::home_dir()
        {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Global lock for tests that manipulate env vars, to avoid cross-thread
    /// env races.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(ref p) = self.prev {
                unsafe { std::env::set_var(self.key, p) };
            } else {
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }

    #[test]
    fn validate_name_allows_alnum_dots_dashes_underscores() {
        validate_slot_name("stable").unwrap();
        validate_slot_name("dev.2").unwrap();
        validate_slot_name("feat-xyz").unwrap();
        validate_slot_name("a_b-c.d").unwrap();
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_slot_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_spaces_and_slashes() {
        assert!(validate_slot_name("has space").is_err());
        assert!(validate_slot_name("has/slash").is_err());
        assert!(validate_slot_name("weird$chars").is_err());
    }

    #[test]
    fn validate_name_rejects_reserved() {
        for reserved in RESERVED_SLOT_NAMES {
            assert!(
                validate_slot_name(reserved).is_err(),
                "{reserved} should be rejected"
            );
        }
    }

    #[test]
    fn tilde_expansion() {
        let _g = env_lock().lock().unwrap();
        let out = expand_tilde("~/foo");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(out, home.join("foo").to_string_lossy());
        }
        assert_eq!(expand_tilde("noTilde"), "noTilde");
        assert_eq!(expand_tilde("/abs/~notme"), "/abs/~notme");
    }

    #[test]
    fn env_interpolation_basic() {
        let _g = env_lock().lock().unwrap();
        let _h = EnvGuard::set("BMUX_TEST_VAR_X", "hello");
        let out = interpolate_env("prefix-${BMUX_TEST_VAR_X}-suffix").unwrap();
        assert_eq!(out, "prefix-hello-suffix");
    }

    #[test]
    fn env_interpolation_default() {
        let _g = env_lock().lock().unwrap();
        let _h = EnvGuard::unset("BMUX_TEST_VAR_Y");
        let out = interpolate_env("${BMUX_TEST_VAR_Y:-fallback}").unwrap();
        assert_eq!(out, "fallback");
    }

    #[test]
    fn env_interpolation_missing_is_error_without_default() {
        let _g = env_lock().lock().unwrap();
        let _h = EnvGuard::unset("BMUX_TEST_VAR_Z");
        let err = interpolate_env("${BMUX_TEST_VAR_Z}").err();
        assert!(matches!(err, Some(InterpolationError::UnknownVar(_))));
    }

    #[test]
    fn env_interpolation_preserves_non_matches() {
        let _g = env_lock().lock().unwrap();
        let out = interpolate_env("$abc ${with space} ${}").unwrap();
        // $abc is not ${...}; ${with space} and ${} have invalid names → verbatim
        assert_eq!(out, "$abc ${with space} ${}");
    }

    #[test]
    fn slot_spec_round_trips_through_toml() {
        let toml_text = r#"
default = "stable"

[slots.stable]
binary = "/usr/local/bin/bmux-stable"

[slots.dev]
binary = "/home/dev/target/release/bmux"
inherit_base = false
"#;
        let file: SlotManifestFile = toml::from_str(toml_text).unwrap();
        assert_eq!(file.default.as_deref(), Some("stable"));
        assert_eq!(file.slots.len(), 2);
        let stable = &file.slots["stable"];
        assert_eq!(stable.binary, "/usr/local/bin/bmux-stable");
        assert!(stable.inherit_base);
        assert!(!file.slots["dev"].inherit_base);
    }

    #[test]
    fn manifest_load_from_file_and_get() {
        let _g = env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("slots.toml");
        std::fs::write(
            &path,
            r#"
default = "stable"

[slots.stable]
binary = "/bin/bmux-stable"
runtime_dir = "/tmp/runtime-stable"

[slots.dev]
binary = "/bin/bmux-dev"
runtime_dir = "/tmp/runtime-dev"
"#,
        )
        .unwrap();
        let m = SlotManifest::load_from_path(&path).unwrap();
        assert_eq!(m.slots.len(), 2);
        let stable = m.get("stable").unwrap();
        assert_eq!(stable.binary, PathBuf::from("/bin/bmux-stable"));
        assert_eq!(stable.runtime_dir, PathBuf::from("/tmp/runtime-stable"));
        assert!(stable.inherit_base);
        assert!(m.get("nope").is_err());
    }

    #[test]
    fn manifest_detects_duplicate_runtime_dir() {
        let _g = env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("slots.toml");
        std::fs::write(
            &path,
            r#"
[slots.a]
binary = "/bin/a"
runtime_dir = "/tmp/shared"

[slots.b]
binary = "/bin/b"
runtime_dir = "/tmp/shared"
"#,
        )
        .unwrap();
        let err = SlotManifest::load_from_path(&path).unwrap_err();
        assert!(matches!(err, SlotError::DuplicateRuntimeDir { .. }));
    }

    #[test]
    fn manifest_extend_merges_files() {
        let _g = env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let child = tmp.path().join("child.toml");
        std::fs::write(
            &child,
            r#"
[slots.extra]
binary = "/bin/extra"
runtime_dir = "/tmp/extra"
"#,
        )
        .unwrap();
        let primary = tmp.path().join("slots.toml");
        std::fs::write(
            &primary,
            format!(
                r#"
extend = ["{}"]

[slots.main]
binary = "/bin/main"
runtime_dir = "/tmp/main"
"#,
                child.display(),
            ),
        )
        .unwrap();
        let m = SlotManifest::load_from_path(&primary).unwrap();
        assert!(m.slots.contains_key("main"));
        assert!(m.slots.contains_key("extra"));
    }

    #[test]
    fn manifest_resolved_default_prefers_explicit_then_singleton() {
        let mut m = SlotManifest::empty();
        m.slots.insert(
            "only".to_string(),
            Slot {
                name: "only".into(),
                binary: PathBuf::from("/x"),
                inherit_base: true,
                config_dir: PathBuf::from("/c"),
                runtime_dir: PathBuf::from("/r"),
                data_dir: PathBuf::from("/d"),
                state_dir: PathBuf::from("/s"),
                log_dir: PathBuf::from("/l"),
            },
        );
        assert_eq!(m.resolved_default().unwrap().name, "only");
        m.default = Some("nosuch".into());
        // Explicit but missing falls back to singleton.
        assert_eq!(m.resolved_default().unwrap().name, "only");
    }

    #[test]
    fn is_read_only_manifest_detects_nix_store_prefix() {
        // We cannot write into /nix/store during tests; exercise the
        // prefix-match logic by constructing a PathBuf in-memory.
        // (canonicalize will fail and we fall through to the raw path match.)
        let p = PathBuf::from("/nix/store/abc-123/slots.toml");
        assert!(is_read_only_manifest(&p));
    }
}
