//! Explicit, captured configuration inputs for applications embedding BMUX.
//!
//! Resolution never discovers files or reads process configuration overrides.
//! Capture and resolution are separate so callers can retain exactly the TOML
//! documents used for a startup snapshot or a deterministic fingerprint.

use super::{
    BmuxConfig, CompositionResolution, ConfigError, ConfigLoadOverrides, ConfigPaths,
    ConfigScopeTarget, Result, ScopedConfigLoadRequest, apply_forced_profile, base_config_disabled,
    discover_local_config_paths, merge_toml_value, reject_removed_presentation_settings,
    resolve_composed_config_value_with_context, resolve_config_override_path,
    scoped_local_overlay_for_target,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[cfg(test)]
mod context_tests;
#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod tests;

/// Captured evaluation inputs. Missing keys never fall back to process state.
///
/// `HOME` (or `USERPROFILE` when HOME is absent) supplies tilde expansion.
/// Platform OS matching is a compile-time property, not an environment input.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ConfigEvaluationContext {
    pub cwd: Option<PathBuf>,
    pub environment: BTreeMap<String, std::ffi::OsString>,
}

impl ConfigEvaluationContext {
    /// Capture ambient evaluation inputs for compatibility with ordinary loading.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            cwd: std::env::current_dir().ok(),
            environment: std::env::vars_os()
                .filter_map(|(key, value)| key.into_string().ok().map(|key| (key, value)))
                .collect(),
        }
    }

    pub(crate) fn variable(&self, name: &str) -> Option<&str> {
        self.environment.get(name).and_then(|value| value.to_str())
    }

    fn interpolate_path(&self, value: &std::path::Path) -> (PathBuf, bool) {
        let input = value.to_string_lossy();
        let home = self
            .environment
            .get("HOME")
            .or_else(|| self.environment.get("USERPROFILE"));
        let (expanded, missing_home) =
            BmuxConfig::expand_home_prefix_with_home(&input, home.map(PathBuf::from));
        let (expanded, missing_vars) =
            BmuxConfig::expand_env_tokens_with_lookup(&expanded, |name| {
                self.environment.get(name).cloned()
            });
        (
            PathBuf::from(expanded),
            missing_home || !missing_vars.is_empty(),
        )
    }
}

/// A file selected explicitly by the caller, in increasing precedence order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    pub path: PathBuf,
    pub required: bool,
}

/// A captured TOML document. Keep its origin even when copying a startup snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDocument {
    pub source: ConfigSource,
    /// `None` records a missing optional source (and therefore its identity).
    pub value: Option<toml::Value>,
    /// Local sources use the existing scoped-local policy; ordinary sources do not.
    pub local: bool,
}

/// A source family, before profile resolution or default insertion.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigFamily {
    pub documents: Vec<ConfigDocument>,
}

impl ConfigFamily {
    /// Read only the supplied paths. Relative paths are captured as absolute paths.
    ///
    /// # Errors
    /// Fails for missing required files, unreadable files, or malformed TOML.
    pub fn capture(sources: &[ConfigSource]) -> Result<Self> {
        let mut documents = Vec::with_capacity(sources.len());
        for source in sources {
            let source = ConfigSource {
                path: resolve_config_override_path(source.path.as_os_str()),
                required: source.required,
            };
            let value = match std::fs::read_to_string(&source.path) {
                Ok(contents) => {
                    Some(
                        toml::from_str(&contents).map_err(|_| ConfigError::ParseError {
                            error: format!(
                                "invalid TOML in {} (source values redacted)",
                                source.path.display()
                            ),
                        })?,
                    )
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if source.required {
                        return Err(ConfigError::FileNotFound { path: source.path });
                    }
                    None
                }
                Err(error) => {
                    return Err(ConfigError::ReadError {
                        error: format!("{error} ({})", source.path.display()),
                    });
                }
            };
            documents.push(ConfigDocument {
                source,
                value,
                local: false,
            });
        }
        Ok(Self { documents })
    }
}

