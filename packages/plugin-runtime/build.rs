//! Record the compiler identity that the plug-rt build fingerprint covers.
//!
//! Two images may pass Rust `Waker`s to each other directly only when they were
//! built by the same compiler for the same target with unwinding panics. The
//! fingerprint is derived from exactly those inputs; anything else must use the
//! `extern "C"` waker shim.
use std::process::Command;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(bmux_plugin_dylib)");
    println!("cargo::rerun-if-env-changed=RUSTC");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = Command::new(&rustc)
        .arg("-vV")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_default();
    // Release and commit identify the compiler; host/LLVM lines are covered by
    // the target triple and the compiler identity respectively.
    let identity: Vec<&str> = version
        .lines()
        .filter(|line| line.starts_with("release:") || line.starts_with("commit-hash:"))
        .collect();
    let identity = if identity.is_empty() {
        // An unidentifiable compiler can never claim layout compatibility.
        format!("unknown-rustc-{}", std::process::id())
    } else {
        identity.join(";")
    };
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo::rustc-env=BMUX_PLUG_RT_COMPILER={identity};target={target}");
}
