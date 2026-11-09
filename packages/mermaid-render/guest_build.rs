//! Materialize the standalone guest from inputs included in the published crate.

use std::fs;
use std::io;
use std::path::Path;

/// Read either a supplied or locally built module. Full validation happens in AOT compilation.
pub fn read_wasm(source: &Path) -> io::Result<Vec<u8>> {
    let wasm = fs::read(source).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not read Mermaid guest {}: {error}", source.display()),
        )
    })?;
    if !wasm.starts_with(b"\0asm\x01\0\0\0") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Mermaid guest is not a WebAssembly v1 module",
        ));
    }
    Ok(wasm)
}

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
