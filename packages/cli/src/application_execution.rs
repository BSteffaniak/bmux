//! Validated application-layer handoff from a BMUX sandbox to a companion.
//!
//! Call recognition on the original captured environment, before removing selectors
//! or choosing application defaults. Errors must not fall back to host execution.
//! This is consistency/membership validation, not authentication against a malicious
//! owner of the sandbox. Consumers must use the returned paths and environment,
//! disable ordinary host configuration discovery, and never clean up this borrowed
//! root. Explicit application roots/external endpoints require a separate conflict
//! check by the consumer. The BMUX artifact below is NOT a Bterm executable.
use anyhow::{Context, Result, bail, ensure};
use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Environment handoff selector; its value is the absolute descriptor path.
pub const CONTEXT_ENV: &str = "BTERM_EXECUTION_CONTEXT";
const DESCRIPTOR_FILE: &str = "application-execution.json";

pub(crate) fn handoff_path(root: &Path) -> PathBuf {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .join(DESCRIPTOR_FILE)
}

/// Environment captured before selector stripping or application default selection.
#[derive(Debug, Clone, Default)]
pub struct CapturedEnvironment(pub BTreeMap<OsString, OsString>);

impl CapturedEnvironment {
    /// Capture once, without mutating the process environment.
    #[must_use]
    pub fn capture() -> Self {
        Self(std::env::vars_os().collect())
    }
}

/// Configuration authority, independent of executable artifact selection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum ConfigurationInheritance {
    /// Use only sandbox-local configuration, never ordinary host discovery.
    Clean,
    /// Use exactly this copied configuration, never reload its host sources.
    /// Snapshot contents can intentionally reference external services/artifacts;
    /// applications must not treat those references as permission to share state.
    Snapshot { path: PathBuf, sha256: String },
}

/// Artifact proof for the selected BMUX binary, not a companion launch instruction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentity {
    pub path: PathBuf,
    pub sha256: String,
}

/// A borrowed sandbox. Dropping this value has no lifecycle side effects.
#[derive(Debug, Clone)]
pub struct ValidatedExecutionContext {
    pub sandbox_id: String,
    pub root: PathBuf,
    pub config_paths: ConfigPaths,
    pub log_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub home_dir: PathBuf,
    pub bmux_artifact: ArtifactIdentity,
    pub configuration: ConfigurationInheritance,
    /// Frozen Bterm source documents captured by the inherited-config launcher.
    /// Origins are provenance only: consumers must resolve these values directly,
    /// never reload them or reopen their host paths. `None` means clean defaults.
    pub bterm_configuration: Option<bmux_config::application::ConfigFamily>,
    /// Exact child environment policy: replace, do not merge with host selectors.
    /// Clean/hermetic modes allow only launcher shell basics; inherited mode keeps
    /// ordinary shell variables. All modes replace BMUX/BTERM/XDG selectors and
    /// home/temporary directories. This is not OS-level filesystem containment.
    pub environment: CapturedEnvironment,
}

/// Captured authority for an explicitly selected, attach-only slot.
///
/// This is not a verified companion bundle and grants no launch, replacement,
/// cleanup, or environment-installation authority. Directory selectors from the
/// process environment do not override the manifest's resolved slot paths.
#[derive(Debug, Clone)]
pub struct ValidatedSlotContext {
    pub name: String,
    pub manifest: PathBuf,
    pub config_paths: ConfigPaths,
    pub log_dir: PathBuf,
    pub bmux_artifact: ArtifactIdentity,
    /// The manifest's inheritance setting, before the captured no-base switch.
    pub inherit_base: bool,
    /// Shared base selected by the existing slot policy, unless disabled.
    /// Absence of this optional file is not an attachment failure.
    pub base_config_path: Option<PathBuf>,
}

