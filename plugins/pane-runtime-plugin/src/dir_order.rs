//! Directory listings in a fixed order.
//!
//! A directory lists its entries in whatever order the file system keeps them, which varies
//! between file systems and over time. Where the runtime acts on what it lists (recovering crash
//! records, scanning reservations), that order shows in what it does, so it lists in file name
//! order instead: the same everywhere, and the same in every replay of a simulated run.

use std::{io, path::Path};

/// One entry of a listing, from the file system `switchy_fs` runs on (the host's, or the
/// simulated run's).
#[cfg(not(feature = "simulator"))]
pub(crate) type DirEntry = std::fs::DirEntry;
/// One entry of a listing, from the file system `switchy_fs` runs on (the host's, or the
/// simulated run's).
#[cfg(feature = "simulator")]
pub(crate) type DirEntry = switchy_fs::simulator::sync::DirEntry;

/// `path`'s entries in file name order.
///
/// # Errors
///
/// * If the directory, or one of its entries, cannot be read
pub(crate) fn read_dir_sorted(
    path: impl AsRef<Path>,
) -> io::Result<std::vec::IntoIter<io::Result<DirEntry>>> {
    let entries: Vec<io::Result<DirEntry>> = switchy_fs::sync::read_dir_sorted(path)?
        .into_iter()
        .map(Ok)
        .collect();
    Ok(entries.into_iter())
}
