//! Shared playbook file parsing and include resolution.
use super::types::{Playbook, Step};
use super::{parse_dsl, parse_toml, types};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Maximum include depth to prevent circular includes.
const MAX_INCLUDE_DEPTH: usize = 10;

/// Parse a playbook from a file path. Detects format by extension.
/// Resolves `@include` / `include = [...]` directives recursively.
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn parse_file(path: &Path) -> Result<Playbook> {
    let allowed = std::env::var_os("BMUX_PLAYBOOK_ALLOWED_ROOT").map(PathBuf::from);
    parse_file_scoped(path, allowed.as_deref())
}

/// Parse with an explicit include boundary. No environment mutation is needed
/// in a multi-owner plugin host.
/// # Errors
/// Returns an error for unreadable inputs or includes escaping the boundary.
pub fn parse_file_scoped(path: &Path, allowed_root: Option<&Path>) -> Result<Playbook> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut seen = BTreeSet::new();
    seen.insert(canonical.clone());
    parse_file_recursive(&canonical, &mut seen, 0, allowed_root)
}

/// Parse a playbook from stdin. Includes are resolved relative to CWD.
/// # Errors
///
/// Returns an error if stdin cannot be read or parsed.
pub fn parse_stdin() -> Result<Playbook> {
    let mut content = String::new();
    std::io::stdin()
        .read_to_string(&mut content)
        .context("failed reading stdin")?;

    let (mut playbook, includes) = parse_content(&content)?;

    // Resolve includes relative to CWD
    let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut seen = BTreeSet::new();
    resolve_includes(&mut playbook, &includes, &base_dir, &mut seen, 0, None)?;

    Ok(playbook)
}

// ── Include resolution ───────────────────────────────────────────────────────