/// Deliberately capture host BMUX discovery separately from explicit resolution.
///
/// Pass `ConfigPaths::default()` and `ConfigLoadOverrides::for_process()` only
/// when host discovery is intended. Base/primary/env/CLI and local ordering match
/// ordinary BMUX loading; overrides are explicit even for a default path.
///
/// # Errors
/// Fails if a selected source cannot be read or parsed.
pub fn capture_host_configuration(
    paths: &ConfigPaths,
    overrides: &ConfigLoadOverrides,
    scope: Option<&ScopedConfigLoadRequest>,
) -> Result<ConfigFamily> {
    let primary = paths.config_file();
    let mut sources = Vec::new();
    if !base_config_disabled()
        && let Some(path) = &overrides.base_config_path
    {
        sources.push(ConfigSource {
            path: path.clone(),
            required: false,
        });
    }
    sources.push(ConfigSource {
        path: primary.clone(),
        required: false,
    });
    sources.push(ConfigSource {
        path: paths.state_config_file(),
        required: false,
    });
    for path in [&overrides.env_config_path, &overrides.cli_config_path]
        .into_iter()
        .flatten()
    {
        sources.push(ConfigSource {
            path: path.clone(),
            required: true,
        });
    }
    let mut family = ConfigFamily::capture(&sources)?;
    if let Some(request) = scope
        && let Some(cwd) = request.target.cwd_path()
    {
        let local_sources =
            discover_local_config_paths(&cwd, &request.local_config_filename, &primary)
                .into_iter()
                .map(|path| ConfigSource {
                    path,
                    required: true,
                })
                .collect::<Vec<_>>();
        let mut local = ConfigFamily::capture(&local_sources)?;
        for document in &mut local.documents {
            document.local = true;
        }
        family.documents.extend(local.documents);
    }
    Ok(family)
}

/// Sparse resolution and origin metadata. Paths are key segments, not dotted
/// strings, so application keys containing dots remain unambiguous.
#[derive(Debug, Clone)]
pub struct ConfigFamilyResolution {
    pub resolution: CompositionResolution,
    pub origins: BTreeMap<Vec<String>, PathBuf>,
}

/// Resolved application inputs.
///
/// `bmux_raw` is sparse and composition-free, ready
/// to serialize into a private startup snapshot. Defaults are applied only to
/// `bmux`; no application schema or defaults are imposed by this crate.
#[derive(Debug, Clone)]
pub struct ApplicationConfiguration {
    pub bmux: BmuxConfig,
    pub application: toml::Value,
    pub bmux_raw: toml::Value,
    pub application_family: ConfigFamilyResolution,
    pub bmux_family: ConfigFamilyResolution,
    /// Captured documents, including unused optional inputs, for fingerprinting.
    pub sources: [ConfigFamily; 2],
}

impl ApplicationConfiguration {
    /// Pin typed defaults and normalized known fields without losing extensions.
    ///
    /// Unlike serializing `bmux`, this retains unknown raw keys, including keys
    /// nested in known tables and table-array elements. Known fields removed by
    /// normalization are removed; normalized arrays retain their exact length.
    /// This private startup data may contain secrets and must not be logged.
    ///
    /// # Errors
    /// Returns a redacted error if the public result has been mutated into an
    /// inconsistent or unserializable BMUX configuration.
    pub fn startup_snapshot(&self) -> Result<toml::Value> {
        let error = || ConfigError::ParseError {
            error: "invalid startup snapshot (source values redacted)".to_owned(),
        };
        super::is_startup_snapshot(&self.bmux_raw)?;
        let before: BmuxConfig = self.bmux_raw.clone().try_into().map_err(|_| error())?;
        let before = toml::Value::try_from(before).map_err(|_| error())?;
        let after = toml::Value::try_from(&self.bmux).map_err(|_| error())?;
        let mut snapshot = reconcile_snapshot(self.bmux_raw.clone(), Some(&before), &after);
        snapshot.as_table_mut().ok_or_else(error)?.insert(
            super::STARTUP_SNAPSHOT_MARKER.to_owned(),
            toml::Value::Table(toml::Table::from_iter([
                ("version".to_owned(), toml::Value::Integer(1)),
                (
                    "representation".to_owned(),
                    toml::Value::String("resolved-paths".to_owned()),
                ),
            ])),
        );
        Ok(snapshot)
    }
}