/// Capture exactly one explicitly named slot from an explicit absolute manifest.
///
/// Uses the existing slot resolver, including extension order, interpolation,
/// defaults, name validation and duplicate-endpoint checks. No process state is
/// mutated and no directories are created. The binary is validated and hashed,
/// but is never executed or substituted for a managed companion.
///
/// The current slot resolver evaluates the process environment internally. Until
/// it accepts an injected environment, this bounded adapter requires the supplied
/// snapshot to match the process environment before AND after resolution. A caller
/// with a sanitized/different snapshot must not silently fall back to host policy.
/// As with ordinary environment capture, callers must not concurrently mutate the
/// process environment during capture.
///
/// # Errors
/// Rejects relative manifests, changed/mismatched capture environments, invalid
/// slot manifests/names, missing artifacts, non-executable binaries, and missing,
/// relative or traversal-containing selected directories. Never adopts an ambient
/// `BMUX_SLOT_NAME`, another slot, or a managed runtime on failure.
pub fn capture_slot(
    name: &str,
    manifest: &Path,
    captured: &CapturedEnvironment,
) -> Result<ValidatedSlotContext> {
    ensure!(
        manifest.is_absolute(),
        "slot manifest must be explicitly absolute"
    );
    bmux_slots::validate_slot_name(name)?;
    ensure!(
        captured.0 == std::env::vars_os().collect::<BTreeMap<_, _>>(),
        "slot resolver requires an unchanged process environment capture"
    );
    let manifest = manifest
        .canonicalize()
        .context("resolve explicit slot manifest")?;
    ensure!(manifest.is_file(), "slot manifest is not a regular file");
    let resolved = bmux_slots::SlotManifest::load_from_path(&manifest)?;
    let slot = resolved.get(name)?;
    for path in [
        &slot.config_dir,
        &slot.runtime_dir,
        &slot.data_dir,
        &slot.state_dir,
        &slot.log_dir,
    ] {
        ensure!(
            path.is_absolute()
                && !path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir)),
            "slot directory must be absolute and without traversal: {}",
            path.display()
        );
        ensure!(path.is_dir(), "missing slot directory: {}", path.display());
    }
    ensure!(
        slot.binary.is_absolute()
            && !slot
                .binary
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
        "slot binary must be absolute and without traversal"
    );
    let binary = slot
        .binary
        .canonicalize()
        .context("resolve selected slot binary")?;
    ensure!(
        binary.is_file(),
        "selected slot binary is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(
            std::fs::metadata(&binary)?.permissions().mode() & 0o111 != 0,
            "selected slot binary is not executable"
        );
    }
    let artifact = ArtifactIdentity {
        sha256: digest(&binary)?,
        path: binary,
    };
    let no_base = captured
        .0
        .get(OsStr::new(bmux_slots::NO_BASE_CONFIG_ENV))
        .is_some_and(|value| {
            matches!(
                value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        });
    let base_config_path =
        (slot.inherit_base && !no_base).then(bmux_slots::default_base_config_path);
    if let Some(path) = &base_config_path {
        ensure!(
            path.is_absolute(),
            "slot base configuration path must be absolute"
        );
    }
    ensure!(
        captured.0 == std::env::vars_os().collect::<BTreeMap<_, _>>(),
        "process environment changed during explicit slot capture"
    );
    Ok(ValidatedSlotContext {
        name: slot.name.clone(),
        manifest,
        config_paths: ConfigPaths::new(
            slot.config_dir.clone(),
            slot.runtime_dir.clone(),
            slot.data_dir.clone(),
            slot.state_dir.clone(),
        ),
        log_dir: slot.log_dir.clone(),
        bmux_artifact: artifact,
        inherit_base: slot.inherit_base,
        base_config_path,
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    version: u32,
    root: PathBuf,
    sandbox_id: String,
    env_mode: String,
    bmux_artifact: ArtifactIdentity,
    configuration: ConfigurationInheritance,
    #[serde(default)]
    bterm_snapshot: Option<ArtifactIdentity>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenDocument {
    origin: PathBuf,
    required: bool,
    value: Option<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Development {
    version: u32,
    binary: PathBuf,
    binary_digest: String,
    cwd: PathBuf,
}

fn digest(path: &Path) -> Result<String> {
    use std::fmt::Write as _;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("cannot read execution-context artifact {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let mut hex = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(hex)
}

// Require canonical spellings, not just equivalent paths: reject traversal,
// symlinked owned directories, and missing directories rather than guessing.
fn exact_directory(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && path.is_dir(),
        "missing absolute sandbox directory: {}",
        path.display()
    );
    ensure!(
        path.canonicalize()? == path,
        "non-canonical sandbox directory: {}",
        path.display()
    );
    Ok(())
}

fn exact_file(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && path.is_file(),
        "missing absolute sandbox file: {}",
        path.display()
    );
    ensure!(
        path.canonicalize()? == path,
        "non-canonical sandbox file: {}",
        path.display()
    );
    Ok(())
}

/// Write the versioned companion handoff after the launcher has written its manifest.
/// This does not change BMUX configuration or lifecycle semantics.
pub(crate) fn write_sandbox_handoff(root: &Path) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    let manifest = crate::sandbox_meta::read_manifest(&root)?;
    let binary = Path::new(&manifest.bmux_bin).canonicalize()?;
    let configuration = if root.join("development.json").exists() {
        let path = root.join("config/bmux/bmux.toml");
        ConfigurationInheritance::Snapshot {
            sha256: digest(&path)?,
            path,
        }
    } else {
        ConfigurationInheritance::Clean
    };
    let bterm_path = root.join("bterm-configuration.json");
    let bterm_snapshot = if bterm_path.exists() {
        Some(ArtifactIdentity {
            sha256: digest(&bterm_path)?,
            path: bterm_path,
        })
    } else {
        None
    };
    let descriptor = Descriptor {
        version: 1,
        root: root.clone(),
        sandbox_id: manifest.id,
        env_mode: manifest.env_mode,
        bmux_artifact: ArtifactIdentity {
            sha256: digest(&binary)?,
            path: binary,
        },
        configuration,
        bterm_snapshot,
    };
    let path = root.join(DESCRIPTOR_FILE);
    crate::sandbox_meta::write_atomic_file(&path, &serde_json::to_vec(&descriptor)?)?;
    Ok(path)
}

/// Recognize a complete sandbox, or reject unsafe/unsupported ambient selection.
///
/// `explicit_descriptor` is a descriptor path, not an application root. If both
/// selectors are supplied they must agree exactly. Slot-only selection currently
/// fails closed: a slot's BMUX binary is not a bundled companion identity.
/// `Ok(None)` means no execution-context selection was present. Ordinary
/// `BMUX_CONFIG`, `BMUX_CONFIG_DIR`, `BMUX_BASE_CONFIG`, and
/// `BMUX_NO_BASE_CONFIG` preferences alone are not execution identities. Neither
/// are pane hints, terminal capabilities, logging or build metadata. The default
/// runtime name alone is neutral; non-default runtime selection requires a handoff.
///
/// # Errors
/// Returns an error for partial selectors, slots, invalid membership/layout,
/// unsupported descriptors, changed artifacts/snapshots, or conflicting selection.
pub fn recognize(
    captured: &CapturedEnvironment,
    explicit_descriptor: Option<&Path>,
) -> Result<Option<ValidatedExecutionContext>> {
    let ambient = captured.0.get(OsStr::new(CONTEXT_ENV)).map(PathBuf::from);
    if let (Some(explicit), Some(ambient)) = (explicit_descriptor, ambient.as_deref()) {
        ensure!(
            explicit == ambient,
            "conflicting application execution-context selectors"
        );
    }
    let Some(path) = explicit_descriptor.or(ambient.as_deref()) else {
        ensure!(
            !captured.0.iter().any(|(key, value)| {
                let key = key.to_string_lossy();
                is_execution_selector(&key) && !(key == "BMUX_RUNTIME_NAME" && value == "default")
            }),
            "partial or unsupported BMUX execution context; refusing host fallback"
        );
        return Ok(None);
    };
    exact_file(path)?;
    let descriptor: Descriptor = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        descriptor.version == 1,
        "unsupported application execution-context version"
    );
    validate_membership(&descriptor, path)?;
    let bterm_configuration = read_bterm_snapshot(&descriptor)?;
    let root = &descriptor.root;
    validate_configuration(&descriptor)?;
    validate_selectors(captured, &descriptor)?;
    let config_paths = ConfigPaths::new(
        root.join("config/bmux"),
        root.join("runtime"),
        root.join("data/bmux"),
        root.join("state"),
    );
    // Development launcher inheritance is independent of sandbox --env-mode.
    let env_mode = match &descriptor.configuration {
        ConfigurationInheritance::Snapshot { .. } => "inherit",
        ConfigurationInheritance::Clean => descriptor.env_mode.as_str(),
    };
    let environment = isolated_environment(captured, path, root, env_mode);
    Ok(Some(ValidatedExecutionContext {
        sandbox_id: descriptor.sandbox_id,
        root: root.clone(),
        config_paths,
        log_dir: root.join("logs"),
        tmp_dir: root.join("tmp"),
        home_dir: root.join("home"),
        bmux_artifact: descriptor.bmux_artifact,
        configuration: descriptor.configuration,
        bterm_configuration,
        environment,
    }))
}

// These select an endpoint, owned state, or attachment identity. Do not infer
// execution authority from the BMUX_ namespace: pane hints, logging, terminal
// capabilities and build-tool metadata are inherited by ordinary child apps.
fn is_execution_selector(key: &str) -> bool {
    matches!(
        key,
        "BMUX_RUNTIME_DIR"
            | "BMUX_RUNTIME_NAME"
            | "BMUX_DATA_DIR"
            | "BMUX_STATE_DIR"
            | "BMUX_LOG_DIR"
            | "BMUX_TARGET"
            | "BMUX_SOCKET"
            | "BMUX_CONTROL_PLANE_URL"
            | "BMUX_SESSION_ID"
    ) || key.starts_with("BMUX_SLOT_")
        || key.starts_with("BMUX_SANDBOX_")
}

fn is_config_preference(key: &str) -> bool {
    matches!(
        key,
        "BMUX_CONFIG" | "BMUX_CONFIG_DIR" | "BMUX_BASE_CONFIG" | "BMUX_NO_BASE_CONFIG"
    )
}

/// Capture ordinary Bterm base/primary/state/environment documents, without
/// resolving profiles or reopening them later. Local scoped files and CLI Bterm
/// overrides are not selected by the BMUX sandbox launcher.
pub(crate) fn capture_bterm_snapshot(
    captured: &CapturedEnvironment,
    cwd: &Path,
) -> Result<Vec<u8>> {
    use bmux_config::application::{ConfigFamily, ConfigSource};
    let env = &captured.0;
    let get = |key: &str| env.get(OsStr::new(key));
    let home = || {
        get("HOME")
            .or_else(|| get("USERPROFILE"))
            .map(PathBuf::from)
            .context("cannot capture Bterm configuration without HOME or explicit Bterm roots")
    };
    let primary = if let Some(directory) = get("BTERM_CONFIG_DIR") {
        PathBuf::from(directory).join("bterm.toml")
    } else {
        let (key, suffix) = if cfg!(target_os = "macos") {
            ("XDG_CONFIG_HOME", "Library/Application Support")
        } else if cfg!(windows) {
            ("APPDATA", "AppData/Roaming")
        } else {
            ("XDG_CONFIG_HOME", ".config")
        };
        get(key)
            .map_or_else(
                || home().map(|path| path.join(suffix)),
                |value| Ok(PathBuf::from(value)),
            )?
            .join("bterm/bterm.toml")
    };
    let state = if let Some(directory) = get("BTERM_STATE_DIR") {
        PathBuf::from(directory)
    } else {
        get("XDG_STATE_HOME")
            .map_or_else(
                || home().map(|path| path.join(".local/state")),
                |value| Ok(PathBuf::from(value)),
            )?
            .join("bterm")
    }
    .join("config-overlay.toml");
    let mut sources = Vec::new();
    let no_base = get("BTERM_NO_BASE_CONFIG").is_some_and(|value| {
        matches!(
            value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    });
    if !no_base && let Some(path) = get("BTERM_BASE_CONFIG") {
        sources.push(ConfigSource {
            path: PathBuf::from(path),
            required: false,
        });
    }
    sources.extend([
        ConfigSource {
            path: primary,
            required: false,
        },
        ConfigSource {
            path: state,
            required: false,
        },
    ]);
    if let Some(path) = get("BTERM_CONFIG") {
        sources.push(ConfigSource {
            path: PathBuf::from(path),
            required: true,
        });
    }
    for source in &mut sources {
        if !source.path.is_absolute() {
            source.path = cwd.join(&source.path);
        }
    }
    let family = ConfigFamily::capture(&sources)?;
    let documents: Vec<_> = family
        .documents
        .into_iter()
        .map(|document| FrozenDocument {
            origin: document.source.path,
            required: document.source.required,
            value: document.value,
        })
        .collect();
    Ok(serde_json::to_vec(&documents)?)
}

pub(crate) fn inherited_application_configuration(
    frozen_bterm: &[u8],
    paths: &ConfigPaths,
    overrides: &bmux_config::ConfigLoadOverrides,
) -> Result<toml::Value> {
    use bmux_config::{ConfigDocument, ConfigFamily, ConfigSource};
    let documents: Vec<FrozenDocument> = serde_json::from_slice(frozen_bterm)?;
    let family = ConfigFamily {
        documents: documents
            .into_iter()
            .map(|document| ConfigDocument {
                source: ConfigSource {
                    path: document.origin,
                    required: document.required,
                },
                value: document.value,
                local: false,
            })
            .collect(),
    };
    let bmux = bmux_config::capture_host_configuration(paths, overrides, None)?;
    let resolved = bmux_config::resolve_application_configuration_with_context(
        &family,
        &bmux,
        None,
        None,
        &bmux_config::ConfigEvaluationContext::capture(),
    )?;
    Ok(resolved.bmux_raw)
}

fn read_bterm_snapshot(
    descriptor: &Descriptor,
) -> Result<Option<bmux_config::application::ConfigFamily>> {
    use bmux_config::application::{ConfigDocument, ConfigFamily, ConfigSource};
    let Some(snapshot) = &descriptor.bterm_snapshot else {
        ensure!(
            !descriptor.root.join("bterm-configuration.json").exists(),
            "missing Bterm snapshot proof"
        );
        return Ok(None);
    };
    ensure!(
        matches!(
            descriptor.configuration,
            ConfigurationInheritance::Snapshot { .. }
        ),
        "clean sandbox cannot import Bterm host settings"
    );
    ensure!(
        snapshot.path == descriptor.root.join("bterm-configuration.json"),
        "Bterm snapshot escapes sandbox"
    );
    exact_file(&snapshot.path)?;
    ensure!(
        digest(&snapshot.path)? == snapshot.sha256,
        "Bterm configuration snapshot changed"
    );
    let documents: Vec<FrozenDocument> = serde_json::from_slice(&std::fs::read(&snapshot.path)?)?;
    ensure!(
        documents
            .iter()
            .all(|document| document.origin.is_absolute()
                && (!document.required || document.value.is_some())),
        "invalid frozen Bterm source"
    );
    Ok(Some(ConfigFamily {
        documents: documents
            .into_iter()
            .map(|document| ConfigDocument {
                source: ConfigSource {
                    path: document.origin,
                    required: document.required,
                },
                value: document.value,
                local: false,
            })
            .collect(),
    }))
}

fn validate_membership(descriptor: &Descriptor, path: &Path) -> Result<()> {
    let root = &descriptor.root;
    exact_directory(root)?;
    ensure!(
        path == root.join(DESCRIPTOR_FILE),
        "execution descriptor/root mismatch"
    );
    exact_file(&root.join(crate::sandbox_meta::MANIFEST_FILE))?;
    let config_file = root.join("config/bmux/bmux.toml");
    if config_file.symlink_metadata().is_ok() {
        exact_file(&config_file)?;
    }
    let manifest = crate::sandbox_meta::read_manifest(root)?;
    ensure!(
        manifest.id == descriptor.sandbox_id
            && manifest.id == crate::sandbox_meta::sandbox_id_from_root(root),
        "sandbox identity mismatch"
    );
    ensure!(
        !Path::new(&manifest.paths.root)
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir)),
        "sandbox manifest root contains traversal"
    );
    ensure!(
        Path::new(&manifest.paths.root).is_absolute()
            && Path::new(&manifest.paths.root).canonicalize()? == *root,
        "sandbox manifest root mismatch"
    );
    ensure!(
        manifest.env_mode == descriptor.env_mode
            && matches!(manifest.env_mode.as_str(), "clean" | "inherit" | "hermetic"),
        "sandbox environment mode mismatch"
    );
    for (actual, relative) in [
        (&manifest.paths.runtime, "runtime"),
        (&manifest.paths.state, "state"),
        (&manifest.paths.logs, "logs"),
    ] {
        ensure!(
            Path::new(actual) == Path::new(&manifest.paths.root).join(relative),
            "sandbox manifest layout mismatch"
        );
        ensure!(
            Path::new(actual).canonicalize()? == root.join(relative),
            "sandbox manifest directory escapes root"
        );
    }
    for relative in [
        "config",
        "config/bmux",
        "runtime",
        "data",
        "data/bmux",
        "state",
        "logs",
        "tmp",
        "home",
    ] {
        exact_directory(&root.join(relative))?;
    }
    let artifact = &descriptor.bmux_artifact;
    exact_file(&artifact.path)?;
    ensure!(
        Path::new(&manifest.bmux_bin).canonicalize()? == artifact.path,
        "selected BMUX artifact mismatch"
    );
    ensure!(
        digest(&artifact.path)? == artifact.sha256,
        "selected BMUX binary digest mismatch"
    );
    Ok(())
}

