//! Directory listings in a fixed order.
//!
//! `std::fs::read_dir` lists entries in whatever order the file system keeps them, which varies
//! between file systems and over time. Where the runtime acts on what it lists (recovering crash
//! records, scanning reservations), that order shows in what it does, so it lists in file name
//! order instead: the same everywhere, and the same in every replay of a simulated run.

use std::{fs::DirEntry, io, path::Path};

/// `path`'s entries, as `std::fs::read_dir` gives them, in file name order (entries that could
/// not be read first).
///
/// # Errors
///
/// * If the directory cannot be read
pub(crate) fn read_dir_sorted(
    path: impl AsRef<Path>,
) -> io::Result<std::vec::IntoIter<io::Result<DirEntry>>> {
    let mut entries: Vec<io::Result<DirEntry>> = std::fs::read_dir(path)?.collect();
    entries.sort_by(|a, b| {
        let name = |entry: &io::Result<DirEntry>| entry.as_ref().ok().map(DirEntry::file_name);
        name(a).cmp(&name(b))
    });
    Ok(entries.into_iter())
}
