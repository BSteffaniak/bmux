//! Shared implementation for the `bmux-env` binary and the `bmux env` / `bmux slot`
//! subcommand trees exposed by `bmux_cli`.
//!
//! This crate's binary (`bmux-env`) and every entry point under `bmux slot` /
//! `bmux env` delegate into the same functions declared here, so the
//! command surface is identical regardless of which front-end you invoke.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use bmux_slots::{
    NewSlotBlock, SLOT_NAME_ENV, SLOTS_BIN_DIR_ENV, Slot, SlotManifest, default_bin_dir,
    default_manifest_path, is_read_only_manifest, remove_slot_block, render_slot_block_toml,
    validate_slot_name, write_slot_block,
};

/// Shell flavors supported by [`cmd_shell`].
#[derive(Copy, Clone, Debug)]
pub enum ShellKind {
    Auto,
    Bash,
    Zsh,
    Fish,
    Nushell,
    Powershell,
    Posix,
}

/// Output format for [`cmd_print`].
#[derive(Copy, Clone, Debug)]
pub enum PrintFormat {
    Shell,
    Json,
    Nix,
    Fish,
}

/// Output format for structured slot subcommands (list/show/install).
#[derive(Copy, Clone, Debug)]
pub enum SlotOutputFormat {
    Toml,
    Json,
    Nix,
}

/// How to place the per-slot binary when installing.
#[derive(Copy, Clone, Debug, Default)]
pub enum InstallMode {
    /// Create a symlink at `<bin_dir>/bmux-<name>` pointing at the source binary.
    #[default]
    Symlink,
    /// Copy the source binary to `<bin_dir>/bmux-<name>`.
    Copy,
}

/// Parameters for [`cmd_install`].
#[derive(Debug, Clone)]
pub struct InstallParams {
    /// New slot name.
    pub name: String,
    /// Path to the source `bmux` binary. Relative paths are resolved against
    /// the current working directory before being written to the manifest.
    pub binary: PathBuf,
    /// Whether the slot should inherit the shared base config.
    pub inherit_base: bool,
    /// Symlink or copy at the destination.
    pub mode: InstallMode,
    /// Destination directory; `None` means use `default_bin_dir()`.
    pub bin_dir: Option<PathBuf>,
    /// Structured output format used when printing the would-be block
    /// (read-only manifest or `--dry-run`).
    pub format: SlotOutputFormat,
    /// When true, never touch disk; just print what would happen.
    pub dry_run: bool,
    /// Allow replacing an existing slot with the same name. Without this,
    /// duplicates are refused (after an interactive confirmation prompt if
    /// a TTY is attached).
    pub overwrite: bool,
    /// Skip interactive confirmation prompts. When a slot with the same
    /// name already exists, `overwrite` must also be set to actually
    /// replace it.
    pub yes: bool,
}

// ---------------------------------------------------------------------------
// shell / exec / print
// ---------------------------------------------------------------------------

/// Resolve a [`ShellKind`] (possibly `Auto`) to a concrete dialect.
#[must_use]
pub fn resolve_shell(kind: ShellKind) -> ShellKind {
    if !matches!(kind, ShellKind::Auto) {
        return kind;
    }
    let raw = std::env::var("SHELL").unwrap_or_default();
    let basename = std::path::Path::new(&raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match basename {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        "fish" => ShellKind::Fish,
        "nu" | "nushell" => ShellKind::Nushell,
        "pwsh" | "powershell" => ShellKind::Powershell,
        _ => ShellKind::Posix,
    }
}

/// Print shell code that idempotently prepends `$BMUX_SLOTS_BIN_DIR` to `PATH`.
///
/// # Errors
///
/// Returns errors for I/O failures on the output stream.
pub fn cmd_shell<W: Write>(w: &mut W, shell: ShellKind) -> Result<()> {
    let resolved = resolve_shell(shell);
    let bin_dir = default_bin_dir();
    let bin_str = bin_dir.to_string_lossy();
    let out = match resolved {
        ShellKind::Bash | ShellKind::Zsh => bash_zsh(&bin_str),
        ShellKind::Fish => fish(&bin_str),
        ShellKind::Nushell => nushell(&bin_str),
        ShellKind::Powershell => powershell(&bin_str),
        ShellKind::Posix | ShellKind::Auto => posix(&bin_str),
    };
    w.write_all(out.as_bytes())?;
    Ok(())
}

/// `exec` the given command with `BMUX_SLOT_NAME=<slot>` and
/// `$BMUX_SLOTS_BIN_DIR` prepended to `PATH`.
///
/// On Unix this `execvp`s (never returns on success). On other platforms it
/// spawns, waits, and exits with the child's exit code.
///
/// # Errors
///
/// - When the command vector is empty.
/// - When the slot is not declared in the manifest.
/// - When `exec`/spawn fails.
pub fn cmd_exec(slot_name: &str, argv: &[String]) -> Result<()> {
    if argv.is_empty() {
        bail!("exec: missing command");
    }
    let manifest = SlotManifest::load_default().context("load slot manifest")?;
    let _slot = manifest
        .get(slot_name)
        .map_err(|e| anyhow!("{e}"))
        .context("resolve slot")?;

    // Safety: CLI-style env mutation before exec/spawn; no threads yet.
    unsafe { std::env::set_var(SLOT_NAME_ENV, slot_name) };
    let bin_dir = default_bin_dir();
    prepend_path_env(&bin_dir)?;

    exec_replace(argv)
}

#[cfg(unix)]
fn exec_replace(argv: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    Err(anyhow!("exec {:?} failed: {err}", argv[0]))
}

#[cfg(not(unix))]
fn exec_replace(argv: &[String]) -> Result<()> {
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("spawn {:?}", argv[0]))?;
    let code = status.code().unwrap_or(1);
    std::process::exit(code);
}

