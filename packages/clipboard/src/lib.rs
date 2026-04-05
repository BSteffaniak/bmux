#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]
#![allow(clippy::option_if_let_else)]

use std::env;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClipboardCommand {
    program: &'static str,
    args: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClipboardError {
    #[error("clipboard backend not available on {os}")]
    BackendUnavailable { os: String },
    #[error("clipboard command '{program}' failed: {message}")]
    BackendFailed { program: String, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clipboard {
    command: ClipboardCommand,
}

impl Clipboard {
    /// Create a new clipboard handle by detecting the platform clipboard backend.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardError::BackendUnavailable`] if no supported clipboard
    /// command is found on the current platform.
    pub fn new() -> Result<Self, ClipboardError> {
        Self::for_os(env::consts::OS, command_exists)
    }

    fn for_os<F>(os: &str, mut exists: F) -> Result<Self, ClipboardError>
    where
        F: FnMut(&str) -> bool,
    {
        detect_backend(os, &mut exists)
            .map(|command| Self { command })
            .ok_or_else(|| ClipboardError::BackendUnavailable { os: os.to_string() })
    }

    /// Copy text to the system clipboard.
    ///
    /// Spawns the platform clipboard command, writes `text` to its stdin,
    /// and waits for it to exit.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardError::BackendFailed`] if the clipboard command
    /// cannot be spawned, stdin writing fails, or the command exits with a
    /// non-zero status.
    pub fn copy_text(&self, text: &str) -> Result<(), ClipboardError> {
        let mut child = Command::new(self.command.program)
            .args(self.command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| ClipboardError::BackendFailed {
                program: self.command.program.to_string(),
                message: error.to_string(),
            })?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(text.as_bytes())
                .and_then(|()| stdin.flush())
                .map_err(|error| ClipboardError::BackendFailed {
                    program: self.command.program.to_string(),
                    message: error.to_string(),
                })?;
        }

        let output = child
            .wait_with_output()
            .map_err(|error| ClipboardError::BackendFailed {
                program: self.command.program.to_string(),
                message: error.to_string(),
            })?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(ClipboardError::BackendFailed {
                program: self.command.program.to_string(),
                message: if stderr.is_empty() {
                    format!("exit status {}", output.status)
                } else {
                    stderr
                },
            })
        }
    }
}

/// Copy text to the system clipboard using a one-shot clipboard handle.
///
/// Convenience wrapper that creates a [`Clipboard`] and copies `text` in
/// a single call.
///
/// # Errors
///
/// Returns [`ClipboardError::BackendUnavailable`] if no clipboard backend
/// is found, or [`ClipboardError::BackendFailed`] if the copy command fails.
pub fn copy_text(text: &str) -> Result<(), ClipboardError> {
    Clipboard::new()?.copy_text(text)
}

fn detect_backend<F>(os: &str, exists: &mut F) -> Option<ClipboardCommand>
where
    F: FnMut(&str) -> bool,
{
    let candidates: &[ClipboardCommand] = match os {
        "macos" => &[ClipboardCommand {
            program: "pbcopy",
            args: &[],
        }],
        "linux" => &[
            ClipboardCommand {
                program: "wl-copy",
                args: &[],
            },
            ClipboardCommand {
                program: "xclip",
                args: &["-selection", "clipboard"],
            },
            ClipboardCommand {
                program: "xsel",
                args: &["--clipboard", "--input"],
            },
        ],
        "windows" => &[ClipboardCommand {
            program: "clip",
            args: &[],
        }],
        _ => &[],
    };
    candidates
        .iter()
        .copied()
        .find(|candidate| exists(candidate.program))
}

fn command_exists(program: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    let pathext = windows_path_exts();
    env::split_paths(&path).any(|dir| executable_exists_in_dir(&dir, program, &pathext))
}

fn executable_exists_in_dir(dir: &Path, program: &str, pathext: &[OsString]) -> bool {
    if program.contains(std::path::MAIN_SEPARATOR) {
        return is_file(&PathBuf::from(program));
    }

    let candidate = dir.join(program);
    if is_file(&candidate) {
        return true;
    }

    if cfg!(windows) {
        for ext in pathext {
            let mut path = candidate.clone().into_os_string();
            path.push(ext);
            if is_file(Path::new(&path)) {
                return true;
            }
        }
    }

    false
}

fn windows_path_exts() -> Vec<OsString> {
    if !cfg!(windows) {
        return Vec::new();
    }
    env::var_os("PATHEXT").map_or_else(
        || {
            [".COM", ".EXE", ".BAT", ".CMD"]
                .iter()
                .map(OsString::from)
                .collect()
        },
        |raw| {
            raw.to_string_lossy()
                .split(';')
                .filter(|value| !value.is_empty())
                .map(OsString::from)
                .collect()
        },
    )
}

fn is_file(path: &Path) -> bool {
    path.metadata().is_ok_and(|metadata| metadata.is_file())
}

#[cfg(test)]
mod tests {
    use super::{Clipboard, ClipboardCommand, ClipboardError, detect_backend};

    #[test]
    fn detect_backend_prefers_wl_copy_on_linux() {
        let backend = detect_backend("linux", &mut |program| {
            matches!(program, "wl-copy" | "xclip")
        });
        assert_eq!(
            backend,
            Some(ClipboardCommand {
                program: "wl-copy",
                args: &[],
            })
        );
    }

    #[test]
    fn detect_backend_falls_back_to_xsel_on_linux() {
        let backend = detect_backend("linux", &mut |program| program == "xsel");
        assert_eq!(
            backend,
            Some(ClipboardCommand {
                program: "xsel",
                args: &["--clipboard", "--input"],
            })
        );
    }

    #[test]
    fn clipboard_new_errors_when_backend_missing() {
        let error = Clipboard::for_os("linux", |_| false).expect_err("backend should be missing");
        assert_eq!(
            error,
            ClipboardError::BackendUnavailable {
                os: "linux".to_string(),
            }
        );
    }
}
