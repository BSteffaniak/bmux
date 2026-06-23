#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::env;
use std::ffi::OsString;
use std::io::{Cursor, Write};
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
    #[error("clipboard backend '{backend}' failed: {message}")]
    BackendFailed { backend: String, message: String },
    #[error("clipboard command '{program}' failed: {message}")]
    CommandFailed { program: String, message: String },
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
    /// Returns [`ClipboardError::CommandFailed`] if the clipboard command
    /// cannot be spawned, stdin writing fails, or the command exits with a
    /// non-zero status.
    pub fn copy_text(&self, text: &str) -> Result<(), ClipboardError> {
        let mut child = Command::new(self.command.program)
            .args(self.command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| ClipboardError::CommandFailed {
                program: self.command.program.to_string(),
                message: error.to_string(),
            })?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(text.as_bytes())
                .and_then(|()| stdin.flush())
                .map_err(|error| ClipboardError::CommandFailed {
                    program: self.command.program.to_string(),
                    message: error.to_string(),
                })?;
        }

        let output = child
            .wait_with_output()
            .map_err(|error| ClipboardError::CommandFailed {
                program: self.command.program.to_string(),
                message: error.to_string(),
            })?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(ClipboardError::CommandFailed {
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
/// is found, or [`ClipboardError::BackendFailed`] / [`ClipboardError::CommandFailed`] if copying fails.
pub fn copy_text(text: &str) -> Result<(), ClipboardError> {
    match arboard_clipboard() {
        Ok(mut clipboard) => {
            clipboard
                .set_text(text.to_string())
                .map_err(|error| ClipboardError::BackendFailed {
                    backend: "arboard".to_string(),
                    message: error.to_string(),
                })
        }
        Err(_) => Clipboard::new()?.copy_text(text),
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardPayload {
    Text(String),
    ImagePng(Vec<u8>),
}

impl ClipboardPayload {
    #[must_use]
    pub const fn mime(&self) -> &'static str {
        match self {
            Self::Text(_) => "text/plain",
            Self::ImagePng(_) => "image/png",
        }
    }

    #[must_use]
    pub const fn bytes_len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::ImagePng(bytes) => bytes.len(),
        }
    }
}

/// Read the current system clipboard, preferring PNG image data when available.
///
/// # Errors
///
/// Returns [`ClipboardError`] if no supported clipboard backend is available or
/// the backend command fails.
pub fn read_payload(prefer_image: bool) -> Result<ClipboardPayload, ClipboardError> {
    tracing::debug!(
        bmux.component = "clipboard.remote_sync",
        prefer_image,
        "clipboard payload read started"
    );
    if prefer_image {
        match read_arboard_image() {
            Ok(Some(payload)) => {
                tracing::debug!(
                    bmux.component = "clipboard.remote_sync",
                    mime = payload.mime(),
                    bytes_len = payload.bytes_len(),
                    source = "arboard_image",
                    "clipboard payload selected"
                );
                return Ok(payload);
            }
            Ok(None) => tracing::debug!(
                bmux.component = "clipboard.remote_sync",
                stage = "arboard_image",
                "clipboard image payload unavailable"
            ),
            Err(error) => tracing::debug!(
                bmux.component = "clipboard.remote_sync",
                stage = "arboard_image",
                error = %error,
                "clipboard image payload read failed"
            ),
        }
        match read_image_file_reference_payload() {
            Ok(Some(payload)) => {
                tracing::debug!(
                    bmux.component = "clipboard.remote_sync",
                    mime = payload.mime(),
                    bytes_len = payload.bytes_len(),
                    source = "file_reference",
                    "clipboard payload selected"
                );
                return Ok(payload);
            }
            Ok(None) => tracing::debug!(
                bmux.component = "clipboard.remote_sync",
                stage = "file_reference",
                "clipboard image file-reference payload unavailable"
            ),
            Err(error) => tracing::debug!(
                bmux.component = "clipboard.remote_sync",
                stage = "file_reference",
                error = %error,
                "clipboard image file-reference payload read failed"
            ),
        }
    }
    match read_text() {
        Ok(text) => {
            tracing::debug!(
                bmux.component = "clipboard.remote_sync",
                mime = "text/plain",
                bytes_len = text.len(),
                source = "text",
                "clipboard payload selected"
            );
            Ok(ClipboardPayload::Text(text))
        }
        Err(error) => {
            tracing::debug!(
                bmux.component = "clipboard.remote_sync",
                stage = "text",
                error = %error,
                "clipboard text payload read failed"
            );
            Err(error)
        }
    }
}

/// Read text from the system clipboard.
///
/// # Errors
///
/// Returns [`ClipboardError`] if no supported read backend is available or the
/// backend command fails.
pub fn read_text() -> Result<String, ClipboardError> {
    arboard_clipboard().map_or_else(
        |error| {
            tracing::debug!(
                bmux.component = "clipboard.remote_sync",
                stage = "arboard_text_init",
                error = %error,
                "arboard text read initialization failed; falling back to command backend"
            );
            read_text_command()
        },
        |mut clipboard| {
            tracing::debug!(
                bmux.component = "clipboard.remote_sync",
                stage = "arboard_text",
                "clipboard text read attempted"
            );
            clipboard
                .get_text()
                .inspect(|text| {
                    tracing::debug!(
                        bmux.component = "clipboard.remote_sync",
                        stage = "arboard_text",
                        bytes_len = text.len(),
                        "clipboard text read succeeded"
                    );
                })
                .map_err(|error| ClipboardError::BackendFailed {
                    backend: "arboard".to_string(),
                    message: error.to_string(),
                })
        },
    )
}

fn read_text_command() -> Result<String, ClipboardError> {
    let (program, args): (&str, &[&str]) = match env::consts::OS {
        "macos" => ("pbpaste", &[]),
        "linux" if command_exists("wl-paste") => ("wl-paste", &["--no-newline"]),
        "linux" if command_exists("xclip") => ("xclip", &["-selection", "clipboard", "-out"]),
        "linux" if command_exists("xsel") => ("xsel", &["--clipboard", "--output"]),
        os => return Err(ClipboardError::BackendUnavailable { os: os.to_string() }),
    };
    tracing::debug!(
        bmux.component = "clipboard.remote_sync",
        stage = "command_text",
        program,
        "clipboard command text read attempted"
    );
    let output = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| ClipboardError::CommandFailed {
            program: program.to_string(),
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(ClipboardError::CommandFailed {
            program: program.to_string(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    String::from_utf8(output.stdout).map_err(|error| ClipboardError::CommandFailed {
        program: program.to_string(),
        message: error.to_string(),
    })
}

/// Copy PNG image bytes to the system clipboard when supported.
///
/// # Errors
///
/// Returns [`ClipboardError`] if image clipboard writes are unsupported or fail.
pub fn copy_png_image(bytes: &[u8]) -> Result<(), ClipboardError> {
    let mut clipboard = arboard_clipboard()?;
    let image = image::load_from_memory(bytes).map_err(|error| ClipboardError::BackendFailed {
        backend: "image".to_string(),
        message: error.to_string(),
    })?;
    let rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();
    clipboard
        .set_image(arboard::ImageData {
            width: usize::try_from(width).unwrap_or(usize::MAX),
            height: usize::try_from(height).unwrap_or(usize::MAX),
            bytes: rgba.into_raw().into(),
        })
        .map_err(|error| ClipboardError::BackendFailed {
            backend: "arboard".to_string(),
            message: error.to_string(),
        })
}

fn arboard_clipboard() -> Result<arboard::Clipboard, ClipboardError> {
    arboard::Clipboard::new().map_err(|error| ClipboardError::BackendFailed {
        backend: "arboard".to_string(),
        message: error.to_string(),
    })
}

fn read_arboard_image() -> Result<Option<ClipboardPayload>, ClipboardError> {
    tracing::debug!(
        bmux.component = "clipboard.remote_sync",
        stage = "arboard_image",
        "clipboard arboard image read attempted"
    );
    let mut clipboard = arboard_clipboard()?;
    match clipboard.get_image() {
        Ok(image) => {
            encode_arboard_image_png(image).map(|bytes| Some(ClipboardPayload::ImagePng(bytes)))
        }
        Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(error) => Err(ClipboardError::BackendFailed {
            backend: "arboard".to_string(),
            message: error.to_string(),
        }),
    }
}

fn encode_arboard_image_png(image: arboard::ImageData<'_>) -> Result<Vec<u8>, ClipboardError> {
    let width = u32::try_from(image.width).map_err(|error| ClipboardError::BackendFailed {
        backend: "arboard".to_string(),
        message: error.to_string(),
    })?;
    let height = u32::try_from(image.height).map_err(|error| ClipboardError::BackendFailed {
        backend: "arboard".to_string(),
        message: error.to_string(),
    })?;
    let rgba =
        image::RgbaImage::from_raw(width, height, image.bytes.into_owned()).ok_or_else(|| {
            ClipboardError::BackendFailed {
                backend: "arboard".to_string(),
                message: "invalid RGBA clipboard image buffer".to_string(),
            }
        })?;
    let dynamic = image::DynamicImage::ImageRgba8(rgba);
    let mut output = Cursor::new(Vec::new());
    dynamic
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|error| ClipboardError::BackendFailed {
            backend: "image".to_string(),
            message: error.to_string(),
        })?;
    Ok(output.into_inner())
}

fn read_image_file_reference_payload() -> Result<Option<ClipboardPayload>, ClipboardError> {
    tracing::debug!(
        bmux.component = "clipboard.remote_sync",
        stage = "file_reference",
        "clipboard image file-reference fallback attempted"
    );
    if let Some(path) = read_arboard_image_path_text()? {
        tracing::debug!(
            bmux.component = "clipboard.remote_sync",
            stage = "file_reference",
            path = %path.display(),
            "clipboard image file-reference candidate found"
        );
        return read_image_file_payload(&path);
    }
    Ok(None)
}

fn read_arboard_image_path_text() -> Result<Option<PathBuf>, ClipboardError> {
    let mut clipboard = arboard_clipboard()?;
    let text = match clipboard.get_text() {
        Ok(text) => text,
        Err(arboard::Error::ContentNotAvailable) => {
            tracing::debug!(
                bmux.component = "clipboard.remote_sync",
                stage = "file_reference_text",
                "clipboard text unavailable while probing file reference"
            );
            return Ok(None);
        }
        Err(error) => {
            return Err(ClipboardError::BackendFailed {
                backend: "arboard".to_string(),
                message: error.to_string(),
            });
        }
    };
    let trimmed = text.trim();
    if let Some(path) = trimmed.strip_prefix("file://") {
        let path = PathBuf::from(percent_decode_file_url_path(path));
        tracing::debug!(
            bmux.component = "clipboard.remote_sync",
            stage = "file_reference_text",
            path = %path.display(),
            "clipboard text contains file URL"
        );
        return Ok(Some(path));
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() && path.is_file() {
        tracing::debug!(
            bmux.component = "clipboard.remote_sync",
            stage = "file_reference_text",
            path = %path.display(),
            "clipboard text contains absolute file path"
        );
        return Ok(Some(path));
    }
    Ok(None)
}

fn read_image_file_payload(path: &Path) -> Result<Option<ClipboardPayload>, ClipboardError> {
    tracing::debug!(
        bmux.component = "clipboard.remote_sync",
        stage = "file_reference_read",
        path = %path.display(),
        "clipboard image file read attempted"
    );
    let bytes = std::fs::read(path).map_err(|error| ClipboardError::BackendFailed {
        backend: "file".to_string(),
        message: error.to_string(),
    })?;
    if is_png(&bytes) {
        tracing::debug!(
            bmux.component = "clipboard.remote_sync",
            stage = "file_reference_read",
            path = %path.display(),
            bytes_len = bytes.len(),
            "clipboard image file is PNG"
        );
        return Ok(Some(ClipboardPayload::ImagePng(bytes)));
    }
    let Ok(image) = image::load_from_memory(&bytes) else {
        tracing::debug!(
            bmux.component = "clipboard.remote_sync",
            stage = "file_reference_read",
            path = %path.display(),
            bytes_len = bytes.len(),
            "clipboard file-reference payload is not a supported image"
        );
        return Ok(None);
    };
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|error| ClipboardError::BackendFailed {
            backend: "image".to_string(),
            message: error.to_string(),
        })?;
    Ok(Some(ClipboardPayload::ImagePng(output.into_inner())))
}

fn percent_decode_file_url_path(path: &str) -> String {
    let mut bytes = Vec::with_capacity(path.len());
    let raw = path.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%'
            && index + 2 < raw.len()
            && let Ok(value) = u8::from_str_radix(&path[index + 1..index + 3], 16)
        {
            bytes.push(value);
            index += 3;
            continue;
        }
        bytes.push(raw[index]);
        index += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

#[cfg(test)]
mod tests {
    use super::{
        Clipboard, ClipboardCommand, ClipboardError, detect_backend, is_png,
        percent_decode_file_url_path,
    };

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

    #[test]
    fn percent_decode_file_url_path_decodes_escaped_bytes() {
        assert_eq!(
            percent_decode_file_url_path("/tmp/image%20one%23.png"),
            "/tmp/image one#.png"
        );
    }

    #[test]
    fn is_png_detects_png_magic() {
        assert!(is_png(b"\x89PNG\r\n\x1a\nrest"));
        assert!(!is_png(b"not png"));
    }
}
