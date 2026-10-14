//! Build the portable guest, never a recursive build of the host workspace.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(feature = "wasm")]
mod wasm_config;

const TARGET: &str = "wasm32-unknown-unknown";
const ARTIFACT: &str = "bmux_mermaid_guest.wasm";

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_WASM");
    if env::var_os("CARGO_FEATURE_WASM").is_none() {
        return;
    }
    if let Err(error) = build() {
        panic!("could not build embedded Mermaid guest: {error}");
    }
}

fn build() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing manifest dir")?);
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    let guest = manifest_dir.join("guest");
    watch_tree(&guest)?;
    watch_tree(&manifest_dir.join("wasm_config.rs"))?;
    let destination_target = env::var("TARGET")?;
    println!("cargo:rustc-env=BMUX_MERMAID_WASM_TARGET={destination_target}");
    for name in ["CARGO", "RUSTC", "CARGO_HOME", "CARGO_NET_OFFLINE"] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let cargo = env::var_os("CARGO").ok_or("Cargo did not supply its executable")?;
    let mut command = Command::new(cargo);
    command.current_dir(&guest).args([
        "build",
        "--manifest-path",
        "Cargo.toml",
        "--target",
        TARGET,
        "--release",
        "--locked",
        "--jobs",
        "1",
    ]);
    // This is an explicit build-artifact isolation exception: nested Cargo must
    // never wait for the outer workspace target lock or compile the host again.
    // OUT_DIR is already compiler/profile/package isolated by the outer Cargo.
    // Keep its selected RUSTC (including the Nix compiler), not a rustup override.
    command.env("CARGO_TARGET_DIR", out_dir.join("guest-target"));
    // Host flags (native CPU, sanitizers, linker, clippy, wrappers, etc.) are not
    // guest flags. An explicit empty encoded value also overrides Cargo config.
    command.env("CARGO_ENCODED_RUSTFLAGS", "");
    for (name, _) in env::vars_os() {
        let key = name.to_string_lossy();
        if key.starts_with("CARGO_PROFILE_")
            || (key.starts_with("CARGO_TARGET_") && key != "CARGO_TARGET_DIR")
            || matches!(
                key.as_ref(),
                "RUSTFLAGS"
                    | "RUSTC_WRAPPER"
                    | "RUSTC_WORKSPACE_WRAPPER"
                    | "CARGO_BUILD_RUSTFLAGS"
                    | "CARGO_BUILD_TARGET"
                    | "CARGO_BUILD_RUSTC_WRAPPER"
                    | "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"
                    | "CARGO_MAKEFLAGS"
                    | "MAKEFLAGS"
                    | "MFLAGS"
            )
        {
            command.env_remove(name);
        }
    }
    // Capture stdout: nested Cargo output must not become outer build directives.
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "guest Cargo failed ({}). Ensure the selected Rust compiler has the {TARGET} standard library installed.\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
        .into());
    }
    let source = out_dir
        .join("guest-target")
        .join(TARGET)
        .join("release")
        .join(ARTIFACT);
    let wasm = fs::read(source)?;
    if !wasm.starts_with(b"\0asm\x01\0\0\0") {
        return Err("guest build did not produce a WebAssembly v1 module".into());
    }
    let destination = out_dir.join("mermaid.wasm");
    // Cargo serializes this package's build script for a given OUT_DIR. Other
    // builds have their own OUT_DIR; no shared temp filename or global cache.
    if fs::read(&destination).ok().as_deref() != Some(wasm.as_slice()) {
        fs::write(destination, &wasm)?;
    }
    precompile(&wasm, &destination_target, &out_dir)?;
    Ok(())
}

#[cfg(feature = "wasm")]
fn precompile(wasm: &[u8], target: &str, out_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let destination = target
        .parse()
        .map_err(|error| format!("invalid Mermaid target {target}: {error}"))?;
    cranelift_codegen::isa::lookup(destination)
        .map_err(|error| format!("Mermaid destination compiler {target}: {error}"))?;
    for (fuel, filename) in [(false, "mermaid.cwasm"), (true, "mermaid-fuel.cwasm")] {
        let config = wasm_config::config(target, fuel)
            .map_err(|error| format!("Mermaid AOT target {target}: {error}"))?;
        let engine = wasmtime::Engine::new(&config)
            .map_err(|error| format!("Mermaid AOT engine for {target}: {error}"))?;
        let artifact = engine
            .precompile_module(wasm)
            .map_err(|error| format!("Mermaid AOT compilation for {target}: {error}"))?;
        fs::write(out_dir.join(filename), artifact)?;
    }
    Ok(())
}

#[cfg(not(feature = "wasm"))]
fn precompile(
    _wasm: &[u8],
    _target: &str,
    _out_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("WASM build requested without the WASM build dependency".into())
}

fn watch_tree(path: &Path) -> io::Result<()> {
    // Watching directories as well as files catches additions and removals.
    println!("cargo:rerun-if-changed={}", path.display());
    if path.is_dir() {
        let mut entries = fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort();
        for entry in entries {
            watch_tree(&entry)?;
        }
    }
    Ok(())
}