// Use the typed pre-normalization shape to distinguish fields intentionally
// removed by repairs from unknown extension keys. A blanket raw/typed merge
// either resurrects repaired fields or loses unknown nested array members.
fn reconcile_snapshot(
    mut raw: toml::Value,
    before: Option<&toml::Value>,
    after: &toml::Value,
) -> toml::Value {
    match (&mut raw, before, after) {
        (toml::Value::Table(raw), before, toml::Value::Table(after)) => {
            if let Some(before) = before.and_then(toml::Value::as_table) {
                for key in before.keys() {
                    if !after.contains_key(key) {
                        raw.remove(key);
                    }
                }
            }
            for (key, value) in after {
                let previous = before.and_then(|before| before.get(key));
                let original = raw.remove(key).unwrap_or_else(|| value.clone());
                raw.insert(key.clone(), reconcile_snapshot(original, previous, value));
            }
        }
        (toml::Value::Array(raw), before, toml::Value::Array(after)) => {
            let previous = before.and_then(toml::Value::as_array);
            *raw = after
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let original = raw.get(index).cloned().unwrap_or_else(|| value.clone());
                    reconcile_snapshot(original, previous.and_then(|array| array.get(index)), value)
                })
                .collect();
        }
        (raw, _, after) => *raw = after.clone(),
    }
    raw
}

fn empty_table() -> toml::Value {
    toml::Value::Table(toml::Table::new())
}

// Retain composition control data but replace ordinary leaves with origin IDs.
// The same merge/profile resolver then computes origin precedence, including
// inheritance, explicit empty arrays, and arbitrary application extension keys.
fn mark_origins(value: &mut toml::Value, marker: &str) {
    if let Some(table) = value.as_table_mut() {
        for (_, child) in table.iter_mut() {
            mark_origins(child, marker);
        }
    } else {
        *value = toml::Value::String(marker.to_owned());
    }
}

fn origin_document(value: &toml::Value, marker: &str) -> toml::Value {
    let mut value = value.clone();
    if let Some(root) = value.as_table_mut() {
        for (key, child) in root {
            if key == super::STARTUP_SNAPSHOT_MARKER {
                continue;
            }
            if key == "composition" {
                if let Some(profiles) = child
                    .get_mut("profiles")
                    .and_then(toml::Value::as_table_mut)
                {
                    for (_, profile) in profiles.iter_mut() {
                        if let Some(patch) = profile.get_mut("patch") {
                            mark_origins(patch, marker);
                        }
                    }
                }
            } else {
                mark_origins(child, marker);
            }
        }
    }
    value
}

fn collect_origins(
    value: &toml::Value,
    path: &mut Vec<String>,
    documents: &[ConfigDocument],
    origins: &mut BTreeMap<Vec<String>, PathBuf>,
) {
    if let Some(table) = value.as_table() {
        for (key, child) in table {
            path.push(key.clone());
            collect_origins(child, path, documents, origins);
            path.pop();
        }
    } else if let Some(index) = value
        .as_str()
        .and_then(|value| value.strip_prefix("source:"))
        .and_then(|value| value.parse::<usize>().ok())
        && let Some(document) = documents.get(index)
    {
        origins.insert(path.clone(), document.source.path.clone());
    }
}