fn prepend_path_env(dir: &Path) -> Result<()> {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut parts: Vec<PathBuf> = std::env::split_paths(&current).collect();
    if parts.iter().any(|p| p == dir) {
        return Ok(());
    }
    parts.insert(0, dir.to_path_buf());
    let joined = std::env::join_paths(parts).context("join PATH")?;
    unsafe { std::env::set_var("PATH", &joined) };
    Ok(())
}

/// Print the resolved env-var set as structured data.
///
/// # Errors
///
/// Returns errors for I/O failures on the output stream.
pub fn cmd_print<W: Write>(w: &mut W, format: PrintFormat) -> Result<()> {
    let env = resolve_env_map();
    match format {
        PrintFormat::Shell => {
            for (k, v) in &env {
                writeln!(w, "{k}={}", shell_single_quote(v))?;
            }
        }
        PrintFormat::Json => {
            let map: serde_json::Map<_, _> = env
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            serde_json::to_writer_pretty(&mut *w, &map)?;
            writeln!(w)?;
        }
        PrintFormat::Nix => {
            writeln!(w, "{{")?;
            for (k, v) in &env {
                writeln!(w, "  {k} = {};", nix_string(v))?;
            }
            writeln!(w, "}}")?;
        }
        PrintFormat::Fish => {
            for (k, v) in &env {
                writeln!(w, "set -gx {k} {}", shell_single_quote(v))?;
            }
        }
    }
    Ok(())
}

