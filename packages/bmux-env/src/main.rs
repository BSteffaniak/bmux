//! `bmux-env` binary — thin wrapper around [`bmux_env`] lib.
//!
//! See [`bmux_env`] for the full command surface. This binary is identical
//! to the `bmux env` / `bmux slot` subcommand trees exposed by bmux_cli.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

use std::path::PathBuf;
use std::process::ExitCode;

use bmux_env::{
    InstallMode, InstallOutcome, InstallParams, PrintFormat, ShellKind, SlotOutputFormat,
    UninstallOutcome, cmd_exec, cmd_install, cmd_print, cmd_shell, cmd_slot_doctor, cmd_slot_list,
    cmd_slot_paths, cmd_slot_show, cmd_uninstall,
};
use clap::{Parser, Subcommand, ValueEnum};

/// Pure-printer PATH / env helper for bmux slot installs.
#[derive(Parser, Debug)]
#[command(name = "bmux-env", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print shell code that prepends `$BMUX_SLOTS_BIN_DIR` to `PATH`.
    Shell {
        #[arg(long, value_enum, default_value_t = ShellKindArg::Auto)]
        shell: ShellKindArg,
    },
    /// Run a command with a slot's env applied (re-execs via execvp).
    Exec {
        slot: String,
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Print the resolved env-var set as structured data.
    Print {
        #[arg(long, value_enum, default_value_t = PrintFormatArg::Shell)]
        format: PrintFormatArg,
    },
    /// List all declared slots.
    List {
        #[arg(long, value_enum, default_value_t = SlotOutputFormatArg::Toml)]
        format: SlotOutputFormatArg,
    },
    /// Show a single slot's detail.
    Show {
        name: Option<String>,
        #[arg(long, value_enum, default_value_t = SlotOutputFormatArg::Toml)]
        format: SlotOutputFormatArg,
    },
    /// Print a slot's resolved paths.
    Paths { name: Option<String> },
    /// Validate the slot manifest.
    Doctor,
    /// Install a new slot (writes to the manifest unless it is read-only).
    Install {
        /// New slot name.
        name: String,
        /// Path to the source `bmux` binary.
        binary: PathBuf,
        /// Disable base-config inheritance for this slot.
        #[arg(long)]
        no_inherit_base: bool,
        /// Symlink the binary (default) or copy it.
        #[arg(long, value_enum, default_value_t = InstallModeArg::Symlink)]
        mode: InstallModeArg,
        /// Destination bin dir for `bmux-<slot>`. Defaults to ~/.local/bin.
        #[arg(long)]
        bin_dir: Option<PathBuf>,
        /// Output format for the printed block.
        #[arg(long, value_enum, default_value_t = SlotOutputFormatArg::Toml)]
        format: SlotOutputFormatArg,
        /// Do not modify disk; just print what would happen.
        #[arg(long)]
        dry_run: bool,
    },
    /// Uninstall a slot.
    Uninstall {
        name: String,
        /// Also remove the slot's config/data/state/log dirs.
        #[arg(long)]
        purge: bool,
        /// Destination bin dir the slot binary lives in.
        #[arg(long)]
        bin_dir: Option<PathBuf>,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum ShellKindArg {
    Auto,
    Bash,
    Zsh,
    Fish,
    Nushell,
    Powershell,
    Posix,
}

impl From<ShellKindArg> for ShellKind {
    fn from(v: ShellKindArg) -> Self {
        match v {
            ShellKindArg::Auto => Self::Auto,
            ShellKindArg::Bash => Self::Bash,
            ShellKindArg::Zsh => Self::Zsh,
            ShellKindArg::Fish => Self::Fish,
            ShellKindArg::Nushell => Self::Nushell,
            ShellKindArg::Powershell => Self::Powershell,
            ShellKindArg::Posix => Self::Posix,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum PrintFormatArg {
    Shell,
    Json,
    Nix,
    Fish,
}

impl From<PrintFormatArg> for PrintFormat {
    fn from(v: PrintFormatArg) -> Self {
        match v {
            PrintFormatArg::Shell => Self::Shell,
            PrintFormatArg::Json => Self::Json,
            PrintFormatArg::Nix => Self::Nix,
            PrintFormatArg::Fish => Self::Fish,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SlotOutputFormatArg {
    Toml,
    Json,
    Nix,
}

impl From<SlotOutputFormatArg> for SlotOutputFormat {
    fn from(v: SlotOutputFormatArg) -> Self {
        match v {
            SlotOutputFormatArg::Toml => Self::Toml,
            SlotOutputFormatArg::Json => Self::Json,
            SlotOutputFormatArg::Nix => Self::Nix,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum InstallModeArg {
    Symlink,
    Copy,
}

impl From<InstallModeArg> for InstallMode {
    fn from(v: InstallModeArg) -> Self {
        match v {
            InstallModeArg::Symlink => Self::Symlink,
            InstallModeArg::Copy => Self::Copy,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("bmux-env: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> anyhow::Result<u8> {
    let cli = Cli::parse();
    let mut stdout = std::io::stdout().lock();
    match cli.command {
        Command::Shell { shell } => {
            cmd_shell(&mut stdout, shell.into())?;
            Ok(0)
        }
        Command::Exec { slot, argv } => {
            cmd_exec(&slot, &argv)?;
            Ok(0)
        }
        Command::Print { format } => {
            cmd_print(&mut stdout, format.into())?;
            Ok(0)
        }
        Command::List { format } => {
            cmd_slot_list(&mut stdout, format.into())?;
            Ok(0)
        }
        Command::Show { name, format } => {
            cmd_slot_show(&mut stdout, name.as_deref(), None, format.into())?;
            Ok(0)
        }
        Command::Paths { name } => {
            cmd_slot_paths(&mut stdout, name.as_deref(), None)?;
            Ok(0)
        }
        Command::Doctor => {
            let ok = cmd_slot_doctor(&mut stdout)?;
            Ok(if ok { 0 } else { 1 })
        }
        Command::Install {
            name,
            binary,
            no_inherit_base,
            mode,
            bin_dir,
            format,
            dry_run,
        } => {
            let params = InstallParams {
                name,
                binary,
                inherit_base: !no_inherit_base,
                mode: mode.into(),
                bin_dir,
                format: format.into(),
                dry_run,
            };
            match cmd_install(&mut stdout, &params)? {
                InstallOutcome::Written | InstallOutcome::DryRun => Ok(0),
                // Distinct exit code for Nix-style declarative refusal so
                // automation can detect it cleanly.
                InstallOutcome::RefusedReadOnly => Ok(77),
            }
        }
        Command::Uninstall {
            name,
            purge,
            bin_dir,
        } => match cmd_uninstall(&mut stdout, &name, purge, bin_dir.as_deref())? {
            UninstallOutcome::Removed => Ok(0),
            UninstallOutcome::RefusedReadOnly => Ok(77),
        },
    }
}
