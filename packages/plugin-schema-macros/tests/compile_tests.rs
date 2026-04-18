//! trybuild harness for the `schema!` proc macro.
//!
//! Compile-pass and compile-fail fixtures live under `tests/ui/`. Each
//! `.rs` fixture is a complete Rust file that invokes `schema!` with a
//! `.bpdl` sibling. Fail fixtures pair with a `.stderr` golden.
//!
//! Regenerate stderr goldens with `TRYBUILD=overwrite cargo test
//! -p bmux_plugin_schema_macros --test compile_tests`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass/*.rs");
    t.compile_fail("tests/ui/fail/*.rs");
}
