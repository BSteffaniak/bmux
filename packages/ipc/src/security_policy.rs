//! Explicit Unix transport policy. Defaults preserve private, same-user IPC.

/// Required owner of a socket directory or connected peer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IdentityPolicy {
    /// Require the process's effective user.
    #[default]
    EffectiveUser,
    /// Require a particular Unix user ID.
    User(u32),
    /// Do not check identity. Other access controls must supply authorization.
    Unrestricted,
}

/// Which directory symlinks the descriptor-relative walk may follow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SymlinkPolicy {
    /// Follow only root-owned ancestor aliases; never the immediate parent.
    #[default]
    SystemAncestors,
    /// Reject all symlinks.
    Reject,
    /// Follow symlinks, including the immediate parent. The consumer trusts redirection.
    Follow,
}

/// Unix filesystem and peer policy for a local IPC transport.
///
/// Relaxations are explicit and never inferred from environment variables.
/// Existing directories are never chmod/chowned. Permission values contain only
/// Unix permission bits, not file-type or special bits. Relaxing path checks can
/// permit endpoint substitution; disabling peer checks requires another source
/// of authentication. This policy does not change endpoint cleanup ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnixSecurityPolicy {
    /// Required owner of the immediate socket parent.
    pub parent_owner: IdentityPolicy,
    /// Exact required parent mode; `None` disables mode validation.
    pub parent_mode: Option<u32>,
    /// Permissions requested for newly created directories (subject to umask).
    pub directory_mode: u32,
    /// Permissions applied to the socket after binding.
    pub socket_mode: u32,
    /// Whether missing directories may be created.
    pub create_parents: bool,
    /// Directory symlink handling.
    pub symlinks: SymlinkPolicy,
    /// Require ancestors owned by root/current user and protected from replacement.
    pub trusted_ancestors: bool,
    /// Identity required on accepted streams and outgoing connections.
    pub peer: IdentityPolicy,
}

impl Default for UnixSecurityPolicy {
    fn default() -> Self {
        Self::strict()
    }
}

impl UnixSecurityPolicy {
    /// Private directories and sockets with same-user peer authentication.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            parent_owner: IdentityPolicy::EffectiveUser,
            parent_mode: Some(0o700),
            directory_mode: 0o700,
            socket_mode: 0o600,
            create_parents: true,
            symlinks: SymlinkPolicy::SystemAncestors,
            trusted_ancestors: true,
            peer: IdentityPolicy::EffectiveUser,
        }
    }

    /// Prepare and validate a Unix socket parent before caller-owned stale-name recovery.
    /// Never removes a socket or repairs existing directory permissions.
    ///
    /// # Errors
    /// Returns an error for invalid policy, missing/insecure parents, or filesystem failure.
    #[cfg(unix)]
    pub fn prepare_parent(self, path: &std::path::Path) -> std::io::Result<()> {
        super::unix_security::prepare_parent_with_policy(path, self)
    }

    pub(super) fn validate(self) -> std::io::Result<()> {
        if [
            Some(self.directory_mode),
            Some(self.socket_mode),
            self.parent_mode,
        ]
        .into_iter()
        .flatten()
        .any(|mode| mode & !0o777 != 0)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "IPC policy modes must contain only permission bits",
            ));
        }
        Ok(())
    }
}
