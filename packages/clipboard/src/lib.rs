#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::env;

#[cfg(feature = "native-clipboard")]
mod native;
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
        copy_with_commands(
            std::iter::once(self.command).chain(
                clipboard_commands(env::consts::OS)
                    .iter()
                    .copied()
                    .filter(|command| *command != self.command && command_exists(command.program)),
            ),
            Vec::new(),
            |command| run_copy_command(command, text),
        )
    }
}

fn run_copy_command(command: ClipboardCommand, text: &str) -> Result<(), ClipboardError> {
    let mut process = Command::new(command.program);
    process
        .args(command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if command.program == "wl-copy"
        && let Some(display) = recovered_wayland_display()
    {
        // Do not mutate the process environment: plugins and other threads share it.
        process.env("WAYLAND_DISPLAY", display);
    }
    let failed = |message| ClipboardError::CommandFailed {
        program: command.program.to_string(),
        message,
    };
    let mut child = process.spawn().map_err(|error| failed(error.to_string()))?;
    let write_result = child.stdin.take().map_or(Ok(()), |mut stdin| {
        stdin
            .write_all(text.as_bytes())
            .and_then(|()| stdin.flush())
    });
    // Always close stdin and reap the child, including after a broken pipe.
    let output = child
        .wait_with_output()
        .map_err(|error| failed(error.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(failed(if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stderr
        }));
    }
    write_result.map_err(|error| failed(error.to_string()))
}

fn copy_with_commands(
    commands: impl IntoIterator<Item = ClipboardCommand>,
    mut errors: Vec<String>,
    mut copy: impl FnMut(ClipboardCommand) -> Result<(), ClipboardError>,
) -> Result<(), ClipboardError> {
    for command in commands {
        match copy(command) {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(error.to_string()),
        }
    }
    if errors.is_empty() {
        Err(ClipboardError::BackendUnavailable {
            os: env::consts::OS.to_string(),
        })
    } else {
        Err(ClipboardError::BackendFailed {
            backend: "all".to_string(),
            message: errors.join("; "),
        })
    }
}

fn recovered_wayland_display() -> Option<OsString> {
    if env::var_os("WAYLAND_DISPLAY").is_some()
        || env::var_os("SSH_CONNECTION").is_some()
        || env::var_os("SSH_CLIENT").is_some()
        || env::var_os("SSH_TTY").is_some()
    {
        return None;
    }
    unique_wayland_display(Path::new(&env::var_os("XDG_RUNTIME_DIR")?))
}

#[cfg(unix)]
fn unique_wayland_display(runtime_dir: &Path) -> Option<OsString> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let owner = std::fs::metadata(runtime_dir).ok()?.uid();
    let mut display = None;
    for entry in std::fs::read_dir(runtime_dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix("wayland-")) else {
            continue;
        };
        if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
        if metadata.file_type().is_socket() && metadata.uid() == owner {
            // Ambiguity must not silently send clipboard contents to another desktop.
            if display.is_some() {
                return None;
            }
            display = Some(name);
        }
    }
    display
}

#[cfg(not(unix))]
fn unique_wayland_display(_runtime_dir: &Path) -> Option<OsString> {
    None
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
    #[cfg(feature = "native-clipboard")]
    let result = native::copy_text(text);
    #[cfg(not(feature = "native-clipboard"))]
    let result = arboard_clipboard().and_then(|mut clipboard| {
        clipboard
            .set_text(text.to_string())
            .map_err(|error| ClipboardError::BackendFailed {
                backend: "arboard".to_string(),
                message: error.to_string(),
            })
    });
    match result {
        Ok(()) => Ok(()),
        Err(error) => copy_with_commands(
            clipboard_commands(env::consts::OS)
                .iter()
                .copied()
                .filter(|command| command_exists(command.program)),
            vec![error.to_string()],
            |command| run_copy_command(command, text),
        ),
    }
}

fn detect_backend<F>(os: &str, exists: &mut F) -> Option<ClipboardCommand>
where
    F: FnMut(&str) -> bool,
{
    clipboard_commands(os)
        .iter()
        .copied()
        .find(|candidate| exists(candidate.program))
}

