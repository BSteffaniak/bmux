//! `bmux slot ...` and `bmux env ...` subcommand handlers.
//!
//! Both namespaces delegate to the same implementation in [`bmux_env`] to
//! guarantee identical behavior regardless of which front-end is invoked.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bmux_cli_schema::{SlotInstallMode, SlotOutputFormat, SlotPrintFormat, SlotShellKind};
use bmux_env::{
    InstallMode, InstallOutcome, InstallParams, PrintFormat, ShellKind,
    SlotOutputFormat as EnvSlotOutputFormat, UninstallOutcome, cmd_exec, cmd_install, cmd_print,
    cmd_shell, cmd_slot_doctor, cmd_slot_list, cmd_slot_paths, cmd_slot_show, cmd_uninstall,
};

// ---------------------------------------------------------------------------
// Format conversions (schema types -> bmux_env types).
// ---------------------------------------------------------------------------

const fn to_env_output(v: SlotOutputFormat) -> EnvSlotOutputFormat {
    match v {
        SlotOutputFormat::Toml => EnvSlotOutputFormat::Toml,
        SlotOutputFormat::Json => EnvSlotOutputFormat::Json,
        SlotOutputFormat::Nix => EnvSlotOutputFormat::Nix,
    }
}

const fn to_env_shell(v: SlotShellKind) -> ShellKind {
    match v {
        SlotShellKind::Auto => ShellKind::Auto,
        SlotShellKind::Bash => ShellKind::Bash,
        SlotShellKind::Zsh => ShellKind::Zsh,
        SlotShellKind::Fish => ShellKind::Fish,
        SlotShellKind::Nushell => ShellKind::Nushell,
        SlotShellKind::Powershell => ShellKind::Powershell,
        SlotShellKind::Posix => ShellKind::Posix,
    }
}

const fn to_env_print(v: SlotPrintFormat) -> PrintFormat {
    match v {
        SlotPrintFormat::Shell => PrintFormat::Shell,
        SlotPrintFormat::Json => PrintFormat::Json,
        SlotPrintFormat::Nix => PrintFormat::Nix,
        SlotPrintFormat::Fish => PrintFormat::Fish,
    }
}

const fn to_env_install_mode(v: SlotInstallMode) -> InstallMode {
    match v {
        SlotInstallMode::Symlink => InstallMode::Symlink,
        SlotInstallMode::Copy => InstallMode::Copy,
    }
}

fn active_slot_name() -> Option<String> {
    match crate::runtime::slot::active_slot() {
        crate::runtime::slot::ActiveSlotState::Resolved { slot, .. } => Some(slot.name.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public handlers used by dispatch.rs.
// ---------------------------------------------------------------------------

pub(super) fn run_slot_list(format: SlotOutputFormat) -> Result<u8> {
    let mut stdout = std::io::stdout().lock();
    cmd_slot_list(&mut stdout, to_env_output(format))?;
    Ok(0)
}

pub(super) fn run_slot_show(name: Option<&str>, format: SlotOutputFormat) -> Result<u8> {
    let active = active_slot_name();
    let mut stdout = std::io::stdout().lock();
    cmd_slot_show(&mut stdout, name, active.as_deref(), to_env_output(format))?;
    Ok(0)
}

pub(super) fn run_slot_paths(name: Option<&str>) -> Result<u8> {
    let active = active_slot_name();
    let mut stdout = std::io::stdout().lock();
    cmd_slot_paths(&mut stdout, name, active.as_deref())?;
    Ok(0)
}

pub(super) fn run_slot_doctor() -> Result<u8> {
    let mut stdout = std::io::stdout().lock();
    let ok = cmd_slot_doctor(&mut stdout)?;
    Ok(u8::from(!ok))
}

/// Bundled arguments for [`run_slot_install`]. Grouped into a struct to
/// keep the call-site readable and avoid `too_many_arguments` /
/// `fn_params_excessive_bools` clippy noise.
///
/// The four bool fields each represent an independent CLI flag
/// (`--no-inherit-base`, `--dry-run`, `--overwrite`, `--yes`) that the
/// user can toggle in any combination. Collapsing them into state-machine
/// enums would obscure the direct CLI-to-struct mapping without improving
/// clarity, so `struct_excessive_bools` is silenced here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Copy, Clone)]
pub(super) struct SlotInstallOptions<'a> {
    pub name: &'a str,
    pub binary: &'a str,
    pub inherit_base: bool,
    pub mode: SlotInstallMode,
    pub bin_dir: Option<&'a Path>,
    pub format: SlotOutputFormat,
    pub dry_run: bool,
    /// Replace an existing slot with the same name.
    pub overwrite: bool,
    /// Skip interactive confirmation prompts.
    pub yes: bool,
}

pub(super) fn run_slot_install(opts: SlotInstallOptions<'_>) -> Result<u8> {
    let params = InstallParams {
        name: opts.name.to_string(),
        binary: PathBuf::from(opts.binary),
        inherit_base: opts.inherit_base,
        mode: to_env_install_mode(opts.mode),
        bin_dir: opts.bin_dir.map(Path::to_path_buf),
        format: to_env_output(opts.format),
        dry_run: opts.dry_run,
        overwrite: opts.overwrite,
        yes: opts.yes,
    };
    let mut stdout = std::io::stdout().lock();
    match cmd_install(&mut stdout, &params)? {
        InstallOutcome::Written | InstallOutcome::DryRun => Ok(0),
        InstallOutcome::RefusedReadOnly => Ok(77),
        InstallOutcome::RefusedDuplicate | InstallOutcome::RefusedCancelled => Ok(1),
    }
}

pub(super) fn run_slot_uninstall(name: &str, purge: bool, bin_dir: Option<&Path>) -> Result<u8> {
    let mut stdout = std::io::stdout().lock();
    match cmd_uninstall(&mut stdout, name, purge, bin_dir)? {
        UninstallOutcome::Removed => Ok(0),
        UninstallOutcome::RefusedReadOnly => Ok(77),
    }
}

pub(super) fn run_slot_shell(shell: SlotShellKind) -> Result<u8> {
    let mut stdout = std::io::stdout().lock();
    cmd_shell(&mut stdout, to_env_shell(shell))?;
    Ok(0)
}

pub(super) fn run_slot_exec(slot: &str, argv: &[String]) -> Result<u8> {
    cmd_exec(slot, argv)?;
    Ok(0)
}

pub(super) fn run_slot_print(format: SlotPrintFormat) -> Result<u8> {
    let mut stdout = std::io::stdout().lock();
    cmd_print(&mut stdout, to_env_print(format))?;
    Ok(0)
}
