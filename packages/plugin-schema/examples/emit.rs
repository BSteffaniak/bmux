//! Debugging helper: print what the compiler generates from a BPDL file.
//!
//! Usage: `cargo run -p bmux_plugin_schema --example emit -- [--rust|--wit] <path-to-bpdl>`
//!
//! `--rust` (default) prints the Rust bindings; `--wit` prints the WIT
//! lowering of the canonical IR. Imports are not resolved, so WIT output
//! only succeeds for schemas without cross-package type references.

fn main() {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let wit = args.first().is_some_and(|arg| arg == "--wit");
    if args
        .first()
        .is_some_and(|arg| arg == "--wit" || arg == "--rust")
    {
        args.remove(0);
    }
    let path = args.first().expect("usage: emit [--rust|--wit] <path>");
    let src = std::fs::read_to_string(path).expect("read");
    let schema = bmux_plugin_schema::compile(&src).expect("compile");
    if wit {
        let package = bmux_plugin_schema::ir::lower(&schema);
        let set = bmux_plugin_schema::ir::PackageSet::new();
        print!(
            "{}",
            bmux_plugin_schema::wit::emit(&set, &package).expect("lower to WIT")
        );
    } else {
        print!("{}", bmux_plugin_schema::codegen_rust::emit(&schema));
    }
}
