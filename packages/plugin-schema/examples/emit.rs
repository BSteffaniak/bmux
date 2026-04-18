//! Debugging helper: print the Rust code generated from a BPDL file.
//!
//! Usage: `cargo run -p bmux_plugin_schema --example emit -- <path-to-bpdl>`

fn main() {
    let path = std::env::args().nth(1).expect("usage: emit <path>");
    let src = std::fs::read_to_string(&path).expect("read");
    let schema = bmux_plugin_schema::compile(&src).expect("compile");
    let code = bmux_plugin_schema::codegen_rust::emit(&schema);
    print!("{code}");
}
