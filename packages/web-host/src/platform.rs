//! OS-owned file identity, cache permissions, locking and setup commands.
#[cfg(unix)]
#[path = "platform/unix.rs"]
mod native;
#[cfg(windows)]
#[path = "platform/windows.rs"]
mod native;
pub(crate) use native::*;
