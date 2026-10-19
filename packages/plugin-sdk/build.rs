fn main() {
    let source = "bpdl/cli-bridge.bpdl";
    println!("cargo:rerun-if-changed={source}");
    let source = std::fs::read_to_string(source).expect("read argv bridge BPDL");
    let schema = bmux_plugin_schema::compile(&source).expect("validate argv bridge BPDL");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(
        output.join("cli_bridge.rs"),
        bmux_plugin_schema::codegen_rust::emit(&schema),
    )
    .expect("write generated argv bridge");
}