fn validate_configuration(descriptor: &Descriptor) -> Result<()> {
    let development_path = descriptor.root.join("development.json");
    match &descriptor.configuration {
        ConfigurationInheritance::Clean => ensure!(
            !development_path.exists(),
            "missing inherited snapshot proof"
        ),
        ConfigurationInheritance::Snapshot { path, sha256 } => {
            ensure!(
                *path == descriptor.root.join("config/bmux/bmux.toml"),
                "snapshot path escapes sandbox"
            );
            exact_file(path)?;
            ensure!(
                digest(path)? == *sha256,
                "sandbox configuration snapshot changed"
            );
            exact_file(&development_path)?;
            let development: Development =
                serde_json::from_slice(&std::fs::read(development_path)?)?;
            ensure!(
                development.version == 1 && development.cwd.is_absolute(),
                "invalid development descriptor"
            );
            ensure!(
                development.binary == descriptor.bmux_artifact.path
                    && development.binary_digest == descriptor.bmux_artifact.sha256,
                "development artifact mismatch"
            );
            let config: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
            ensure!(
                config
                    .get("server")
                    .and_then(|v| v.get("gateway"))
                    .and_then(|v| v.get("enabled"))
                    .and_then(toml::Value::as_bool)
                    == Some(false),
                "snapshot gateway must remain disabled"
            );
        }
    }
    Ok(())
}

