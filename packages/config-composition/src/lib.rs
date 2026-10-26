#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(unsafe_op_in_unsafe_fn)]
//! Independent configuration capture, pure composition, and typed profile inheritance.
//! Configuration values and captured bytes are sensitive: Debug/Display redact them.
//! Fallible operations return structured [`ErrorKind`] codes, never raw parser errors.
//! [`Loader::builder_for`] enables only optional XDG user configuration, not project scans.
//! Public recursive value trees are trusted owned data; see [`Document::try_new`] for
//! checked construction and [`discard_value`] for stack-safe disposal of deep trees.
//! See `API.md` for ordering, authority, resource bounds, and platform security contracts.
mod capture;
mod profile;
mod value;
pub use capture::*;
pub use profile::*;
pub use value::*;

/// Structured diagnostics never include parser messages or configuration contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Required explicit context is absent or invalid (for example relative cwd).
    InvalidContext,
    /// Builder policy is invalid, ambiguous, or exceeds supported recursion bounds.
    InvalidPolicy,
    /// A required filesystem object does not exist.
    Missing,
    /// A filesystem operation failed; raw operating-system details are discarded.
    Io,
    /// The requested object is not a regular file.
    NotRegular,
    /// A symlink was encountered under a rejecting policy.
    Symlink,
    /// A canonical filesystem path escaped the configured boundary.
    Boundary,
    /// A canonical file was encountered twice under a rejecting policy.
    Duplicate,
    /// A configured resource or safe conversion depth bound was exceeded.
    Limit,
    /// Captured bytes are not UTF-8 or the document codec rejected their syntax.
    Parse,
    /// Serde conversion or codec encoding rejected the value or target type.
    Decode,
    /// Merge encountered differing value variants under a rejecting policy.
    TypeMismatch,
    /// A selected profile or one of its parents does not exist.
    UnknownProfile,
    /// Profile inheritance contains an active-path cycle.
    ProfileCycle,
}
/// Secret-safe failure with optional source identity, not a path or offending value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    /// Stable machine-readable error category.
    pub kind: ErrorKind,
    /// Associated capture/merge source when available.
    pub source: Option<SourceId>,
}
impl Error {
    pub(crate) fn new(kind: ErrorKind) -> Self {
        Self { kind, source: None }
    }
    pub(crate) fn at(mut self, source: SourceId) -> Self {
        self.source = Some(source);
        self
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "configuration {:?}", self.kind)
    }
}
impl std::error::Error for Error {}
/// Result whose errors never format configuration values or external parser messages.
pub type Result<T> = std::result::Result<T, Error>;
