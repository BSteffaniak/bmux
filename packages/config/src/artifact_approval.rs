//! Explicit host-owned artifact approvals. Never merged with ordinary configuration.
//!
//! See `artifact_approval/README.md` for the versioned authority and storage contract.
use crate::ConfigPaths;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub const APPROVAL_FILE: &str = "trusted-plugin-artifacts.toml";
const MAX_BYTES: usize = 1024 * 1024;
const MAX_APPROVALS: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum ArtifactApprovalError {
    #[error("artifact approval I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid artifact approval document: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("artifact approval authority rejected: {0}")]
    Invalid(String),
}

/// Generic observation labels, not execution authority or a grant.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactApproval {
    plugin_id: String,
    artifact_sha256: String,
    role: String,
    request_policy: String,
}
impl ArtifactApproval {
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }
    #[must_use]
    pub fn artifact_sha256(&self) -> &str {
        &self.artifact_sha256
    }
    #[must_use]
    pub fn role(&self) -> &str {
        &self.role
    }
    #[must_use]
    pub fn request_policy(&self) -> &str {
        &self.request_policy
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema_version: u32,
    runtime_dir: PathBuf,
    approvals: Vec<ArtifactApproval>,
}

/// Loaded only through protected host storage; not deserializable from plugin config.
#[derive(Debug)]
pub struct ArtifactApprovalStore {
    runtime_dir: PathBuf,
    approvals: Vec<ArtifactApproval>,
}
impl ArtifactApprovalStore {
    /// Load exactly the selected primary host configuration directory, without fallback.
    /// Absence is not an approval. No directories or files are created.
    ///
    /// # Errors
    /// Rejects insecure paths, unsupported platforms/schema versions, invalid/conflicting
    /// entries, oversized input and a document belonging to another runtime.
    pub fn load(paths: &ConfigPaths) -> Result<Option<Self>, ArtifactApprovalError> {
        let file = match open_protected(&paths.config_dir) {
            Ok(file) => file,
            Err(ArtifactApprovalError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(u64::try_from(MAX_BYTES).map_err(|_| invalid("invalid document bound"))? + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_BYTES {
            return Err(invalid("document exceeds 1 MiB"));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid("document is not UTF-8"))?;
        Self::parse_for_runtime(text, &paths.runtime_dir).map(Some)
    }

    fn parse_for_runtime(text: &str, runtime: &Path) -> Result<Self, ArtifactApprovalError> {
        let mut document: Document = toml::from_str(text)?;
        if document.schema_version != 1 {
            return Err(invalid("unsupported schema_version"));
        }
        if !runtime.is_absolute()
            || document.runtime_dir != runtime
            || runtime.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err(invalid(
                "document does not belong to selected absolute runtime directory",
            ));
        }
        if document.approvals.len() > MAX_APPROVALS {
            return Err(invalid("too many approvals"));
        }
        let mut identities = BTreeSet::new();
        for approval in &mut document.approvals {
            if !label(&approval.plugin_id)
                || !label(&approval.request_policy)
                || !matches!(approval.role.as_str(), "backend" | "frontend")
                || approval.artifact_sha256.len() != 64
                || !approval
                    .artifact_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(invalid(
                    "invalid approval identity, role, digest or policy label",
                ));
            }
            approval.artifact_sha256.make_ascii_lowercase();
            // One artifact per logical component and policy. Conflicting pins are never ordered.
            if !identities.insert((approval.plugin_id.clone(), approval.role.clone())) {
                return Err(invalid("duplicate/conflicting component approval"));
            }
        }
        Ok(Self {
            runtime_dir: document.runtime_dir,
            approvals: document.approvals,
        })
    }

    /// Host composition must validate every selector before issuing any grant.
    pub fn iter(&self) -> impl Iterator<Item = &ArtifactApproval> {
        self.approvals.iter()
    }

    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Exact artifact matching; this is configuration evidence, never a loader receipt.
    #[must_use]
    pub fn find(
        &self,
        plugin_id: &str,
        role: &str,
        observed_sha256: &str,
    ) -> Option<&ArtifactApproval> {
        self.approvals.iter().find(|approval| {
            approval.plugin_id == plugin_id
                && approval.role == role
                && approval
                    .artifact_sha256
                    .eq_ignore_ascii_case(observed_sha256)
        })
    }
}
fn invalid(message: &str) -> ArtifactApprovalError {
    ArtifactApprovalError::Invalid(message.into())
}
fn label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-/".contains(&byte)
        })
}

