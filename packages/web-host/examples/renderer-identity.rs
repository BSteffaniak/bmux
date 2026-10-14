//! Release tooling uses the same input selection and framing as build.rs/runtime.
#[path = "../src/provenance.rs"]
mod provenance;

fn main() -> std::io::Result<()> {
    let root = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .ok_or_else(|| std::io::Error::other("usage: renderer-identity <source-root>"))?;
    println!(
        "{}",
        provenance::identity(&root, &provenance::inputs(&root)?)?
    );
    Ok(())
}