fn resolve_env_map() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let bin_dir = default_bin_dir();
    out.insert(
        SLOTS_BIN_DIR_ENV.to_string(),
        bin_dir.to_string_lossy().into_owned(),
    );
    if let Ok(m) = SlotManifest::load_default() {
        if let Some(ref src) = m.source {
            out.insert(
                "BMUX_SLOTS_MANIFEST_RESOLVED".to_string(),
                src.to_string_lossy().into_owned(),
            );
        }
        if let Some(d) = m.resolved_default() {
            out.insert("BMUX_DEFAULT_SLOT".to_string(), d.name.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// slot read commands (list / show / paths / doctor)
// ---------------------------------------------------------------------------

/// List all declared slots.
///
/// # Errors
///
/// Returns errors for manifest-load or I/O failures.
pub fn cmd_slot_list<W: Write>(w: &mut W, format: SlotOutputFormat) -> Result<()> {
    let manifest = load_manifest()?;
    match format {
        SlotOutputFormat::Toml => emit_toml_list(w, &manifest)?,
        SlotOutputFormat::Json => emit_json_list(w, &manifest)?,
        SlotOutputFormat::Nix => emit_nix_list(w, &manifest)?,
    }
    Ok(())
}

/// Show a single slot's detail.
///
/// `default_name` is consulted when `name` is `None` (typically passed as the
/// active slot name by bmux_cli, or `None` by the standalone bmux-env binary).
///
/// # Errors
///
/// Returns errors for manifest-load or resolution failures.
pub fn cmd_slot_show<W: Write>(
    w: &mut W,
    name: Option<&str>,
    default_name: Option<&str>,
    format: SlotOutputFormat,
) -> Result<()> {
    let manifest = load_manifest()?;
    let slot = resolve_slot_name(&manifest, name, default_name)?;
    match format {
        SlotOutputFormat::Toml => {
            writeln!(w, "[slots.{}]", slot.name)?;
            emit_slot_toml_body(w, slot)?;
        }
        SlotOutputFormat::Json => {
            let value = slot_to_json(slot);
            serde_json::to_writer_pretty(&mut *w, &value)?;
            writeln!(w)?;
        }
        SlotOutputFormat::Nix => {
            emit_slot_nix(w, slot)?;
        }
    }
    Ok(())
}

/// Print a slot's resolved paths.
///
/// # Errors
///
/// Returns errors for manifest-load or resolution failures.
pub fn cmd_slot_paths<W: Write>(
    w: &mut W,
    name: Option<&str>,
    default_name: Option<&str>,
) -> Result<()> {
    let manifest = load_manifest()?;
    let slot = resolve_slot_name(&manifest, name, default_name)?;
    writeln!(w, "slot         = {}", slot.name)?;
    writeln!(w, "binary       = {}", slot.binary.display())?;
    writeln!(w, "config_dir   = {}", slot.config_dir.display())?;
    writeln!(w, "runtime_dir  = {}", slot.runtime_dir.display())?;
    writeln!(w, "data_dir     = {}", slot.data_dir.display())?;
    writeln!(w, "state_dir    = {}", slot.state_dir.display())?;
    writeln!(w, "log_dir      = {}", slot.log_dir.display())?;
    writeln!(w, "inherit_base = {}", slot.inherit_base)?;
    Ok(())
}

/// Run the manifest doctor, printing advisory messages.
///
/// Returns `true` when everything is healthy, `false` otherwise.
///
/// # Errors
///
/// Returns errors for I/O failures on the output stream.
pub fn cmd_slot_doctor<W: Write>(w: &mut W) -> Result<bool> {
    let manifest = match SlotManifest::load_default() {
        Ok(m) => m,
        Err(e) => {
            writeln!(w, "doctor: failed to load manifest: {e}")?;
            return Ok(false);
        }
    };
    if manifest.slots.is_empty() {
        writeln!(w, "doctor: no slots declared (legacy single-install mode)")?;
        return Ok(true);
    }
    writeln!(w, "doctor: {} slot(s) declared", manifest.slots.len())?;
    let mut any_fail = false;
    for (name, slot) in &manifest.slots {
        writeln!(w, "  slot {name}:")?;
        if validate_slot_name(name).is_err() {
            writeln!(w, "    ✗ invalid name")?;
            any_fail = true;
        } else {
            writeln!(w, "    ✓ name valid")?;
        }
        if slot.binary.exists() {
            writeln!(w, "    ✓ binary present")?;
        } else {
            writeln!(w, "    ✗ binary {} does not exist", slot.binary.display())?;
            any_fail = true;
        }
        if slot.inherit_base {
            let base = bmux_slots::default_base_config_path();
            if base.exists() {
                writeln!(w, "    ✓ base.toml present at {}", base.display())?;
            } else {
                writeln!(
                    w,
                    "    ⚠ inherit_base = true but base.toml missing at {}",
                    base.display()
                )?;
            }
        }
    }
    Ok(!any_fail)
}

// ---------------------------------------------------------------------------
// slot write commands (install / uninstall)
// ---------------------------------------------------------------------------

/// Outcome of [`cmd_install`], for callers that want to distinguish success
/// from a read-only refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The slot binary + manifest block were written successfully.
    Written,
    /// Manifest was detected as read-only; nothing was written.
    RefusedReadOnly,
    /// `--dry-run` was specified; nothing was written.
    DryRun,
    /// A slot with the same name already exists and the caller did not
    /// pass `overwrite` (and either is non-interactive or `yes` was set).
    RefusedDuplicate,
    /// A slot with the same name already exists and the user declined the
    /// overwrite confirmation prompt.
    RefusedCancelled,
}

/// Install a new slot.
///
/// On success (or in dry-run / read-only refusal), emits the would-be manifest
/// block to `w` in the requested format. When writing, also creates
/// `<bin_dir>/bmux-<name>` as a symlink or copy and appends the block to the
/// manifest.
///
/// # Errors
///
/// - Invalid slot name.
/// - Binary path missing.
/// - Filesystem failures while creating symlink or copy.
/// - Manifest write failures (distinct from read-only refusal, which is
///   signaled via [`InstallOutcome::RefusedReadOnly`] rather than an Err).
pub fn cmd_install<W: Write>(w: &mut W, params: &InstallParams) -> Result<InstallOutcome> {
    validate_slot_name(&params.name).map_err(|e| anyhow!("{e}"))?;

    // Resolve the source binary (to absolute) before storing in manifest.
    let source_binary = if params.binary.is_absolute() {
        params.binary.clone()
    } else {
        std::env::current_dir()
            .context("resolve relative --binary path")?
            .join(&params.binary)
    };
    if !source_binary.exists() {
        bail!("source binary {} does not exist", source_binary.display());
    }

    let block = NewSlotBlock {
        name: params.name.clone(),
        binary: source_binary.clone(),
        inherit_base: params.inherit_base,
    };

    // Always emit the block for the user to see.
    emit_install_block(w, &block, params.format)?;

    if params.dry_run {
        return Ok(InstallOutcome::DryRun);
    }

    let manifest_path = default_manifest_path();
    if manifest_path.exists() && is_read_only_manifest(&manifest_path) {
        writeln!(
            w,
            "note: {} is managed declaratively (read-only); block above is for manual insertion.",
            manifest_path.display()
        )?;
        return Ok(InstallOutcome::RefusedReadOnly);
    }

    // Detect a pre-existing `[slots.<name>]` block in the target manifest file
    // (we intentionally do not consider `extend` files — we only edit this
    // file). If found, either refuse, prompt, or remove it depending on
    // `overwrite` / `yes` / TTY.
    let existing = manifest_has_slot(&manifest_path, &params.name)?;
    if existing {
        match resolve_overwrite_decision(w, &params.name, params.overwrite, params.yes)? {
            OverwriteDecision::Proceed => {
                writeln!(w, "overwriting existing slot '{}'", params.name)?;
                // Drop the old block. Ignore `UnknownSlot` since we already
                // know it exists; surface any other error.
                remove_slot_block(&manifest_path, &params.name).map_err(|e| anyhow!("{e}"))?;
            }
            OverwriteDecision::RefusedDuplicate => {
                writeln!(
                    w,
                    "slot '{}' already exists; pass --overwrite to replace it",
                    params.name
                )?;
                return Ok(InstallOutcome::RefusedDuplicate);
            }
            OverwriteDecision::RefusedCancelled => {
                writeln!(w, "aborted; slot '{}' left unchanged", params.name)?;
                return Ok(InstallOutcome::RefusedCancelled);
            }
        }
    }

    // Place the binary first so a partial failure is easier to reason about
    // (user can retry the manifest write).
    let bin_dir = params.bin_dir.clone().unwrap_or_else(default_bin_dir);
    std::fs::create_dir_all(&bin_dir)
        .with_context(|| format!("create bin dir {}", bin_dir.display()))?;
    let target_path = bin_dir.join(format!("bmux-{}", params.name));
    if target_path.exists() || target_path.is_symlink() {
        // Replace existing link/file; callers are expected to run uninstall
        // first if they want to preserve the old link.
        std::fs::remove_file(&target_path)
            .with_context(|| format!("remove pre-existing {}", target_path.display()))?;
    }
    match params.mode {
        InstallMode::Symlink => {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&source_binary, &target_path).with_context(|| {
                    format!(
                        "symlink {} -> {}",
                        target_path.display(),
                        source_binary.display()
                    )
                })?;
            }
            #[cfg(windows)]
            {
                // File symlinks on Windows need special perms; fall back to copy
                // if symlink fails.
                if let Err(e) = std::os::windows::fs::symlink_file(&source_binary, &target_path) {
                    writeln!(w, "note: symlink failed ({e}); falling back to copy")?;
                    std::fs::copy(&source_binary, &target_path)
                        .with_context(|| format!("copy fallback to {}", target_path.display()))?;
                }
            }
        }
        InstallMode::Copy => {
            std::fs::copy(&source_binary, &target_path).with_context(|| {
                format!(
                    "copy {} -> {}",
                    source_binary.display(),
                    target_path.display()
                )
            })?;
        }
    }

    write_slot_block(&manifest_path, &block).map_err(|e| anyhow!("{e}"))?;
    writeln!(
        w,
        "installed slot '{}' at {} (manifest: {})",
        params.name,
        target_path.display(),
        manifest_path.display()
    )?;
    Ok(InstallOutcome::Written)
}