#[cfg(unix)]
fn open_protected(root: &Path) -> Result<File, ArtifactApprovalError> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
        path::Component,
    };
    fn check(file: &File, directory: bool) -> Result<(), ArtifactApprovalError> {
        const STICKY_BIT: u32 = 0o1000;
        let metadata = file.metadata()?;
        // SAFETY: geteuid takes no arguments and has no memory preconditions.
        let uid = unsafe { libc::geteuid() };
        let trusted_owner = metadata.uid() == uid || metadata.uid() == 0;
        let sticky_ancestor = directory && metadata.mode() & STICKY_BIT != 0;
        if !trusted_owner
            || (!sticky_ancestor && metadata.mode() & 0o022 != 0)
            || (directory && !metadata.is_dir())
            || (!directory && !metadata.is_file())
        {
            return Err(invalid(
                "path must be owned by current user/root and not writable by other users",
            ));
        }
        Ok(())
    }
    if !root.is_absolute() {
        return Err(invalid("host configuration directory must be absolute"));
    }
    let mut directory = File::open("/")?;
    check(&directory, true)?;
    let mut components = root.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(invalid(
                "host configuration directory must have normalized components",
            ));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| invalid("NUL in config path"))?;
        directory = open_config_component(&directory, &name, components.peek().is_none())?;
        check(&directory, true)?;
    }
    // Final authority directory cannot rely on sticky-directory protection.
    if directory.metadata()?.mode() & 0o022 != 0 {
        return Err(invalid(
            "host configuration directory is writable by other users",
        ));
    }
    let name = CString::new(APPROVAL_FILE).map_err(|_| invalid("invalid approval filename"))?;
    // SAFETY: directory is live, name is NUL-terminated; no symlink is followed.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful openat returned a new uniquely owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    check(&file, false)?;
    Ok(file)
}
/// Match IPC's `SystemAncestors` rule: only root-owned links before the final
/// directory may be followed, and only beneath a protected directory handle.
#[cfg(unix)]
fn open_config_component(
    parent: &File,
    name: &std::ffi::CStr,
    last: bool,
) -> Result<File, ArtifactApprovalError> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    };
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: parent is live, name is terminated, and stat is writable storage.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fstatat initialized stat on success.
    let stat = unsafe { stat.assume_init() };
    let system_alias = !last && stat.st_uid == 0 && stat.st_mode & libc::S_IFMT == libc::S_IFLNK;
    if system_alias && parent.metadata()?.mode() & 0o022 != 0 {
        return Err(invalid("system alias parent is writable by other users"));
    }
    let flags = libc::O_RDONLY
        | libc::O_DIRECTORY
        | libc::O_CLOEXEC
        | if system_alias { 0 } else { libc::O_NOFOLLOW };
    // SAFETY: parent/name are valid; only a trusted system ancestor may be followed.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returned a uniquely owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(not(unix))]
fn open_protected(root: &Path) -> Result<File, ArtifactApprovalError> {
    // Absence approves nothing, so it needs no protected-storage contract and
    // must not stop the host from starting. A present document still fails
    // closed until this platform's ownership/permission checks exist.
    if !root.is_absolute() {
        return Err(invalid("host configuration directory must be absolute"));
    }
    match std::fs::symlink_metadata(root.join(APPROVAL_FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Err(invalid(
            "protected artifact approvals are unsupported on this platform",
        )),
    }
}

#[cfg(test)]
#[path = "artifact_approval/tests.rs"]
mod tests;