fn resolve_family(
    family: &ConfigFamily,
    forced_profile: Option<&str>,
    target: Option<&ConfigScopeTarget>,
    context: &ConfigEvaluationContext,
) -> Result<(toml::Value, ConfigFamilyResolution)> {
    let mut merged = empty_table();
    let mut marked = empty_table();
    for (index, document) in family.documents.iter().enumerate() {
        let Some(mut value) = document.value.clone() else {
            if document.source.required {
                return Err(ConfigError::FileNotFound {
                    path: document.source.path.clone(),
                });
            }
            continue;
        };
        if document.local {
            let target = target.ok_or_else(|| ConfigError::ParseError {
                error: "local configuration document requires a scope target".to_owned(),
            })?;
            let Some(overlay) = scoped_local_overlay_for_target(value, target)? else {
                continue;
            };
            value = overlay;
        }
        if !value.is_table() {
            return Err(ConfigError::ParseError {
                error: "config root must be a table".to_owned(),
            });
        }
        merge_toml_value(
            &mut marked,
            origin_document(&value, &format!("source:{index}")),
        );
        merge_toml_value(&mut merged, value);
    }
    apply_forced_profile(&mut merged, forced_profile)?;
    apply_forced_profile(&mut marked, forced_profile)?;
    let (resolved, resolution, _) =
        resolve_composed_config_value_with_context(&merged, empty_table(), context)?;
    // Pin auto-selection to the actual resolution for the parallel origin pass.
    apply_forced_profile(&mut marked, resolution.selected_profile.as_deref())?;
    let (marked, _, _) =
        resolve_composed_config_value_with_context(&marked, empty_table(), context)?;
    let mut origins = BTreeMap::new();
    collect_origins(&marked, &mut Vec::new(), &family.documents, &mut origins);
    Ok((
        resolved,
        ConfigFamilyResolution {
            resolution,
            origins,
        },
    ))
}

fn resolve_origin_paths(
    value: &mut toml::Value,
    metadata: &ConfigFamilyResolution,
    context: &ConfigEvaluationContext,
) {
    fn visit(
        value: &mut toml::Value,
        path: &mut Vec<String>,
        metadata: &ConfigFamilyResolution,
        context: &ConfigEvaluationContext,
    ) {
        if let Some(table) = value.as_table_mut() {
            for (key, child) in table {
                path.push(key.clone());
                visit(child, path, metadata, context);
                path.pop();
            }
            return;
        }
        let segments = path.iter().map(String::as_str).collect::<Vec<_>>();
        let is_path = matches!(
            segments.as_slice(),
            ["plugins", "search_paths"]
                | ["presenter", "web", "assets"]
                | ["recording", "dir" | "auto_export_dir"]
                | [
                    "connections",
                    "targets",
                    _,
                    "identity_file" | "known_hosts_file" | "ca_file"
                ]
                | ["kiosk", "files", "sshd_include_path" | "wrapper_dir"]
                | ["server", "gateway", "cert_file" | "key_file"]
        );
        if !is_path {
            return;
        }
        let parent = metadata
            .origins
            .get(path)
            .and_then(|origin| origin.parent());
        let resolve = |value: &mut toml::Value| {
            if let Some(text) = value.as_str() {
                if text.is_empty() {
                    return;
                }
                let (mut expanded, unresolved) =
                    context.interpolate_path(std::path::Path::new(text));
                if !unresolved
                    && expanded.is_relative()
                    && let Some(parent) = parent
                {
                    expanded = parent.join(expanded);
                }
                *value = toml::Value::String(expanded.to_string_lossy().into_owned());
            }
        };
        if let Some(array) = value.as_array_mut() {
            for child in array {
                resolve(child);
            }
        } else {
            resolve(value);
        }
    }
    // Marked snapshot paths must not be interpolated again by explicit consumers.
    // The shared composition pass already validated the marker.
    if value.get(super::STARTUP_SNAPSHOT_MARKER).is_some() {
        return;
    }
    visit(value, &mut Vec::new(), metadata, context);
}