fn parse_file_recursive(
    path: &Path,
    seen: &mut BTreeSet<PathBuf>,
    depth: usize,
    allowed_root: Option<&Path>,
) -> Result<Playbook> {
    if depth > MAX_INCLUDE_DEPTH {
        bail!("include depth exceeds maximum ({MAX_INCLUDE_DEPTH})");
    }

    if let Some(root) = allowed_root {
        let allowed = root
            .canonicalize()
            .context("invalid managed allowed root")?;
        let canonical = path
            .canonicalize()
            .context("managed playbook path must exist")?;
        if !canonical.starts_with(allowed) {
            bail!(
                "managed playbook/include escapes allowed root: {}",
                canonical.display()
            );
        }
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading {}", path.display()))?;

    let (mut playbook, includes) =
        parse_content(&content).with_context(|| format!("failed parsing {}", path.display()))?;

    let base_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    resolve_includes(
        &mut playbook,
        &includes,
        &base_dir,
        seen,
        depth,
        allowed_root,
    )?;

    Ok(playbook)
}

#[allow(clippy::similar_names)]
fn resolve_includes(
    playbook: &mut Playbook,
    includes: &[(usize, String)],
    base_dir: &Path,
    seen: &mut BTreeSet<PathBuf>,
    depth: usize,
    allowed_root: Option<&Path>,
) -> Result<()> {
    if includes.is_empty() {
        return Ok(());
    }

    // Resolve all includes and collect their steps with insertion positions.
    let mut insertions: Vec<(usize, Vec<Step>)> = Vec::new();

    for (insert_at, include_path) in includes {
        let resolved = base_dir.join(include_path);
        let canonical = resolved
            .canonicalize()
            .with_context(|| format!("include path not found: {}", resolved.display()))?;

        if !seen.insert(canonical.clone()) {
            bail!(
                "circular include detected: {} already included",
                canonical.display()
            );
        }

        let included = parse_file_recursive(&canonical, seen, depth + 1, allowed_root)
            .with_context(|| format!("failed parsing included file {}", canonical.display()))?;

        // Merge config from included files: parent values take priority,
        // included values fill gaps.  This lets shared setup files provide
        // defaults (e.g. @shell, @env, @var, plugin config) that the
        // including playbook can override.
        merge_included_config(&mut playbook.config, &included.config);

        insertions.push((*insert_at, included.steps));
    }

    // Insert included steps at their declared positions (in reverse order
    // to preserve indices as we insert).
    insertions.sort_by_key(|insertion| std::cmp::Reverse(insertion.0)); // reverse sort by position
    for (insert_at, steps) in insertions {
        let pos = insert_at.min(playbook.steps.len());
        for (i, step) in steps.into_iter().enumerate() {
            playbook.steps.insert(pos + i, step);
        }
    }

    // Re-index all steps sequentially.
    for (i, step) in playbook.steps.iter_mut().enumerate() {
        step.index = i;
    }

    Ok(())
}

/// Merge config from an included playbook into the parent.
///
/// Parent values always take priority.  Included values fill gaps so that
/// shared setup files can provide defaults (e.g. `@shell sh`, `@env`,
/// `@var`, plugin enable/disable) without overriding the includer's config.
///
/// Fields that are **not** merged (they remain parent-only):
/// `name`, `description`, `viewport`, `timeout`, `record`, `verbose`,
/// `render_trace`, `binary`, `bundled_plugin_ids`.
fn merge_included_config(parent: &mut types::PlaybookConfig, included: &types::PlaybookConfig) {
    // shell: fill if parent hasn't set one (first include wins).
    if parent.shell.is_none() && included.shell.is_some() {
        parent.shell.clone_from(&included.shell);
    }

    // env_mode: fill if parent hasn't set one.
    if parent.env_mode.is_none() && included.env_mode.is_some() {
        parent.env_mode = included.env_mode;
    }

    // env: insert included entries that don't already exist in parent.
    for (key, value) in &included.env {
        parent
            .env
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }

    // vars: insert included entries that don't already exist in parent.
    for (key, value) in &included.vars {
        parent
            .vars
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }

    // plugins.enable: append included entries that aren't already present.
    for id in &included.plugins.enable {
        if !parent.plugins.enable.contains(id) {
            parent.plugins.enable.push(id.clone());
        }
    }

    // plugins.disable: append included entries that aren't already present.
    for id in &included.plugins.disable {
        if !parent.plugins.disable.contains(id) {
            parent.plugins.disable.push(id.clone());
        }
    }
}

/// Parse raw content, trying TOML first then DSL.
fn parse_content(content: &str) -> Result<(Playbook, Vec<(usize, String)>)> {
    // Try TOML first
    if let Ok(result) = parse_toml::parse_toml(content) {
        return Ok(result);
    }
    // Fall back to DSL
    parse_dsl::parse_dsl(content)
}

/// Copy the parsed include closure into an owner-private source snapshot.
/// File handles are resolved beneath an opened root with no symlink traversal;
/// parsing and copying consume the same bounded bytes, not a later reopen.
/// # Errors
/// Rejects escapes, symlinks, unsupported platforms and oversized source graphs.
pub fn freeze_sources(source: &Path, allowed: &Path, destination: &Path) -> Result<PathBuf> {
    let closure = read_source_closure(source, allowed)?;
    write_source_closure(&closure, destination)
}

/// Identity of the complete bounded include closure for executable-input approval.
/// Uses the same no-follow reads, names and bytes as managed source freezing.
/// # Errors
/// Rejects unreadable, oversized, escaping or symlink-containing source graphs.
pub fn source_closure_sha256(source: &Path, allowed: &Path) -> Result<String> {
    Ok(closure_sha256(&read_source_closure(source, allowed)?))
}

/// Freeze only the exact closure approved by the caller. Hash verification and
/// copying consume the same in-memory bytes; no source is reopened after checking.
/// # Errors
/// Rejects changed source closures and all invalid source graphs.
pub fn freeze_sources_verified(
    source: &Path,
    allowed: &Path,
    destination: &Path,
    expected_sha256: &str,
) -> Result<PathBuf> {
    let closure = read_source_closure(source, allowed)?;
    if closure_sha256(&closure) != expected_sha256 {
        bail!("playbook include closure changed since approval");
    }
    write_source_closure(&closure, destination)
}

struct SourceClosure {
    entry: PathBuf,
    files: std::collections::BTreeMap<PathBuf, Vec<u8>>,
}

fn read_source_closure(source: &Path, allowed: &Path) -> Result<SourceClosure> {
    let relative = normalize_relative(
        source
            .strip_prefix(allowed)
            .context("source escapes allowed root")?,
    )?;
    let root = std::fs::File::open(allowed)?;
    let mut closure = SourceClosure {
        entry: relative.clone(),
        files: std::collections::BTreeMap::new(),
    };
    let mut budget = 4 * 1024 * 1024;
    read_source_one(&root, &relative, &mut closure, &mut budget, 0)?;
    Ok(closure)
}

fn closure_sha256(closure: &SourceClosure) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut hash = Sha256::new();
    hash.update(b"bmux-playbook-closure-v1\0");
    let entry = closure.entry.as_os_str().as_encoded_bytes();
    hash.update((entry.len() as u64).to_le_bytes());
    hash.update(entry);
    for (name, bytes) in &closure.files {
        let name = name.as_os_str().as_encoded_bytes();
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    let mut result = String::with_capacity(64);
    for byte in hash.finalize() {
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn write_source_closure(closure: &SourceClosure, destination: &Path) -> Result<PathBuf> {
    std::fs::create_dir(destination)?;
    for (relative, bytes) in &closure.files {
        let target = destination.join(relative);
        std::fs::create_dir_all(target.parent().context("source parent missing")?)?;
        std::fs::write(target, bytes)?;
    }
    Ok(destination.join(&closure.entry))
}

fn read_source_one(
    root: &std::fs::File,
    relative: &Path,
    closure: &mut SourceClosure,
    budget: &mut usize,
    depth: usize,
) -> Result<()> {
    if depth > MAX_INCLUDE_DEPTH || closure.files.len() >= 128 {
        bail!("managed source closure exceeds depth/file limit");
    }
    let relative = normalize_relative(relative)?;
    if closure.files.contains_key(&relative) {
        bail!("duplicate or circular managed include");
    }
    let file = open_beneath(root, &relative)?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 || bytes.len() > *budget {
        bail!("managed source closure exceeds byte limit");
    }
    *budget -= bytes.len();
    let content = std::str::from_utf8(&bytes)?;
    let (_, includes) = parse_content(content)?;
    closure.files.insert(relative.clone(), bytes);
    for (_, include) in includes {
        let include = relative
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(include);
        read_source_one(root, &include, closure, budget, depth + 1)?;
    }
    Ok(())
}

fn normalize_relative(path: &Path) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => result.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if result.pop() => {}
            _ => bail!("managed source path escapes allowed root"),
        }
    }
    if result.as_os_str().is_empty() {
        bail!("empty managed source path");
    }
    Ok(result)
}

