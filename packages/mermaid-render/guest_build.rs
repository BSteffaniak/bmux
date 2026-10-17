//! Materialize the standalone guest from inputs included in the published crate.

use std::fs;
use std::io;
use std::path::Path;

pub fn prepare(inputs: &Path, destination: &Path, font: &[u8]) -> io::Result<()> {
    fs::create_dir_all(destination.join("src"))?;
    // Cargo excludes nested packages from archives, even with an include list.
    // Templates keep the guest inputs packageable without a nested Cargo.toml.
    for (source, target) in [
        ("Cargo.toml.in", "Cargo.toml"),
        ("Cargo.lock.in", "Cargo.lock"),
        ("src/lib.rs", "src/lib.rs"),
    ] {
        fs::copy(inputs.join(source), destination.join(target))?;
    }
    fs::write(destination.join("BmuxDiagram-Regular.ttf"), font)?;
    Ok(())
}
