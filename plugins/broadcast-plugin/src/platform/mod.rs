//! OS selection for private storage and the raw retained-output rendezvous.
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_directory;
#[cfg(windows)]
mod windows_security;
#[cfg(unix)]
pub(crate) use unix::{
    ObserverListener, create_new, lock, open_private, private_dir, publish, publish_new,
    sync_directory,
};
#[cfg(windows)]
pub(crate) use windows::{
    ObserverListener, create_new, lock, open_private, private_dir, publish, publish_new,
    sync_directory,
};

#[cfg(test)]
mod tests;