fn clipboard_commands(os: &str) -> &'static [ClipboardCommand] {
    match os {
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
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardPayloadSource {
    ArboardImage,
    FileReference,
    Text,
}

impl ClipboardPayloadSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ArboardImage => "arboard_image",
            Self::FileReference => "file_reference",
            Self::Text => "text",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardReadResult {
    pub payload: ClipboardPayload,
    pub source: ClipboardPayloadSource,
    pub report: ClipboardReadReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardReadReport {
    pub prefer_image: bool,
    pub attempts: Vec<ClipboardReadAttempt>,
}

impl ClipboardReadReport {
    #[must_use]
    pub fn attempts_summary(&self) -> String {
        self.attempts
            .iter()
            .map(|attempt| {
                format!(
                    "{}:{}:{}",
                    attempt.backend,
                    attempt.operation,
                    attempt.outcome.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardReadAttempt {
    pub backend: &'static str,
    pub operation: &'static str,
    pub outcome: ClipboardReadAttemptOutcome,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardReadAttemptOutcome {
    Selected,
    Unavailable,
    Failed,
}

impl ClipboardReadAttemptOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
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
    read_payload_detailed(prefer_image).map(|result| result.payload)
}

/// Read the current system clipboard with structured backend-attempt diagnostics.
///
/// # Errors
///
/// Returns [`ClipboardError`] if no supported clipboard backend is available or
/// the backend command fails.
#[allow(clippy::too_many_lines)]
pub fn read_payload_detailed(prefer_image: bool) -> Result<ClipboardReadResult, ClipboardError> {
    let mut report = ClipboardReadReport {
        prefer_image,
        attempts: Vec::new(),
    };
    tracing::debug!(
        bmux.component = "clipboard.remote_sync",
        prefer_image,
        "clipboard payload read started"
    );
    if prefer_image {
        match read_arboard_image() {
            Ok(Some(payload)) => {
                report.attempts.push(ClipboardReadAttempt {
                    backend: "arboard",
                    operation: "get_image",
                    outcome: ClipboardReadAttemptOutcome::Selected,
                    error: None,
                });
                return Ok(clipboard_read_result(
                    payload,
                    ClipboardPayloadSource::ArboardImage,
                    report,
                ));
            }
            Ok(None) => {
                report.attempts.push(ClipboardReadAttempt {
                    backend: "arboard",
                    operation: "get_image",
                    outcome: ClipboardReadAttemptOutcome::Unavailable,
                    error: None,
                });
                tracing::debug!(
                    bmux.component = "clipboard.remote_sync",
                    backend = "arboard",
                    operation = "get_image",
                    outcome = "unavailable",
                    "clipboard backend attempt completed"
                );
            }
            Err(error) => {
                report.attempts.push(ClipboardReadAttempt {
                    backend: "arboard",
                    operation: "get_image",
                    outcome: ClipboardReadAttemptOutcome::Failed,
                    error: Some(error.to_string()),
                });
                tracing::debug!(
                    bmux.component = "clipboard.remote_sync",
                    backend = "arboard",
                    operation = "get_image",
                    outcome = "failed",
                    error = %error,
                    "clipboard backend attempt completed"
                );
            }
        }
        match read_image_file_reference_payload() {
            Ok(Some(payload)) => {
                report.attempts.push(ClipboardReadAttempt {
                    backend: "arboard",
                    operation: "get_text_file_reference",
                    outcome: ClipboardReadAttemptOutcome::Selected,
                    error: None,
                });
                return Ok(clipboard_read_result(
                    payload,
                    ClipboardPayloadSource::FileReference,
                    report,
                ));
            }
            Ok(None) => {
                report.attempts.push(ClipboardReadAttempt {
                    backend: "arboard",
                    operation: "get_text_file_reference",
                    outcome: ClipboardReadAttemptOutcome::Unavailable,
                    error: None,
                });
                tracing::debug!(
                    bmux.component = "clipboard.remote_sync",
                    backend = "arboard",
                    operation = "get_text_file_reference",
                    outcome = "unavailable",
                    "clipboard backend attempt completed"
                );
            }
            Err(error) => {
                report.attempts.push(ClipboardReadAttempt {
                    backend: "arboard",
                    operation: "get_text_file_reference",
                    outcome: ClipboardReadAttemptOutcome::Failed,
                    error: Some(error.to_string()),
                });
                tracing::debug!(
                    bmux.component = "clipboard.remote_sync",
                    backend = "arboard",
                    operation = "get_text_file_reference",
                    outcome = "failed",
                    error = %error,
                    "clipboard backend attempt completed"
                );
            }
        }
    }
    match read_text() {
        Ok(text) => {
            let payload = ClipboardPayload::Text(text);
            report.attempts.push(ClipboardReadAttempt {
                backend: "arboard_or_command",
                operation: "get_text",
                outcome: ClipboardReadAttemptOutcome::Selected,
                error: None,
            });
            Ok(clipboard_read_result(
                payload,
                ClipboardPayloadSource::Text,
                report,
            ))
        }
        Err(error) => {
            report.attempts.push(ClipboardReadAttempt {
                backend: "arboard_or_command",
                operation: "get_text",
                outcome: ClipboardReadAttemptOutcome::Failed,
                error: Some(error.to_string()),
            });
            tracing::debug!(
                bmux.component = "clipboard.remote_sync",
                backend = "arboard_or_command",
                operation = "get_text",
                outcome = "failed",
                error = %error,
                attempts = %report.attempts_summary(),
                "clipboard read completed without supported payload"
            );
            Err(error)
        }
    }
}

fn clipboard_read_result(
    payload: ClipboardPayload,
    source: ClipboardPayloadSource,
    report: ClipboardReadReport,
) -> ClipboardReadResult {
    tracing::debug!(
        bmux.component = "clipboard.remote_sync",
        selected_source = source.as_str(),
        selected_mime = payload.mime(),
        bytes_len = payload.bytes_len(),
        attempts = %report.attempts_summary(),
        "clipboard read completed"
    );
    ClipboardReadResult {
        payload,
        source,
        report,
    }
}

/// Read text from the system clipboard.
///
/// # Errors
///
/// Returns [`ClipboardError`] if no supported read backend is available or the
/// backend command fails.
pub fn read_text() -> Result<String, ClipboardError> {
    #[cfg(feature = "native-clipboard")]
    {
        match native::read_text() {
            Ok(Some(text)) => Ok(text),
            Ok(None) => Err(ClipboardError::BackendFailed {
                backend: "native".to_string(),
                message: "clipboard has no text format".to_string(),
            }),
            Err(error) => {
                tracing::debug!(error = %error, "native text read failed; trying command backend");
                read_text_command()
            }
        }
    }
    #[cfg(not(feature = "native-clipboard"))]
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
    #[cfg(not(feature = "native-clipboard"))]
    let mut clipboard = arboard_clipboard()?;
    let image = image::load_from_memory(bytes).map_err(|error| ClipboardError::BackendFailed {
        backend: "image".to_string(),
        message: error.to_string(),
    })?;
    let rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();
    #[cfg(feature = "native-clipboard")]
    {
        native::copy_image(&native::NativeImage {
            width: usize::try_from(width).unwrap_or(usize::MAX),
            height: usize::try_from(height).unwrap_or(usize::MAX),
            bytes: rgba.into_raw(),
        })
    }
    #[cfg(not(feature = "native-clipboard"))]
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

#[cfg(not(feature = "native-clipboard"))]
fn arboard_clipboard() -> Result<arboard::Clipboard, ClipboardError> {
    arboard::Clipboard::new().map_err(|error| ClipboardError::BackendFailed {
        backend: "arboard".to_string(),
        message: error.to_string(),
    })
}

#[cfg(feature = "native-clipboard")]
fn read_arboard_image() -> Result<Option<ClipboardPayload>, ClipboardError> {
    native::read_image()?
        .map(|image| encode_native_image_png(image).map(ClipboardPayload::ImagePng))
        .transpose()
}

#[cfg(feature = "native-clipboard")]
fn encode_native_image_png(image: native::NativeImage) -> Result<Vec<u8>, ClipboardError> {
    let width = u32::try_from(image.width).map_err(|error| ClipboardError::BackendFailed {
        backend: "native".to_string(),
        message: error.to_string(),
    })?;
    let height = u32::try_from(image.height).map_err(|error| ClipboardError::BackendFailed {
        backend: "native".to_string(),
        message: error.to_string(),
    })?;
    let rgba = image::RgbaImage::from_raw(width, height, image.bytes).ok_or_else(|| {
        ClipboardError::BackendFailed {
            backend: "native".to_string(),
            message: "invalid RGBA clipboard image buffer".to_string(),
        }
    })?;
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|error| ClipboardError::BackendFailed {
            backend: "image".to_string(),
            message: error.to_string(),
        })?;
    Ok(output.into_inner())
}

#[cfg(not(feature = "native-clipboard"))]
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

#[cfg(not(feature = "native-clipboard"))]
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
    #[cfg(feature = "native-clipboard")]
    let Some(text) = native::read_text()? else {
        return Ok(None);
    };
    #[cfg(not(feature = "native-clipboard"))]
    let text = read_arboard_file_reference_text()?;
    #[cfg(not(feature = "native-clipboard"))]
    let Some(text) = text else {
        return Ok(None);
    };
    Ok(image_path_from_text(&text))
}

#[cfg(not(feature = "native-clipboard"))]
fn read_arboard_file_reference_text() -> Result<Option<String>, ClipboardError> {
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
    Ok(Some(text))
}

fn image_path_from_text(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if let Some(path) = trimmed.strip_prefix("file://") {
        let path = PathBuf::from(percent_decode_file_url_path(path));
        tracing::debug!(
            bmux.component = "clipboard.remote_sync",
            stage = "file_reference_text",
            path = %path.display(),
            "clipboard text contains file URL"
        );
        return Some(path);
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() && path.is_file() {
        tracing::debug!(
            bmux.component = "clipboard.remote_sync",
            stage = "file_reference_text",
            path = %path.display(),
            "clipboard text contains absolute file path"
        );
        return Some(path);
    }
    None
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
        Clipboard, ClipboardCommand, ClipboardError, clipboard_commands, copy_with_commands,
        detect_backend, image_path_from_text, is_png, percent_decode_file_url_path,
    };
    #[cfg(feature = "native-clipboard")]
    use super::{encode_native_image_png, native};
    use std::path::PathBuf;

    #[test]
    fn copy_retries_failed_backends_and_stops_after_success() {
        let mut attempted = Vec::new();
        copy_with_commands(
            clipboard_commands("linux").iter().copied(),
            vec![],
            |command| {
                attempted.push(command.program);
                if command.program == "xclip" {
                    Ok(())
                } else {
                    Err(ClipboardError::CommandFailed {
                        program: command.program.to_string(),
                        message: "no display".to_string(),
                    })
                }
            },
        )
        .expect("second backend should succeed");
        assert_eq!(attempted, ["wl-copy", "xclip"]);
    }

    #[test]
    fn copy_reports_every_failed_backend_including_native_failure() {
        let error = copy_with_commands(
            clipboard_commands("linux").iter().copied(),
            vec!["native write failed".to_string()],
            |command| {
                Err(ClipboardError::CommandFailed {
                    program: command.program.to_string(),
                    message: "unavailable".to_string(),
                })
            },
        )
        .expect_err("all backends fail")
        .to_string();
        for backend in ["native write failed", "wl-copy", "xclip", "xsel"] {
            assert!(error.contains(backend));
        }
    }

    #[cfg(unix)]
    #[test]
    fn wayland_discovery_requires_one_real_socket() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().expect("temporary runtime directory");
        let path = directory.path();
        assert_eq!(super::unique_wayland_display(path), None);
        std::fs::write(path.join("wayland-0"), "not a socket").unwrap();
        let _unrelated = UnixListener::bind(path.join("wayland-1-swww-daemon.sock")).unwrap();
        assert_eq!(super::unique_wayland_display(path), None);
        let _display = UnixListener::bind(path.join("wayland-1")).unwrap();
        assert_eq!(
            super::unique_wayland_display(path),
            Some("wayland-1".into())
        );
        let _other_display = UnixListener::bind(path.join("wayland-2")).unwrap();
        assert_eq!(super::unique_wayland_display(path), None);
    }

    #[cfg(unix)]
    #[test]
    fn wayland_discovery_does_not_follow_symlinks() {
        use std::os::unix::{fs::symlink, net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let _socket = UnixListener::bind(directory.path().join("other-socket")).unwrap();
        symlink("other-socket", directory.path().join("wayland-1")).unwrap();
        assert_eq!(super::unique_wayland_display(directory.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn copy_recovers_missing_display_in_isolated_process() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};

        const OUTPUT: &str = "BMUX_CLIPBOARD_TEST_OUTPUT";
        if std::env::var_os(OUTPUT).is_some() {
            if std::env::var_os("BMUX_CLIPBOARD_TEST_NO_DISCOVERY").is_some() {
                assert!(super::recovered_wayland_display().is_none());
            } else {
                // Exercise Wayland recovery explicitly even on macOS, where
                // copy_text correctly selects pbcopy instead of this fixture.
                super::run_copy_command(
                    super::ClipboardCommand {
                        program: "wl-copy",
                        args: &[],
                    },
                    "scrollback text\n",
                )
                .expect("recovered clipboard command");
                assert!(std::env::var_os("WAYLAND_DISPLAY").is_none());
            }
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let _socket = UnixListener::bind(directory.path().join("wayland-1")).unwrap();
        let command = directory.path().join("wl-copy");
        std::fs::write(
            &command,
            "#!/bin/sh\nread -r text\nprintf '%s\\n%s' \"$WAYLAND_DISPLAY\" \"$text\" > \"$BMUX_CLIPBOARD_TEST_OUTPUT\"\n",
        ).unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = directory.path().join("copied");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "tests::copy_recovers_missing_display_in_isolated_process",
            ])
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("SSH_CONNECTION")
            .env_remove("SSH_CLIENT")
            .env_remove("SSH_TTY")
            .env("XDG_RUNTIME_DIR", directory.path())
            .env("PATH", directory.path())
            .env(OUTPUT, &output);
        assert!(child.status().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "wayland-1\nscrollback text"
        );
        child.env("BMUX_CLIPBOARD_TEST_NO_DISCOVERY", "1");
        for key in ["WAYLAND_DISPLAY", "SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"] {
            child.env(key, "explicit");
            assert!(child.status().unwrap().success());
            child.env_remove(key);
        }
    }

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
    fn image_file_reference_text_preserves_current_path_rules() {
        assert_eq!(
            image_path_from_text(" file:///tmp/a%20b.png \n"),
            Some(PathBuf::from("/tmp/a b.png"))
        );
        assert_eq!(image_path_from_text("relative.png"), None);
        assert_eq!(image_path_from_text(""), None);
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("image.png");
        std::fs::write(&file, b"fixture").unwrap();
        assert_eq!(image_path_from_text(file.to_str().unwrap()), Some(file));
        assert_eq!(
            image_path_from_text(directory.path().to_str().unwrap()),
            None
        );
    }

    #[cfg(feature = "native-clipboard")]
    #[test]
    fn native_rgba_png_conversion_preserves_pixels() {
        let pixels = vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 1, 2, 3, 255];
        let png = encode_native_image_png(native::NativeImage {
            width: 2,
            height: 2,
            bytes: pixels.clone(),
        })
        .unwrap();
        let decoded = image::load_from_memory(&png).unwrap().into_rgba8();
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(decoded.into_raw(), pixels);
        assert!(
            encode_native_image_png(native::NativeImage {
                width: 2,
                height: 2,
                bytes: vec![]
            })
            .is_err()
        );
    }

    #[test]
    fn is_png_detects_png_magic() {
        assert!(is_png(b"\x89PNG\r\n\x1a\nrest"));
        assert!(!is_png(b"not png"));
    }
}