/// Resolve the Bterm superset first, then overlay independently resolved BMUX
/// fields, including its profile effects. `[bterm]` from BMUX inputs is ignored.
///
/// No ambient config discovery or process overrides occur here. Profile auto
/// selection and BMUX path-token interpolation retain their existing semantics.
/// `forced_profile` applies independently to both families when supplied.
///
/// Application path interpretation belongs to the consumer: use the winning
/// `application_family.origins` entry (prefixed with `bterm`) to resolve relative
/// paths against their original source, never against the startup snapshot.
///
/// # Errors
/// Fails for invalid composition, malformed BMUX overlays, or invalid final BMUX
/// configuration. Sparse application values are intentionally not schema-checked.
pub fn resolve_application_configuration(
    bterm_sources: &ConfigFamily,
    bmux_sources: &ConfigFamily,
    forced_profile: Option<&str>,
    target: Option<&ConfigScopeTarget>,
) -> Result<ApplicationConfiguration> {
    resolve_application_configuration_with_context(
        bterm_sources,
        bmux_sources,
        forced_profile,
        target,
        &ConfigEvaluationContext::capture(),
    )
}

/// Resolve captured documents using only the supplied evaluation context.
///
/// No environment or current-directory reads occur, including during profile
/// selection, path expansion, validation or origin attribution. Source documents
/// (including profile definitions and unknown extensions) remain in `sources`;
/// `bmux_raw` is the sparse resolved projection and does not retain composition.
/// Errors redact source values; captured documents must remain private.
///
/// # Errors
/// Returns a redacted error for invalid composition or BMUX configuration.
pub fn resolve_application_configuration_with_context(
    bterm_sources: &ConfigFamily,
    bmux_sources: &ConfigFamily,
    forced_profile: Option<&str>,
    target: Option<&ConfigScopeTarget>,
    context: &ConfigEvaluationContext,
) -> Result<ApplicationConfiguration> {
    resolve_application_configuration_inner(
        bterm_sources,
        bmux_sources,
        forced_profile,
        target,
        context,
    )
    .map_err(|_| ConfigError::ParseError {
        error: "invalid explicit configuration (source values redacted)".to_owned(),
    })
}

fn resolve_application_configuration_inner(
    bterm_sources: &ConfigFamily,
    bmux_sources: &ConfigFamily,
    forced_profile: Option<&str>,
    target: Option<&ConfigScopeTarget>,
    context: &ConfigEvaluationContext,
) -> Result<ApplicationConfiguration> {
    let (mut raw, application_family) =
        resolve_family(bterm_sources, forced_profile, target, context)?;
    let (mut overlay, bmux_family) = resolve_family(bmux_sources, forced_profile, target, context)?;
    let application = raw
        .as_table_mut()
        .and_then(|root| root.remove("bterm"))
        .unwrap_or_else(empty_table);
    if let Some(root) = overlay.as_table_mut() {
        root.remove("bterm");
    }
    reject_removed_presentation_settings(&raw, "")?;
    reject_removed_presentation_settings(&overlay, "")?;
    resolve_origin_paths(&mut raw, &application_family, context);
    resolve_origin_paths(&mut overlay, &bmux_family, context);
    merge_toml_value(&mut raw, overlay);
    let mut defaults =
        toml::Value::try_from(BmuxConfig::default()).map_err(|error| ConfigError::ParseError {
            error: error.to_string(),
        })?;
    merge_toml_value(&mut defaults, raw.clone());
    let mut bmux: BmuxConfig =
        defaults
            .try_into()
            .map_err(|error: toml::de::Error| ConfigError::ParseError {
                error: error.to_string(),
            })?;
    // Paths have already been interpolated exactly once with the captured context.
    // Avoid legacy interpolation and its ambient reads/value-bearing diagnostics.
    bmux.sanitize_invalid_values();
    bmux.validate()?;
    Ok(ApplicationConfiguration {
        bmux,
        application,
        bmux_raw: raw,
        application_family,
        bmux_family,
        sources: [bterm_sources.clone(), bmux_sources.clone()],
    })
}