/// Decision produced by [`resolve_overwrite_decision`] for a slot install
/// request where a manifest block with the same name already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverwriteDecision {
    /// Remove the existing block and continue with the install.
    Proceed,
    /// Do not overwrite; caller did not opt in (non-interactive or `--yes`
    /// without `--overwrite`).
    RefusedDuplicate,
    /// Do not overwrite; user explicitly declined the confirmation prompt.
    RefusedCancelled,
}

/// Check whether the target manifest file already contains a
/// `[slots.<name>]` block. Mirrors the detection used by
/// [`bmux_slots::write_slot_block`] so both paths agree.
fn manifest_has_slot(manifest_path: &Path, name: &str) -> Result<bool> {
    let contents = match std::fs::read_to_string(manifest_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(anyhow!(
                "failed reading manifest {}: {e}",
                manifest_path.display()
            ));
        }
    };
    let header = format!("[slots.{name}]");
    Ok(contents.contains(&header))
}

/// Decide what to do when the target slot name already exists. On an
/// interactive TTY the user is prompted (`[y/N]`). Non-interactive callers
/// must pass `overwrite` to proceed.
fn resolve_overwrite_decision<W: Write>(
    w: &mut W,
    name: &str,
    overwrite: bool,
    yes: bool,
) -> Result<OverwriteDecision> {
    let interactive = std::io::stdin().is_terminal();

    // With `--overwrite --yes`, skip the prompt entirely.
    if overwrite && yes {
        return Ok(OverwriteDecision::Proceed);
    }

    // Non-interactive: require `--overwrite` to proceed; no prompting.
    if !interactive {
        if overwrite {
            return Ok(OverwriteDecision::Proceed);
        }
        return Ok(OverwriteDecision::RefusedDuplicate);
    }

    // Interactive TTY: always confirm before replacing (even with
    // `--overwrite`, since the operation is destructive).
    if prompt_overwrite_confirmation(w, name)? {
        Ok(OverwriteDecision::Proceed)
    } else {
        Ok(OverwriteDecision::RefusedCancelled)
    }
}