#[cfg(unix)]
fn open_beneath(root: &std::fs::File, relative: &Path) -> Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let mut directory = root.try_clone()?;
    let parts = relative.components().collect::<Vec<_>>();
    for (position, part) in parts.iter().enumerate() {
        let name = std::ffi::CString::new(part.as_os_str().as_bytes())?;
        let is_directory = position + 1 < parts.len();
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if is_directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: directory is an owned live descriptor; name is NUL terminated.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful openat returns a fresh owned descriptor.
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    if !directory.metadata()?.is_file() {
        bail!("managed source is not a regular file");
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn open_beneath(_root: &std::fs::File, _relative: &Path) -> Result<std::fs::File> {
    bail!("managed source snapshots require no-follow directory-relative opens on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{PlaybookConfig, PluginConfig, SandboxEnvMode};

    fn default_config() -> PlaybookConfig {
        PlaybookConfig::default()
    }

    #[test]
    fn approved_closure_rejects_changed_include_and_keeps_verified_bytes() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main.dsl");
        let child = root.path().join("child.dsl");
        std::fs::write(&main, "@include child.dsl\n").unwrap();
        std::fs::write(&child, "new-session name='approved'\n").unwrap();
        let approved = source_closure_sha256(&main, root.path()).unwrap();
        let frozen_root = root.path().join("frozen");
        let frozen = freeze_sources_verified(&main, root.path(), &frozen_root, &approved).unwrap();
        assert_eq!(
            source_closure_sha256(&frozen, &frozen_root).unwrap(),
            approved
        );
        std::fs::write(&child, "new-session name='changed'\n").unwrap();
        assert_ne!(source_closure_sha256(&main, root.path()).unwrap(), approved);
        assert!(
            freeze_sources_verified(&main, root.path(), &root.path().join("rejected"), &approved)
                .is_err()
        );
        assert!(!root.path().join("rejected").exists());
        assert_eq!(
            source_closure_sha256(&frozen, &frozen_root).unwrap(),
            approved
        );
    }

    #[test]
    fn source_snapshot_pins_includes_and_rejects_escape() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("deck");
        std::fs::create_dir(&source).unwrap();
        let main = source.join("main.playbook");
        std::fs::write(&main, "@include child.playbook\n").unwrap();
        std::fs::write(source.join("child.playbook"), "new-session name='first'\n").unwrap();
        let frozen = freeze_sources(&main, &source, &root.path().join("frozen")).unwrap();
        std::fs::write(
            source.join("child.playbook"),
            "new-session name='changed'\n",
        )
        .unwrap();
        let parsed = parse_file(&frozen).unwrap();
        assert!(parsed.steps[0].to_dsl().contains("first"));
        assert!(normalize_relative(Path::new("../outside")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn source_snapshot_rejects_symlink_include() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("deck");
        std::fs::create_dir(&source).unwrap();
        let main = source.join("main.playbook");
        std::fs::write(&main, "@include child.playbook\n").unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, "new-session name='outside'\n").unwrap();
        std::os::unix::fs::symlink(outside, source.join("child.playbook")).unwrap();
        assert!(freeze_sources(&main, &source, &root.path().join("frozen")).is_err());
    }

    // ── shell merging ──────────────────────────────────────────────────

    #[test]
    fn include_fills_shell_when_parent_unset() {
        let mut parent = default_config();
        let included = PlaybookConfig {
            shell: Some("sh".to_string()),
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.shell.as_deref(), Some("sh"));
    }

    #[test]
    fn parent_shell_wins_over_included() {
        let mut parent = PlaybookConfig {
            shell: Some("bash".to_string()),
            ..default_config()
        };
        let included = PlaybookConfig {
            shell: Some("sh".to_string()),
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.shell.as_deref(), Some("bash"));
    }

    #[test]
    fn first_include_wins_for_shell() {
        let mut parent = default_config();
        let first = PlaybookConfig {
            shell: Some("sh".to_string()),
            ..default_config()
        };
        let second = PlaybookConfig {
            shell: Some("bash".to_string()),
            ..default_config()
        };
        merge_included_config(&mut parent, &first);
        merge_included_config(&mut parent, &second);
        assert_eq!(parent.shell.as_deref(), Some("sh"));
    }

    // ── env_mode merging ───────────────────────────────────────────────

    #[test]
    fn include_fills_env_mode_when_parent_unset() {
        let mut parent = default_config();
        let included = PlaybookConfig {
            env_mode: Some(SandboxEnvMode::Clean),
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.env_mode, Some(SandboxEnvMode::Clean));
    }

    #[test]
    fn parent_env_mode_wins_over_included() {
        let mut parent = PlaybookConfig {
            env_mode: Some(SandboxEnvMode::Inherit),
            ..default_config()
        };
        let included = PlaybookConfig {
            env_mode: Some(SandboxEnvMode::Clean),
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.env_mode, Some(SandboxEnvMode::Inherit));
    }

    // ── env merging ────────────────────────────────────────────────────

    #[test]
    fn include_fills_env_gaps() {
        let mut parent = default_config();
        parent.env.insert("A".into(), "from_parent".into());
        let mut included = default_config();
        included.env.insert("A".into(), "from_include".into());
        included.env.insert("B".into(), "from_include".into());
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.env["A"], "from_parent", "parent value preserved");
        assert_eq!(parent.env["B"], "from_include", "gap filled from include");
    }

    // ── vars merging ───────────────────────────────────────────────────

    #[test]
    fn include_fills_vars_gaps() {
        let mut parent = default_config();
        parent.vars.insert("X".into(), "parent_x".into());
        let mut included = default_config();
        included.vars.insert("X".into(), "include_x".into());
        included.vars.insert("Y".into(), "include_y".into());
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.vars["X"], "parent_x", "parent var preserved");
        assert_eq!(parent.vars["Y"], "include_y", "gap filled from include");
    }

    // ── plugin merging ─────────────────────────────────────────────────

    #[test]
    fn include_appends_plugin_enable() {
        let mut parent = PlaybookConfig {
            plugins: PluginConfig {
                enable: vec!["a".into()],
                disable: vec![],
            },
            ..default_config()
        };
        let included = PlaybookConfig {
            plugins: PluginConfig {
                enable: vec!["a".into(), "b".into()],
                disable: vec!["c".into()],
            },
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.plugins.enable, vec!["a", "b"], "dedup + append");
        assert_eq!(parent.plugins.disable, vec!["c"], "disable appended");
    }

    // ── identity fields not merged ─────────────────────────────────────

    #[test]
    fn include_does_not_merge_identity_fields() {
        let mut parent = default_config();
        let included = PlaybookConfig {
            name: Some("included-name".into()),
            description: Some("included-desc".into()),
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert!(parent.name.is_none(), "name should not be merged");
        assert!(
            parent.description.is_none(),
            "description should not be merged"
        );
    }

    // ── end-to-end parse_file include config merging ───────────────────

    #[test]
    fn parse_file_merges_shell_from_include() {
        let fixtures = tempfile::tempdir().expect("create private playbook fixtures");
        std::fs::write(
            fixtures.path().join("include_setup.dsl"),
            "@shell sh\nnew-session name='included'\nsleep ms=1\n",
        )
        .expect("write included setup");
        std::fs::write(
            fixtures.path().join("include_main.dsl"),
            "@include include_setup.dsl\nsleep ms=2\nsleep ms=3\n",
        )
        .expect("write including playbook");
        let playbook = parse_file(&fixtures.path().join("include_main.dsl"))
            .expect("should parse include_main.dsl");
        assert_eq!(
            playbook.config.shell.as_deref(),
            Some("sh"),
            "shell should be merged from the include when the parent has no override"
        );
        assert!(
            playbook.steps.len() >= 4,
            "should have steps from both files"
        );
    }
}