fn validate_selectors(captured: &CapturedEnvironment, descriptor: &Descriptor) -> Result<()> {
    for (key, value) in &captured.0 {
        let key = key.to_string_lossy();
        let relative = match key.as_ref() {
            "BMUX_CONFIG_DIR" => "config/bmux",
            "BMUX_RUNTIME_DIR" => "runtime",
            "BMUX_DATA_DIR" => "data/bmux",
            "BMUX_STATE_DIR" => "state",
            "BMUX_LOG_DIR" => "logs",
            "BMUX_NO_BASE_CONFIG" if value == "1" => continue,
            "BMUX_RUNTIME_NAME" if value == "default" => continue,
            "BMUX_TARGET" if value == "local" => continue,
            _ if is_execution_selector(&key) || is_config_preference(&key) => {
                bail!("conflicting or unsupported sandbox selector: {key}")
            }
            _ => continue,
        };
        let selected = Path::new(value);
        ensure!(
            !selected
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir)),
            "sandbox selector contains traversal: {key}"
        );
        ensure!(
            selected.is_absolute() && selected.canonicalize()? == descriptor.root.join(relative),
            "conflicting sandbox path selector: {key}"
        );
    }
    Ok(())
}

fn isolated_environment(
    captured: &CapturedEnvironment,
    descriptor: &Path,
    root: &Path,
    env_mode: &str,
) -> CapturedEnvironment {
    let mut values = captured.0.clone();
    values.retain(|key, _| {
        let key = key.to_string_lossy();
        if matches!(env_mode, "clean" | "hermetic") {
            return matches!(
                key.as_ref(),
                "PATH" | "USER" | "SHELL" | "TERM" | "LANG" | "LC_ALL"
            );
        }
        !key.starts_with("BMUX_") && !key.starts_with("BTERM_") && !key.starts_with("XDG_")
    });
    if env_mode == "hermetic" {
        values.insert("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into());
    }
    for (key, relative) in [
        ("BMUX_CONFIG_DIR", "config/bmux"),
        ("BMUX_RUNTIME_DIR", "runtime"),
        ("BMUX_DATA_DIR", "data/bmux"),
        ("BMUX_STATE_DIR", "state"),
        ("BMUX_LOG_DIR", "logs"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "tmp"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("HOME", "home"),
        ("TMPDIR", "tmp"),
        ("TMP", "tmp"),
        ("TEMP", "tmp"),
    ] {
        values.insert(key.into(), root.join(relative).into_os_string());
    }
    values.insert(CONTEXT_ENV.into(), descriptor.as_os_str().into());
    values.insert("BMUX_NO_BASE_CONFIG".into(), "1".into());
    values.insert("BMUX_RUNTIME_NAME".into(), "default".into());
    values.insert("BMUX_TARGET".into(), "local".into());
    CapturedEnvironment(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox_meta::{SandboxManifest, SandboxManifestPaths, write_manifest};

    #[test]
    #[serial_test::serial] // Capture checks the complete environment; serialize with environment-mutating tests.
    fn explicit_slot_capture_preserves_manifest_paths_and_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let binary = root.join("selected-bmux");
        std::fs::write(&binary, b"explicit slot artifact").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut slot = toml::Table::new();
        slot.insert(
            "binary".into(),
            toml::Value::String(binary.to_string_lossy().into_owned()),
        );
        slot.insert("inherit_base".into(), toml::Value::Boolean(false));
        for field in [
            "config_dir",
            "runtime_dir",
            "data_dir",
            "state_dir",
            "log_dir",
        ] {
            let path = root.join(field);
            std::fs::create_dir(&path).unwrap();
            slot.insert(
                field.into(),
                toml::Value::String(path.to_string_lossy().into_owned()),
            );
        }
        let manifest = root.join("slots.toml");
        let table = toml::Table::from_iter([(
            "slots".into(),
            toml::Value::Table(toml::Table::from_iter([(
                "explicit-test".into(),
                toml::Value::Table(slot),
            )])),
        )]);
        std::fs::write(&manifest, toml::to_string(&table).unwrap()).unwrap();
        let context =
            capture_slot("explicit-test", &manifest, &CapturedEnvironment::capture()).unwrap();
        assert_eq!(context.name, "explicit-test");
        assert_eq!(context.manifest, manifest);
        assert_eq!(context.config_paths.config_dir, root.join("config_dir"));
        assert_eq!(context.config_paths.runtime_dir, root.join("runtime_dir"));
        assert_eq!(context.config_paths.data_dir, root.join("data_dir"));
        assert_eq!(context.config_paths.state_dir, root.join("state_dir"));
        assert_eq!(context.log_dir, root.join("log_dir"));
        assert_eq!(context.bmux_artifact.path, binary);
        assert_eq!(context.bmux_artifact.sha256, digest(&binary).unwrap());
        assert!(!context.inherit_base);
        assert!(context.base_config_path.is_none());
        assert!(capture_slot("not-declared", &manifest, &CapturedEnvironment::capture()).is_err());
        std::fs::remove_dir(root.join("runtime_dir")).unwrap();
        assert!(capture_slot("explicit-test", &manifest, &CapturedEnvironment::capture()).is_err());
    }

    #[test]
    fn slot_capture_rejects_relative_manifest_and_different_environment() {
        assert!(
            capture_slot(
                "explicit-test",
                Path::new("relative.toml"),
                &CapturedEnvironment::capture()
            )
            .is_err()
        );
        let mut different = CapturedEnvironment::capture();
        different.0.insert(
            "BTERM_SLOT_CAPTURE_TEST".into(),
            "not-the-process-value".into(),
        );
        assert!(
            capture_slot(
                "explicit-test",
                Path::new("/not-opened/slots.toml"),
                &different
            )
            .is_err()
        );
        assert!(
            capture_slot(
                "default",
                Path::new("/not-opened/slots.toml"),
                &CapturedEnvironment::capture()
            )
            .is_err()
        );
    }

    fn fixture(snapshot: bool) -> (tempfile::TempDir, PathBuf, CapturedEnvironment) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for relative in [
            "config/bmux",
            "runtime",
            "data/bmux",
            "state",
            "logs",
            "tmp",
            "home",
        ] {
            std::fs::create_dir_all(root.join(relative)).unwrap();
        }
        let binary = root.join("bmux-artifact");
        std::fs::write(&binary, b"selected bmux, not bterm").unwrap();
        if snapshot {
            std::fs::write(
                root.join("config/bmux/bmux.toml"),
                "[server.gateway]\nenabled = false\n[plugins.custom]\nartifact = '/host/plugins'\n",
            )
            .unwrap();
            std::fs::write(root.join("development.json"), serde_json::to_vec(&serde_json::json!({
                "version": 1, "binary": binary, "binary_digest": digest(&binary).unwrap(), "cwd": root,
            })).unwrap()).unwrap();
        }
        let manifest = SandboxManifest {
            id: crate::sandbox_meta::sandbox_id_from_root(&root),
            source: "sandbox-cli".into(),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
            pid: 1,
            bmux_bin: binary.to_string_lossy().into_owned(),
            command: vec![],
            env_mode: if snapshot { "inherit" } else { "clean" }.into(),
            status: "running".into(),
            exit_code: None,
            kept: true,
            paths: SandboxManifestPaths {
                root: root.to_string_lossy().into_owned(),
                logs: root.join("logs").to_string_lossy().into_owned(),
                runtime: root.join("runtime").to_string_lossy().into_owned(),
                state: root.join("state").to_string_lossy().into_owned(),
            },
        };
        write_manifest(&root, &manifest).unwrap();
        let descriptor = write_sandbox_handoff(&root).unwrap();
        let env = CapturedEnvironment(BTreeMap::from([(
            CONTEXT_ENV.into(),
            descriptor.clone().into_os_string(),
        )]));
        (temp, descriptor, env)
    }

    #[test]
    fn valid_sandbox_preserves_borrowed_lifecycle() {
        let (_temp, path, env) = fixture(false);
        let lock_path = path.parent().unwrap().join("sandbox.lock");
        std::fs::write(&lock_path, "launcher-owned lock").unwrap();
        let before = std::fs::read(path.parent().unwrap().join("sandbox.json")).unwrap();
        let context = recognize(&env, None).unwrap().unwrap();
        assert_eq!(
            context.config_paths.runtime_dir,
            context.root.join("runtime")
        );
        assert_eq!(
            context.config_paths.config_file(),
            context.root.join("config/bmux/bmux.toml")
        );
        assert_eq!(context.configuration, ConfigurationInheritance::Clean);
        assert_eq!(
            context.bmux_artifact.path,
            context.root.join("bmux-artifact")
        );
        drop(context);
        assert_eq!(
            std::fs::read(path.parent().unwrap().join("sandbox.json")).unwrap(),
            before
        );
        assert!(path.exists());
        assert_eq!(
            std::fs::read_to_string(lock_path).unwrap(),
            "launcher-owned lock"
        );
    }

    #[test]
    fn ordinary_config_preferences_are_not_execution_identities() {
        let mut env = CapturedEnvironment::default();
        for key in [
            "BMUX_CONFIG",
            "BMUX_CONFIG_DIR",
            "BMUX_BASE_CONFIG",
            "BMUX_NO_BASE_CONFIG",
        ] {
            env.0.insert(key.into(), "host-preference".into());
            assert!(recognize(&env, None).unwrap().is_none(), "{key}");
        }
        for key in [
            "BMUX_RUNTIME_DIR",
            "BMUX_DATA_DIR",
            "BMUX_STATE_DIR",
            "BMUX_LOG_DIR",
            "BMUX_SLOT_NAME",
            "BMUX_TARGET",
            "BMUX_RUNTIME_NAME",
            "BMUX_SESSION_ID",
            "BMUX_SOCKET",
        ] {
            let mut selected = env.clone();
            selected.0.insert(key.into(), "host-selector".into());
            assert!(recognize(&selected, None).is_err(), "{key}");
        }
        // Config preferences cease to be ambient authority once a sandbox is selected.
        let (_temp, _, mut selected) = fixture(false);
        selected.0.extend(env.0);
        assert!(recognize(&selected, None).is_err());
    }

    #[test]
    fn bterm_snapshot_uses_frozen_documents_and_rejects_tampering() {
        let (_temp, path, mut env) = fixture(true);
        let root = path.parent().unwrap();
        let host = tempfile::tempdir().unwrap();
        let config_dir = host.path().join("configuration");
        let state_dir = host.path().join("state");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        let primary = config_dir.join("bterm.toml");
        let base = host.path().join("base.toml");
        let overlay = host.path().join("selected.toml");
        std::fs::write(&primary, "[bterm.window]\ntitle = 'captured primary'\n").unwrap();
        std::fs::write(
            &base,
            "[profiles.work.bterm.window]\ntitle = 'base profile'\n",
        )
        .unwrap();
        std::fs::write(&overlay, "active_profile = 'work'\n").unwrap();
        let source_env = CapturedEnvironment(BTreeMap::from([
            ("BTERM_CONFIG_DIR".into(), config_dir.into_os_string()),
            ("BTERM_STATE_DIR".into(), state_dir.into_os_string()),
            ("BTERM_BASE_CONFIG".into(), base.clone().into_os_string()),
            ("BTERM_CONFIG".into(), overlay.clone().into_os_string()),
        ]));
        let snapshot_path = root.join("bterm-configuration.json");
        std::fs::write(
            &snapshot_path,
            capture_bterm_snapshot(&source_env, host.path()).unwrap(),
        )
        .unwrap();
        write_sandbox_handoff(root).unwrap();
        // Ambient BTERM_CONFIG must not be reopened in the child.
        env.0
            .insert("BTERM_CONFIG".into(), "/missing/host/config".into());
        let first = recognize(&env, None).unwrap().unwrap();
        let family = first.bterm_configuration.unwrap();
        assert_eq!(family.documents.len(), 4);
        assert_eq!(
            family.documents[1].value.as_ref().unwrap()["bterm"]["window"]["title"].as_str(),
            Some("captured primary")
        );
        assert!(!first.environment.0.contains_key(OsStr::new("BTERM_CONFIG")));
        std::fs::remove_file(primary).unwrap();
        std::fs::remove_file(base).unwrap();
        std::fs::remove_file(overlay).unwrap();
        assert_eq!(
            recognize(&env, None)
                .unwrap()
                .unwrap()
                .bterm_configuration
                .unwrap(),
            family
        );
        std::fs::write(snapshot_path, "[]").unwrap();
        assert!(recognize(&env, None).is_err());
    }

    #[test]
    fn bterm_snapshot_honors_no_base_and_required_overlay() {
        let host = tempfile::tempdir().unwrap();
        let mut env = CapturedEnvironment(BTreeMap::from([
            ("BTERM_CONFIG_DIR".into(), host.path().as_os_str().into()),
            ("BTERM_STATE_DIR".into(), host.path().as_os_str().into()),
            ("BTERM_BASE_CONFIG".into(), "/missing/base".into()),
            ("BTERM_NO_BASE_CONFIG".into(), "true".into()),
        ]));
        let bytes = capture_bterm_snapshot(&env, host.path()).unwrap();
        let documents: Vec<FrozenDocument> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(documents.len(), 2);
        env.0
            .insert("BTERM_CONFIG".into(), "missing-required.toml".into());
        assert!(capture_bterm_snapshot(&env, host.path()).is_err());
    }

    #[test]
    fn ordinary_bmux_metadata_is_not_execution_authority() {
        let mut env = CapturedEnvironment::default();
        for (key, value) in [
            ("BMUX_PANE", "pane-hint"),
            ("BMUX_LOG_LEVEL", "debug"),
            ("BMUX_IMAGE_PROTOCOL", "kitty"),
            ("BMUX_NIX_TOOLCHAIN", "1"),
            ("BMUX_RUNTIME_NAME", "default"),
            ("BMUX_FUTURE_TERMINAL_HINT", "hint"),
        ] {
            env.0.insert(key.into(), value.into());
            assert!(recognize(&env, None).unwrap().is_none(), "{key}");
        }
        let (_temp, path, mut sandbox) = fixture(false);
        sandbox.0.extend(env.0.clone());
        assert!(recognize(&sandbox, Some(&path)).unwrap().is_some());
        for key in [
            "BMUX_SLOT_NAME",
            "BMUX_SANDBOX_ROOT",
            "BMUX_RUNTIME_DIR",
            "BMUX_DATA_DIR",
            "BMUX_STATE_DIR",
            "BMUX_LOG_DIR",
            "BMUX_TARGET",
            "BMUX_SOCKET",
            "BMUX_CONTROL_PLANE_URL",
            "BMUX_SESSION_ID",
            "BMUX_RUNTIME_NAME",
        ] {
            let mut selected = env.clone();
            selected
                .0
                .insert(key.into(), "unsupported-selection".into());
            assert!(recognize(&selected, None).is_err(), "{key}");
            let mut selected = sandbox.clone();
            selected
                .0
                .insert(key.into(), "unsupported-selection".into());
            assert!(recognize(&selected, Some(&path)).is_err(), "{key}");
        }
    }

    #[test]
    fn partial_slot_and_conflicting_selectors_fail_closed() {
        assert!(
            recognize(&CapturedEnvironment::default(), None)
                .unwrap()
                .is_none()
        );
        for key in ["BMUX_SLOT_NAME", "BMUX_RUNTIME_DIR", "BMUX_SOCKET"] {
            let env = CapturedEnvironment(BTreeMap::from([(key.into(), "host".into())]));
            assert!(recognize(&env, None).is_err());
        }
        let (_temp, path, mut env) = fixture(false);
        assert!(recognize(&env, Some(Path::new("/different/descriptor"))).is_err());
        env.0.insert("BMUX_TARGET".into(), "remote".into());
        assert!(recognize(&env, Some(&path)).is_err());
        env.0.remove(OsStr::new("BMUX_TARGET"));
        env.0
            .insert("BMUX_RUNTIME_DIR".into(), "/host/runtime".into());
        assert!(recognize(&env, None).is_err());
    }

    #[test]
    fn clean_policy_excludes_host_endpoints_state_and_configuration() {
        let (_temp, _, mut env) = fixture(false);
        for key in [
            "HOME",
            "TMPDIR",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "BTERM_ROOT",
            "BTERM_ENDPOINT",
            "HOST_SECRET",
        ] {
            env.0.insert(key.into(), "/host".into());
        }
        let context = recognize(&env, None).unwrap().unwrap();
        for key in ["HOME", "TMPDIR", "XDG_CONFIG_HOME", "XDG_STATE_HOME"] {
            assert!(Path::new(&context.environment.0[OsStr::new(key)]).starts_with(&context.root));
        }
        for key in ["BTERM_ROOT", "BTERM_ENDPOINT", "HOST_SECRET"] {
            assert!(!context.environment.0.contains_key(OsStr::new(key)));
        }
        assert_eq!(
            context.environment.0[OsStr::new("BMUX_NO_BASE_CONFIG")],
            "1"
        );
    }

    #[test]
    fn inherited_snapshot_never_reloads_host_sources() {
        let (temp, path, env) = fixture(true);
        let host_source = temp.path().join("host-source.toml");
        std::fs::write(&host_source, "first").unwrap();
        let context = recognize(&env, None).unwrap().unwrap();
        let ConfigurationInheritance::Snapshot { path: snapshot, .. } = context.configuration
        else {
            panic!("expected snapshot")
        };
        assert!(snapshot.starts_with(&context.root));
        std::fs::write(host_source, "changed host source").unwrap();
        assert!(recognize(&env, None).is_ok());
        std::fs::write(snapshot, "[server.gateway]\nenabled = true\n").unwrap();
        assert!(recognize(&env, Some(&path)).is_err());
    }

    #[test]
    fn changed_binary_and_unknown_version_are_rejected() {
        let (_temp, path, env) = fixture(false);
        let mut descriptor: Descriptor =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        descriptor.version = 2;
        std::fs::write(&path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
        assert!(recognize(&env, None).is_err());
        descriptor.version = 1;
        std::fs::write(&path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
        std::fs::write(&descriptor.bmux_artifact.path, "changed").unwrap();
        assert!(recognize(&env, None).is_err());
    }

    #[test]
    fn tampered_manifest_layout_is_rejected() {
        let (_temp, path, env) = fixture(false);
        let root = path.parent().unwrap();
        let mut manifest = crate::sandbox_meta::read_manifest(root).unwrap();
        let original_root = manifest.paths.root.clone();
        manifest.paths.root = root.join("home").to_string_lossy().into_owned();
        write_manifest(root, &manifest).unwrap();
        assert!(recognize(&env, None).is_err());
        manifest.paths.root = original_root;
        manifest.paths.runtime = root.join("state").to_string_lossy().into_owned();
        write_manifest(root, &manifest).unwrap();
        assert!(recognize(&env, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_owned_state_and_configuration_are_rejected() {
        let (temp, path, env) = fixture(false);
        let root = path.parent().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::remove_dir(root.join("state")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("state")).unwrap();
        assert!(recognize(&env, None).is_err());
        std::fs::remove_file(root.join("state")).unwrap();
        std::fs::create_dir(root.join("state")).unwrap();
        let host_config = temp.path().join("host.toml");
        std::fs::write(&host_config, "").unwrap();
        std::os::unix::fs::symlink(host_config, root.join("config/bmux/bmux.toml")).unwrap();
        assert!(recognize(&env, None).is_err());
    }
}