/// Prompt the user on stdin/stdout for an overwrite confirmation. Returns
/// `true` only when the user typed `y` / `yes` (case-insensitive). Default
/// is No.
fn prompt_overwrite_confirmation<W: Write>(w: &mut W, name: &str) -> Result<bool> {
    writeln!(w, "Slot '{name}' already exists. Overwrite? [y/N]")?;
    w.flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("failed reading overwrite confirmation")?;
    let trimmed = answer.trim().to_ascii_lowercase();
    Ok(trimmed == "y" || trimmed == "yes")
}

/// Outcome of [`cmd_uninstall`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// Removed from manifest and binary link.
    Removed,
    /// Manifest was read-only; nothing changed.
    RefusedReadOnly,
}

/// Remove a slot's manifest block and binary link.
///
/// When `purge` is true, also removes the slot's config/data/state/log dirs.
/// Otherwise the slot's on-disk data is preserved.
///
/// # Errors
///
/// - Invalid slot name.
/// - Manifest I/O failures.
pub fn cmd_uninstall<W: Write>(
    w: &mut W,
    name: &str,
    purge: bool,
    bin_dir: Option<&Path>,
) -> Result<UninstallOutcome> {
    validate_slot_name(name).map_err(|e| anyhow!("{e}"))?;

    let manifest_path = default_manifest_path();
    if manifest_path.exists() && is_read_only_manifest(&manifest_path) {
        writeln!(
            w,
            "note: {} is managed declaratively (read-only); cannot uninstall.",
            manifest_path.display()
        )?;
        return Ok(UninstallOutcome::RefusedReadOnly);
    }

    // Snapshot slot paths for --purge before we drop the manifest block.
    let purge_paths = if purge {
        SlotManifest::load_default()
            .ok()
            .and_then(|m| m.slots.get(name).cloned())
            .map(|s| vec![s.config_dir, s.data_dir, s.state_dir, s.log_dir])
    } else {
        None
    };

    // Remove manifest block. Missing manifest => warn-only.
    if manifest_path.exists() {
        match remove_slot_block(&manifest_path, name) {
            Ok(()) => {}
            Err(e) => {
                writeln!(w, "warning: removing manifest block: {e}")?;
            }
        }
    }

    // Remove binary link/copy.
    let bin_dir = bin_dir.map_or_else(default_bin_dir, Path::to_path_buf);
    let target = bin_dir.join(format!("bmux-{name}"));
    if (target.exists() || target.is_symlink())
        && let Err(e) = std::fs::remove_file(&target)
    {
        writeln!(w, "warning: removing {}: {e}", target.display())?;
    }

    if let Some(paths) = purge_paths {
        for p in paths {
            if p.exists()
                && let Err(e) = std::fs::remove_dir_all(&p)
            {
                writeln!(w, "warning: purging {}: {e}", p.display())?;
            }
        }
    }

    writeln!(w, "uninstalled slot '{name}'")?;
    Ok(UninstallOutcome::Removed)
}

fn emit_install_block<W: Write>(
    w: &mut W,
    block: &NewSlotBlock,
    format: SlotOutputFormat,
) -> Result<()> {
    match format {
        SlotOutputFormat::Toml => {
            writeln!(w, "# Slot block:")?;
            write!(w, "{}", render_slot_block_toml(block))?;
        }
        SlotOutputFormat::Json => {
            let value = serde_json::json!({
                "name": block.name,
                "binary": block.binary.to_string_lossy(),
                "inherit_base": block.inherit_base,
            });
            serde_json::to_writer_pretty(&mut *w, &value)?;
            writeln!(w)?;
        }
        SlotOutputFormat::Nix => {
            writeln!(w, "# Home Manager attrset:")?;
            writeln!(w, "slots.{} = {{", block.name)?;
            writeln!(
                w,
                "  binary = {};",
                nix_string(&block.binary.to_string_lossy())
            )?;
            writeln!(w, "  inheritBase = {};", block.inherit_base)?;
            writeln!(w, "}};")?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers shared across commands
// ---------------------------------------------------------------------------

fn load_manifest() -> Result<SlotManifest> {
    SlotManifest::load_default().map_err(|e| anyhow::anyhow!("failed loading slot manifest: {e}"))
}

fn resolve_slot_name<'a>(
    manifest: &'a SlotManifest,
    name: Option<&str>,
    default_name: Option<&str>,
) -> Result<&'a Slot> {
    if let Some(n) = name {
        return manifest.get(n).map_err(|e| anyhow!("{e}"));
    }
    if let Some(n) = default_name {
        return manifest.get(n).map_err(|e| anyhow!("{e}"));
    }
    if let Some(d) = manifest.resolved_default() {
        return Ok(d);
    }
    anyhow::bail!(
        "no slot specified and no active or default slot is set (manifest has {} slots)",
        manifest.slots.len()
    )
}

fn emit_toml_list<W: Write>(w: &mut W, m: &SlotManifest) -> std::io::Result<()> {
    if let Some(ref d) = m.default {
        writeln!(w, "default = {}", bmux_slots::toml_string_literal(d))?;
        writeln!(w)?;
    }
    for (name, slot) in &m.slots {
        writeln!(w, "[slots.{name}]")?;
        emit_slot_toml_body(w, slot)?;
        writeln!(w)?;
    }
    Ok(())
}

fn emit_slot_toml_body<W: Write>(w: &mut W, slot: &Slot) -> std::io::Result<()> {
    writeln!(
        w,
        "binary       = {}",
        bmux_slots::toml_string_literal(&slot.binary.to_string_lossy())
    )?;
    writeln!(w, "inherit_base = {}", slot.inherit_base)?;
    writeln!(
        w,
        "config_dir   = {}",
        bmux_slots::toml_string_literal(&slot.config_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "runtime_dir  = {}",
        bmux_slots::toml_string_literal(&slot.runtime_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "data_dir     = {}",
        bmux_slots::toml_string_literal(&slot.data_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "state_dir    = {}",
        bmux_slots::toml_string_literal(&slot.state_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "log_dir      = {}",
        bmux_slots::toml_string_literal(&slot.log_dir.to_string_lossy())
    )?;
    Ok(())
}

fn emit_json_list<W: Write>(w: &mut W, m: &SlotManifest) -> std::io::Result<()> {
    let value = serde_json::json!({
        "default": m.default,
        "slots": m.slots.values().map(slot_to_json).collect::<Vec<_>>(),
    });
    serde_json::to_writer_pretty(&mut *w, &value)?;
    writeln!(w)
}

fn slot_to_json(slot: &Slot) -> serde_json::Value {
    serde_json::json!({
        "name": slot.name,
        "binary": slot.binary.to_string_lossy(),
        "inherit_base": slot.inherit_base,
        "config_dir": slot.config_dir.to_string_lossy(),
        "runtime_dir": slot.runtime_dir.to_string_lossy(),
        "data_dir": slot.data_dir.to_string_lossy(),
        "state_dir": slot.state_dir.to_string_lossy(),
        "log_dir": slot.log_dir.to_string_lossy(),
    })
}

fn emit_nix_list<W: Write>(w: &mut W, m: &SlotManifest) -> std::io::Result<()> {
    writeln!(w, "{{")?;
    if let Some(ref d) = m.default {
        writeln!(w, "  default = {};", nix_string(d))?;
    }
    writeln!(w, "  slots = {{")?;
    for (name, slot) in &m.slots {
        writeln!(w, "    {name} = {{")?;
        writeln!(
            w,
            "      binary = {};",
            nix_string(&slot.binary.to_string_lossy())
        )?;
        writeln!(w, "      inheritBase = {};", slot.inherit_base)?;
        writeln!(
            w,
            "      configDir = {};",
            nix_string(&slot.config_dir.to_string_lossy())
        )?;
        writeln!(
            w,
            "      runtimeDir = {};",
            nix_string(&slot.runtime_dir.to_string_lossy())
        )?;
        writeln!(
            w,
            "      dataDir = {};",
            nix_string(&slot.data_dir.to_string_lossy())
        )?;
        writeln!(
            w,
            "      stateDir = {};",
            nix_string(&slot.state_dir.to_string_lossy())
        )?;
        writeln!(
            w,
            "      logDir = {};",
            nix_string(&slot.log_dir.to_string_lossy())
        )?;
        writeln!(w, "    }};")?;
    }
    writeln!(w, "  }};")?;
    writeln!(w, "}}")?;
    Ok(())
}

fn emit_slot_nix<W: Write>(w: &mut W, slot: &Slot) -> std::io::Result<()> {
    writeln!(w, "{{")?;
    writeln!(w, "  name = {};", nix_string(&slot.name))?;
    writeln!(
        w,
        "  binary = {};",
        nix_string(&slot.binary.to_string_lossy())
    )?;
    writeln!(w, "  inheritBase = {};", slot.inherit_base)?;
    writeln!(
        w,
        "  configDir = {};",
        nix_string(&slot.config_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "  runtimeDir = {};",
        nix_string(&slot.runtime_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "  dataDir = {};",
        nix_string(&slot.data_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "  stateDir = {};",
        nix_string(&slot.state_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "  logDir = {};",
        nix_string(&slot.log_dir.to_string_lossy())
    )?;
    writeln!(w, "}}")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// shell dialect helpers
// ---------------------------------------------------------------------------

fn bash_zsh(bin_dir: &str) -> String {
    let quoted = shell_double_quote_body(bin_dir);
    format!(
        r#"# bmux-env shell (bash/zsh)
export BMUX_SLOTS_BIN_DIR="${{BMUX_SLOTS_BIN_DIR:-{quoted}}}"
case ":$PATH:" in
  *":$BMUX_SLOTS_BIN_DIR:"*) ;;
  *) export PATH="$BMUX_SLOTS_BIN_DIR:$PATH" ;;
esac
"#
    )
}

fn posix(bin_dir: &str) -> String {
    let quoted = shell_double_quote_body(bin_dir);
    format!(
        r#"# bmux-env shell (posix)
BMUX_SLOTS_BIN_DIR="${{BMUX_SLOTS_BIN_DIR:-{quoted}}}"
export BMUX_SLOTS_BIN_DIR
case ":$PATH:" in
  *":$BMUX_SLOTS_BIN_DIR:"*) ;;
  *) PATH="$BMUX_SLOTS_BIN_DIR:$PATH"; export PATH ;;
esac
"#
    )
}

fn fish(bin_dir: &str) -> String {
    let quoted = shell_single_quote(bin_dir);
    format!(
        r#"# bmux-env shell (fish)
if not set -q BMUX_SLOTS_BIN_DIR
    set -gx BMUX_SLOTS_BIN_DIR {quoted}
end
if not contains -- $BMUX_SLOTS_BIN_DIR $PATH
    set -gx PATH $BMUX_SLOTS_BIN_DIR $PATH
end
"#
    )
}

fn nushell(bin_dir: &str) -> String {
    let quoted = nu_double_quote(bin_dir);
    format!(
        r#"# bmux-env shell (nushell)
let-env BMUX_SLOTS_BIN_DIR = ($env.BMUX_SLOTS_BIN_DIR? | default {quoted})
let-env PATH = (
    $env.PATH
    | split row (char esep)
    | prepend $env.BMUX_SLOTS_BIN_DIR
    | uniq
    | str collect (char esep)
)
"#
    )
}

fn powershell(bin_dir: &str) -> String {
    let quoted = ps_double_quote(bin_dir);
    format!(
        r#"# bmux-env shell (powershell)
if (-not $env:BMUX_SLOTS_BIN_DIR) {{ $env:BMUX_SLOTS_BIN_DIR = {quoted} }}
$sep = [System.IO.Path]::PathSeparator
$parts = $env:PATH.Split($sep)
if ($parts -notcontains $env:BMUX_SLOTS_BIN_DIR) {{
    $env:PATH = $env:BMUX_SLOTS_BIN_DIR + $sep + $env:PATH
}}
"#
    )
}

fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn shell_double_quote_body(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn nu_double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

fn ps_double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push_str("`\"");
        } else if ch == '`' {
            out.push_str("``");
        } else if ch == '$' {
            out.push_str("`$");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    out
}

fn nix_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_zsh_output_is_idempotent_shape() {
        let out = bash_zsh("/some/bin");
        assert!(out.contains("export BMUX_SLOTS_BIN_DIR=\"${BMUX_SLOTS_BIN_DIR:-/some/bin}\""));
        assert!(out.contains(r#"case ":$PATH:""#));
        assert!(out.contains("export PATH=\"$BMUX_SLOTS_BIN_DIR:$PATH\""));
    }

    #[test]
    fn shell_double_quote_body_escapes_metachars() {
        assert_eq!(shell_double_quote_body("a b"), "a b");
        assert_eq!(shell_double_quote_body(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(shell_double_quote_body("$var"), r"\$var");
        assert_eq!(shell_double_quote_body("a`b"), r"a\`b");
    }

    #[test]
    fn nix_string_escapes_quotes_and_backslashes() {
        assert_eq!(nix_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(nix_string(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn cmd_install_dry_run_does_not_touch_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("fake-bmux");
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        let bin_dir = tmp.path().join("bin");

        let params = InstallParams {
            name: "cursor".into(),
            binary: binary.clone(),
            inherit_base: false,
            mode: InstallMode::Symlink,
            bin_dir: Some(bin_dir.clone()),
            format: SlotOutputFormat::Toml,
            dry_run: true,
            overwrite: false,
            yes: false,
        };
        let mut out = Vec::new();
        let outcome = cmd_install(&mut out, &params).unwrap();
        assert!(matches!(outcome, InstallOutcome::DryRun));
        assert!(!bin_dir.join("bmux-cursor").exists());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[slots.cursor]"));
    }

    #[test]
    fn manifest_has_slot_detects_existing_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("slots.toml");
        std::fs::write(
            &path,
            "[slots.dev]\nbinary = \"/tmp/bmux\"\ninherit_base = true\n",
        )
        .unwrap();
        assert!(manifest_has_slot(&path, "dev").unwrap());
        assert!(!manifest_has_slot(&path, "prod").unwrap());
    }

    #[test]
    fn manifest_has_slot_returns_false_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.toml");
        assert!(!manifest_has_slot(&path, "dev").unwrap());
    }

    // Global mutex serialising tests that mutate BMUX_SLOTS_MANIFEST /
    // BMUX_SLOTS_BIN_DIR. Tests are otherwise racy since env is process-wide.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let prev = std::env::var_os(key);
            // Safety: tests are serialised via env_lock() to avoid races.
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Safety: tests are serialised via env_lock() to avoid races.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn make_install_params(
        name: &str,
        binary: PathBuf,
        bin_dir: PathBuf,
        overwrite: bool,
        yes: bool,
    ) -> InstallParams {
        InstallParams {
            name: name.to_string(),
            binary,
            inherit_base: false,
            mode: InstallMode::Symlink,
            bin_dir: Some(bin_dir),
            format: SlotOutputFormat::Toml,
            dry_run: false,
            overwrite,
            yes,
        }
    }

    #[test]
    fn cmd_install_refuses_duplicate_without_overwrite_non_interactive() {
        let _g = env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("slots.toml");
        let bin_dir = tmp.path().join("bin");
        let binary = tmp.path().join("fake-bmux");
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        // Seed an existing slot block.
        std::fs::write(
            &manifest,
            "[slots.dev]\nbinary = \"/tmp/old-bmux\"\ninherit_base = true\n",
        )
        .unwrap();

        let _manifest_env = EnvGuard::set(bmux_slots::SLOTS_MANIFEST_ENV, &manifest);

        let params = make_install_params("dev", binary, bin_dir, false, false);
        let mut out = Vec::new();
        let outcome = cmd_install(&mut out, &params).unwrap();
        assert!(matches!(outcome, InstallOutcome::RefusedDuplicate));

        // Manifest untouched.
        let contents = std::fs::read_to_string(&manifest).unwrap();
        assert!(contents.contains("/tmp/old-bmux"));

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("already exists"));
        assert!(text.contains("--overwrite"));
    }

    #[test]
    fn cmd_install_overwrite_yes_replaces_existing_slot() {
        let _g = env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("slots.toml");
        let bin_dir = tmp.path().join("bin");
        let binary = tmp.path().join("new-bmux");
        std::fs::write(&binary, b"#!/bin/sh\n# new\n").unwrap();
        std::fs::write(
            &manifest,
            "[slots.dev]\nbinary = \"/tmp/old-bmux\"\ninherit_base = true\n",
        )
        .unwrap();

        let _manifest_env = EnvGuard::set(bmux_slots::SLOTS_MANIFEST_ENV, &manifest);

        let params = make_install_params("dev", binary.clone(), bin_dir.clone(), true, true);
        let mut out = Vec::new();
        let outcome = cmd_install(&mut out, &params).unwrap();
        assert!(matches!(outcome, InstallOutcome::Written));

        // The old block is gone; the new one points at the new binary.
        let contents = std::fs::read_to_string(&manifest).unwrap();
        assert!(!contents.contains("/tmp/old-bmux"));
        assert!(contents.contains(binary.to_string_lossy().as_ref()));

        // The bin-dir symlink/file exists for the slot.
        assert!(bin_dir.join("bmux-dev").exists() || bin_dir.join("bmux-dev").is_symlink());

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("overwriting existing slot 'dev'"));
        assert!(text.contains("installed slot 'dev'"));
    }

    #[test]
    fn cmd_install_dry_run_with_existing_slot_does_not_touch_disk() {
        let _g = env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("slots.toml");
        let bin_dir = tmp.path().join("bin");
        let binary = tmp.path().join("new-bmux");
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        let original = "[slots.dev]\nbinary = \"/tmp/old-bmux\"\ninherit_base = true\n";
        std::fs::write(&manifest, original).unwrap();

        let _manifest_env = EnvGuard::set(bmux_slots::SLOTS_MANIFEST_ENV, &manifest);

        let mut params = make_install_params("dev", binary, bin_dir.clone(), false, false);
        params.dry_run = true;
        let mut out = Vec::new();
        let outcome = cmd_install(&mut out, &params).unwrap();
        assert!(matches!(outcome, InstallOutcome::DryRun));

        // Manifest untouched; bin-dir not even created.
        assert_eq!(std::fs::read_to_string(&manifest).unwrap(), original);
        assert!(!bin_dir.exists());
    }

    // tempfile is a dev-dependency of bmux_slots; surface it here via
    // bmux_slots' re-export would be cleaner long-term, but we can pull it
    // directly via the workspace dev-deps for now. This test crate already
    // re-uses tempfile through bmux_slots' dev-deps chain.
}
